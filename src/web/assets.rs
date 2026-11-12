use crate::web::state::AppState;
use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
pub struct Assets;

/// `/assets/<path>?v=<hash>`, so an immutable cache never serves stale bytes.
pub fn url(path: &str) -> String {
    match Assets::get(path) {
        Some(f) => format!(
            "/assets/{path}?v={}",
            hex::encode(&f.metadata.sha256_hash()[..6])
        ),
        None => format!("/assets/{path}"),
    }
}

fn content_type(path: &str) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Browsers need the charset on text types or non-ASCII renders as mojibake.
    if mime.type_() == "text" {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    }
}

/// `build.rs` writes the browser packages here; `web::extension` serves them behind auth with `no-store`.
const PRIVATE: [&str; 1] = ["extension/"];

async fn serve(Path(path): Path<String>) -> Response {
    // rust-embed would find nothing for a traversal anyway; refused explicitly.
    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if PRIVATE.iter().any(|p| path.starts_with(p)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get(&path) {
        Some(file) => (
            [
                (header::CONTENT_TYPE, content_type(&path)),
                (header::CACHE_CONTROL, cache_control(&path).to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A year for assets referenced by name; the manifest is held by the installed app, so it gets an hour.
fn cache_control(path: &str) -> &'static str {
    if path.ends_with(".webmanifest") {
        "public, max-age=3600"
    } else {
        "public, max-age=31536000, immutable"
    }
}

/// From the root: a worker under `/assets` could not control `/ui`, and it must not carry the year-long `max-age`.
async fn service_worker() -> Response {
    match Assets::get("sw.js") {
        Some(file) => (
            [
                (
                    header::CONTENT_TYPE,
                    "text/javascript; charset=utf-8".to_string(),
                ),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(askama::Template)]
#[template(path = "offline.html")]
struct OfflineTemplate;

async fn offline() -> Response {
    use askama::Template;
    match OfflineTemplate.render() {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/assets/{*path}", get(serve))
        .route("/sw.js", get(service_worker))
        .route("/offline", get(offline))
}

pub fn assets_router() -> Router<AppState> {
    routes()
}

/// Same routes with no state, for tests.
#[cfg(test)]
pub fn assets_router_standalone() -> Router {
    routes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn app() -> axum::Router {
        assets_router_standalone()
    }

    #[tokio::test]
    async fn serves_the_stylesheet_with_the_right_content_type() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/app.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "text/css; charset=utf-8");
    }

    #[tokio::test]
    async fn assets_are_public_and_cacheable() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/vendor/htmx.min.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()["cache-control"]
                .to_str()
                .unwrap()
                .contains("max-age")
        );
    }

    #[tokio::test]
    async fn the_extension_packages_are_not_reachable_through_assets() {
        for path in [
            "/assets/extension/chrome.zip",
            "/assets/extension/firefox.xpi",
            "/assets/extension/firefox.signed",
        ] {
            let res = app()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path} was served");
        }
    }

    #[tokio::test]
    async fn the_tokenizer_is_not_reachable_through_assets() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/tokenizer.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(
            Assets::get("tokenizer.json").is_none(),
            "the tokenizer is embedded in the asset table a second time"
        );
    }

    #[tokio::test]
    async fn the_manifest_is_served_as_a_manifest() {
        // A manifest sent as text/plain is ignored and the install prompt never appears.
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/manifest.webmanifest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "application/manifest+json");
        assert_eq!(res.headers()["cache-control"], "public, max-age=3600");
    }

    #[test]
    fn the_manifest_declares_no_share_target() {
        let m = Assets::get("manifest.webmanifest").expect("manifest");
        let v: serde_json::Value = serde_json::from_slice(&m.data).expect("valid json");
        assert!(v.get("share_target").is_none(), "share_target is back");
        assert_eq!(v["start_url"], "/ui");
    }

    #[tokio::test]
    async fn the_service_worker_is_served_from_the_root_and_is_not_cached() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/sw.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/javascript")
        );
        assert_eq!(res.headers()["cache-control"], "no-cache");
    }

    #[tokio::test]
    async fn every_asset_url_is_content_hashed_and_immutable() {
        let u = url("js/theme.js");
        let (path, v) = u.split_once("?v=").unwrap();
        assert_eq!(path, "/assets/js/theme.js");
        assert_eq!(v.len(), 12);
        for uri in [u.clone(), format!("{path}?v=000000000000")] {
            let res = app()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert!(
                res.headers()["cache-control"]
                    .to_str()
                    .unwrap()
                    .contains("immutable")
            );
        }
        let hash = |p: &str| url(p).split_once("?v=").unwrap().1.to_string();
        assert_ne!(hash("js/theme.js"), hash("js/prefs.js"));
    }

    #[tokio::test]
    async fn a_missing_asset_is_404_not_a_panic() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/nope.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn path_traversal_is_refused() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/..%2f..%2fetc%2fpasswd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::OK);
    }
}
