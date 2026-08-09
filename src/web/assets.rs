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
                // Assets are rebuilt with the binary, so a long max-age is
                // safe and keeps navigation instant.
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000".to_string(),
                ),
            ],
            file.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub fn assets_router() -> Router<AppState> {
    Router::new().route("/assets/{*path}", get(serve))
}

/// Same routes with no state, for tests.
#[cfg(test)]
pub fn assets_router_standalone() -> Router {
    Router::new().route("/assets/{*path}", get(serve))
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
    fn the_stylesheet_makes_no_external_requests() {
        // A CDN url here would defeat embedding the fonts.
        let css = Assets::get("app.css").unwrap();
        let css = std::str::from_utf8(css.data.as_ref()).unwrap();
        assert!(!css.contains("https://"), "external url in stylesheet");
        assert!(!css.contains("http://"), "external url in stylesheet");
    }
}
