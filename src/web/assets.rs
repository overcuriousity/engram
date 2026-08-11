use crate::web::state::AppState;
use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(rust_embed::Embed)]
#[folder = "assets/"]
pub struct Assets;

fn content_type(path: &str) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Browsers need the charset on text types or non-ASCII renders as mojibake.
    if mime.type_() == "text" {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    }
}

async fn serve(Path(path): Path<String>) -> Response {
    // rust-embed matches on exact stored paths, so a traversal attempt simply
    // finds nothing. Reject it explicitly anyway rather than relying on that.
    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
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

    #[test]
    fn fonts_are_embedded_in_the_binary() {
        // No CDN at runtime: an external font request would leak usage and
        // break offline use.
        for f in [
            "inter-400.woff2",
            "inter-500.woff2",
            "inter-600.woff2",
            "jetbrains-mono-400.woff2",
        ] {
            assert!(
                Assets::get(&format!("fonts/{f}")).is_some(),
                "missing embedded font {f}"
            );
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

    #[test]
    fn the_manifest_declares_what_an_install_needs() {
        let m = Assets::get("manifest.webmanifest").expect("manifest must be embedded");
        let m: serde_json::Value = serde_json::from_slice(m.data.as_ref())
            .expect("the manifest must be valid JSON or the browser drops it");

        assert_eq!(m["name"], "engram");
        assert_eq!(m["display"], "standalone");
        assert_eq!(m["start_url"], "/ui/capture");
        // `id` stays what it always was even though `start_url` moved: a
        // browser keys an installed app on `id`, and changing it turns an
        // update into a second app sitting beside the first.
        assert_eq!(m["id"], "/ui/search");
        assert_eq!(m["scope"], "/", "the worker controls the whole origin");
        // The splash matches the page it opens into. These were the dark
        // palette while the app serves light, so an install flashed a dark
        // screen and then repainted cream.
        assert_eq!(m["background_color"], "#f8f6f1");
        assert_eq!(m["theme_color"], "#f8f6f1");

        let icons = m["icons"].as_array().unwrap();
        // Android will not offer to install without both of these sizes, and
        // will not round the icon without a maskable one.
        for size in ["192x192", "512x512"] {
            assert!(
                icons.iter().any(|i| i["sizes"] == size),
                "no {size} icon in the manifest"
            );
        }
        assert!(
            icons.iter().any(|i| i["purpose"] == "maskable"),
            "no maskable icon in the manifest"
        );
        for i in icons {
            // Manifest paths are URLs; the embed is rooted at `assets/`, so the
            // mount point comes off before the lookup.
            let src = i["src"].as_str().unwrap();
            let embedded = src
                .strip_prefix("/assets/")
                .expect("icons are served from /assets");
            assert!(
                Assets::get(embedded).is_some(),
                "the manifest names {src}, which is not embedded"
            );
        }
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

    #[test]
    fn the_service_worker_handles_fetch_and_caches_no_asset() {
        // The fetch handler is the part a browser checks for before it treats
        // the site as installable. The absence of asset caching is the part
        // that keeps it from serving yesterday's HTML.
        let js = Assets::get("sw.js").expect("sw.js must be embedded");
        let js = std::str::from_utf8(js.data.as_ref()).unwrap();
        assert!(js.contains("addEventListener('fetch'"));
        assert!(!js.contains("https://"), "external url in the worker");
        assert!(
            !js.contains("cache.addAll"),
            "the worker must not precache the app shell"
        );
    }

    #[test]
    fn the_icons_are_embedded_at_the_sizes_the_manifest_promises() {
        for f in [
            "icon.svg",
            "icon-192.png",
            "icon-512.png",
            "apple-touch-icon.png",
        ] {
            assert!(Assets::get(f).is_some(), "missing embedded icon {f}");
        }
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

    /// The manifest paints `#f8f6f1` behind a launch. An offline page that
    /// opens dark is the same flash of the wrong colour that moving the
    /// manifest off the dark palette was meant to remove.
    #[test]
    fn the_offline_page_opens_in_the_colour_the_manifest_paints() {
        let js = Assets::get("sw.js").expect("sw.js must be embedded");
        let js = std::str::from_utf8(js.data.as_ref()).unwrap();
        assert!(
            js.contains("html{background:#f8f6f1"),
            "the offline page does not start from the app's light base colour"
        );
        assert!(
            js.contains("prefers-color-scheme:dark"),
            "a device set to dark gets no dark offline page"
        );
    }

    #[test]
    fn the_stylesheet_carries_both_themes_and_the_ported_palette() {
        let css = Assets::get("app.css").expect("app.css must be embedded");
        let css = std::str::from_utf8(css.data.as_ref()).unwrap();
        assert!(css.contains("#f8f6f1"), "light base colour missing");
        assert!(css.contains("#0e1015"), "dark base colour missing");
        assert!(css.contains("#3b6e91"), "light accent missing");
        assert!(css.contains("#5aa8b0"), "dark accent missing");
        assert!(css.contains("[data-theme=\"dark\"]"));
        assert!(css.contains("--radius-sm: 3px"));
    }

    #[test]
    fn the_script_is_embedded_and_makes_no_external_requests() {
        let js = Assets::get("app.js").expect("app.js must be embedded");
        let js = std::str::from_utf8(js.data.as_ref()).unwrap();
        assert!(!js.contains("https://"), "external url in script");
        assert!(
            js.contains("data-terms"),
            "highlighting reads the terms attribute"
        );
        assert!(
            js.contains("clipboard"),
            "copy buttons need the clipboard API"
        );
    }

    #[test]
    fn the_stylesheet_makes_no_external_requests() {
        // A CDN url here would defeat embedding the fonts.
        let css = Assets::get("app.css").unwrap();
        let css = std::str::from_utf8(css.data.as_ref()).unwrap();
        assert!(!css.contains("https://"), "external url in stylesheet");
        assert!(!css.contains("http://"), "external url in stylesheet");
    }
}
