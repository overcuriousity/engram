//! Serving the browser extension from the deployment it pairs with.
//!
//! The extension ships inside the binary: `assets/` is already embedded with
//! `rust-embed`, and the Chrome package is built into it by `build.rs`. A
//! deployment therefore always serves the build that matches it, and there is
//! no separate artifact to publish or forget.

use crate::auth::Identity;
use crate::web::assets::Assets;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::pair::request_origin;
use crate::web::state::{AppState, judge_pending};
use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(Template)]
#[template(path = "extension.html")]
struct InstallTemplate {
    theme: String,
    judge_pending: Option<i64>,
    origin: String,
    /// False in a checkout that has not been through a release signing. The
    /// page then says so rather than offering a link that 404s.
    have_xpi: bool,
}

/// The download page. Authenticated like everything else, and it carries this
/// deployment's origin into the pairing link — the static, signed manifest
/// cannot know it, so the page is where it is learned.
async fn install_page(State(st): State<AppState>, _id: Identity, headers: HeaderMap) -> Response {
    HtmlTemplate(InstallTemplate {
        theme: "light".into(),
        judge_pending: judge_pending(&st).await,
        origin: request_origin(&headers).unwrap_or_default(),
        have_xpi: Assets::get("extension/firefox.xpi").is_some(),
    })
    .into_response()
}

fn embedded(path: &str, mime: &str, filename: &str) -> Response {
    match Assets::get(path) {
        Some(f) => (
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
                // Rebuilt with the binary, so a cached copy would be a copy of
                // the wrong build.
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            f.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not built into this binary").into_response(),
    }
}

async fn chrome_zip(_id: Identity) -> Response {
    embedded(
        "extension/chrome.zip",
        "application/zip",
        "engram-chrome.zip",
    )
}

/// Served with the type Firefox installs from, so the link is one click.
async fn firefox_xpi(_id: Identity) -> Response {
    embedded(
        "extension/firefox.xpi",
        "application/x-xpinstall",
        "engram.xpi",
    )
}

pub fn extension_router() -> Router<AppState> {
    Router::new()
        .route("/extension/install", get(install_page))
        .route("/extension/chrome.zip", get(chrome_zip))
        .route("/extension/firefox.xpi", get(firefox_xpi))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_extension_downloads_need_authentication() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        for path in [
            "/extension/chrome.zip",
            "/extension/firefox.xpi",
            "/extension/install",
        ] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                res.status(),
                StatusCode::OK,
                "{path} served unauthenticated"
            );
        }
    }

    #[tokio::test]
    async fn the_chrome_package_is_built_into_the_binary() {
        // `build.rs` zips it at compile time, so a deployment always serves the
        // package that matches it and there is no separate artifact to publish
        // or forget.
        let zip = crate::web::assets::Assets::get("extension/chrome.zip")
            .expect("chrome.zip must be embedded");
        assert!(zip.data.len() > 512);
        assert_eq!(&zip.data[..2], b"PK");
    }

    #[tokio::test]
    async fn the_install_page_carries_its_own_origin() {
        let (app, token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/extension/install")
                    .header("host", "engram.example")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("http://engram.example"), "got: {html}");
    }
}
