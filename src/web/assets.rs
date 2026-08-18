use crate::web::state::AppState;
use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
pub struct Assets;

/// What `/assets/app.js?v=` carries, so a year-long cache stays safe.
///
/// Computed in `build.rs` from the bytes of the files themselves; see
/// `stamp_assets` there for why the URL has to move at all. Read by
/// `layout.html`, which is the one place the pages name these two files.
pub fn stamp() -> &'static str {
    env!("ASSET_STAMP")
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

/// Embedded, but not public.
///
/// `build.rs` writes the browser packages under `assets/extension/`, which is
/// the directory `rust-embed` takes wholesale. They are served by
/// `web::extension` instead, behind the same authentication as everything else
/// and with `no-store` — and neither of those means anything while the same
/// bytes are reachable here anonymously with a year-long `max-age`.
const PRIVATE: [&str; 1] = ["extension/"];

async fn serve(Path(path): Path<String>) -> Response {
    // rust-embed matches on exact stored paths, so a traversal attempt simply
    // finds nothing. Reject it explicitly anyway rather than relying on that.
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

/// How long a browser may keep an asset.
///
/// A year for everything the page references by name, because those are
/// rebuilt with the binary. The manifest is the exception, for the same reason
/// the service worker is: it is read once at install time and then held by the
/// installed app, so a year-long copy of a stale `start_url` or a stale icon
/// set outlives several deployments.
fn cache_control(path: &str) -> &'static str {
    if path.ends_with(".webmanifest") {
        "public, max-age=3600"
    } else {
        "public, max-age=31536000"
    }
}

/// The service worker, from the root rather than from `/assets`.
///
/// Two reasons it cannot be an ordinary asset. A worker may only control paths
/// under the directory it was served from, so one under `/assets` could not see
/// a navigation to `/ui/search`. And it must not carry the year-long `max-age`
/// the other assets do: an update has to be able to reach a phone that already
/// installed the app.
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

fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/assets/{*path}", get(serve))
        .route("/sw.js", get(service_worker))
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
        // Static assets carry no data, so they need no auth, and a long
        // max-age keeps the UI snappy.
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/assets/htmx.min.js")
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
        // `build.rs` writes them under `assets/`, so they are embedded here
        // whether or not they belong here. `web::extension` serves them behind
        // authentication and with `no-store`; both are undone if the same
        // bytes answer anonymously from this route with a year-long max-age.
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
    async fn the_manifest_is_served_as_a_manifest() {
        // A manifest sent as text/plain is ignored, and the install prompt
        // never appears.
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
        // Not the year the other assets get: an installed app re-reads this,
        // and a stale copy would outlive several deployments.
        assert_eq!(res.headers()["cache-control"], "public, max-age=3600");
    }

    #[tokio::test]
    async fn the_service_worker_is_served_from_the_root_and_is_not_cached() {
        // Under /assets it could not control /ui, and with the year-long
        // max-age the other assets carry, an update could never reach a phone
        // that already installed the app.
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
