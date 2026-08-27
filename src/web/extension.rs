//! Serving the browser extension from the deployment it pairs with.
//!
//! The extension ships inside the binary: `assets/` is already embedded with
//! `rust-embed`, and the Chrome package is built into it by `build.rs`. A
//! deployment therefore always serves the build that matches it, and there is
//! no separate artifact to publish or forget.

use crate::tenants::Tenant;
use crate::web::assets::Assets;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::pair::request_origin;
use crate::web::state::{AppState, judge_pending};
use askama::Template;
use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

#[derive(Template)]
#[template(path = "extension.html")]
struct InstallTemplate {
    judge_pending: Option<i64>,
    origin: String,
    /// Whether the XPI on offer has been through AMO.
    ///
    /// Both arrive under the same name, and the difference decides what the
    /// operator can do with it: a signed package installs in one click and
    /// stays, an unsigned one loads through `about:debugging` and is gone at
    /// the next restart. Saying which is on offer is the difference between an
    /// instruction that works and one that does not.
    xpi_signed: bool,
    /// A credential minted for one phone, present only on the render that
    /// follows the press that asked for it.
    ///
    /// `None` on every ordinary page load, deliberately: minting on a GET
    /// would put a live credential in the token list every time anyone opened
    /// the download page, and a list of thirty tokens nobody remembers asking
    /// for is a list nobody revokes from.
    device_token: Option<String>,
}

/// The download page. Authenticated like everything else, and it carries this
/// deployment's origin into the pairing link — the static, signed manifest
/// cannot know it, so the page is where it is learned.
async fn install_page(tenant: Tenant, headers: HeaderMap) -> Response {
    HtmlTemplate(InstallTemplate {
        judge_pending: judge_pending(&tenant).await,
        origin: request_origin(&headers).unwrap_or_default(),
        xpi_signed: Assets::get("extension/firefox.signed").is_some(),
        device_token: None,
    })
    .into_response()
}

/// Mint a credential for one phone and render the doors that carry it.
///
/// A token living in a bookmark or a Shortcut is the whole difficulty of the
/// phone doors, and the only way it is tolerable is that each is minted for the
/// device that asked, named for what asked, and revocable on its own. The
/// plaintext is shown on this one render and never again — the store keeps only
/// its argon2id hash — so a second phone presses the button again rather than
/// sharing the first one's credential.
async fn phone_token(tenant: Tenant, headers: HeaderMap) -> Response {
    let minted = crate::auth::tokens::mint(
        &tenant.core.store.control,
        "phone",
        &tenant.user.subject,
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await;
    let device_token = match minted {
        Ok((_row, plaintext)) => Some(plaintext),
        Err(e) => {
            tracing::error!(error = %e, "could not mint a phone token");
            return e.into_response();
        }
    };
    HtmlTemplate(InstallTemplate {
        judge_pending: judge_pending(&tenant).await,
        origin: request_origin(&headers).unwrap_or_default(),
        xpi_signed: Assets::get("extension/firefox.signed").is_some(),
        device_token,
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

async fn chrome_zip(_: Tenant) -> Response {
    embedded(
        "extension/chrome.zip",
        "application/zip",
        "engram-chrome.zip",
    )
}

/// Served with the type Firefox installs from, so the link is one click.
async fn firefox_xpi(_: Tenant) -> Response {
    embedded(
        "extension/firefox.xpi",
        "application/x-xpinstall",
        "engram.xpi",
    )
}

pub fn extension_router() -> Router<AppState> {
    Router::new()
        .route("/extension/install", get(install_page))
        .route("/extension/phone", post(phone_token))
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

    /// The install page, and the render that follows the mint press.
    async fn press_the_phone_button(app: &axum::Router, cookie: &str) -> String {
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/extension/phone")
                    .method("POST")
                    .header("cookie", cookie)
                    .header("host", "engram.test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        crate::web::test_support::body_of(res).await
    }

    #[tokio::test]
    async fn minting_a_phone_token_needs_authentication() {
        // The one route here that creates a credential rather than serving a
        // file, so it is worth asserting on its own rather than trusting that
        // it sits behind the same extractor as its neighbours.
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/extension/phone")
                    .method("POST")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::OK, "minted for a stranger");
    }

    #[tokio::test]
    async fn opening_the_install_page_mints_nothing() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/extension/install")
                    .header("cookie", &cookie)
                    .header("host", "engram.test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        // A credential appears when it is asked for, never from reading a page.
        assert!(
            !body.contains("engram_"),
            "the download page handed out a live token"
        );
        assert!(body.contains("Mint a token for this phone"));
    }

    #[tokio::test]
    async fn the_minted_token_opens_the_door_the_phone_doors_post_to() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let body = press_the_phone_button(&app, &cookie).await;
        assert!(
            body.contains("/api/v1/capture"),
            "the doors post to the capture endpoint"
        );

        let token = body
            .split("Bearer engram_")
            .nth(1)
            .map(|rest| {
                format!(
                    "engram_{}",
                    rest.split(['<', '"', '\'', ' ', '&']).next().unwrap()
                )
            })
            .expect("a minted token on the page");
        let res = app
            .oneshot(crate::web::api::tests::raw_post(
                "/api/v1/capture",
                &token,
                "text/plain",
                b"shared from a phone",
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::CREATED,
            "the token on the page must work"
        );
    }

    #[tokio::test]
    async fn each_press_mints_its_own_device_token() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let first = press_the_phone_button(&app, &cookie).await;
        let second = press_the_phone_button(&app, &cookie).await;
        assert_ne!(
            first, second,
            "two devices must not share one revocable credential"
        );
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
        // `https`, with no `X-Forwarded-Proto` to go on. A deployment on a
        // real host name is behind TLS, and a proxy that forwards without
        // setting the header is the ordinary case rather than the exotic one.
        assert!(html.contains("https://engram.example"), "got: {html}");
    }
}
