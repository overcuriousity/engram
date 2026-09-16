//! Pairing the app in one scan.
//!
//! A page behind a session draws a QR code; the app scans it and is paired.
//! What the picture carries is a grant, not a token — `auth::grants` says
//! why — and the app trades it for a token through the claim route below.

use crate::error::Error;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::pair::{request_origin, urlencode};
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// The URI the QR carries. `o` is where the phone will point, `c` the grant,
/// `v` this server's version so the app can say *older than I expect*
/// rather than fail strangely, and `f` the operator-named certificate
/// fingerprint, when there is one.
pub fn pair_uri(origin: &str, code: &str, fingerprint: Option<&str>) -> String {
    let mut uri = format!(
        "engram://pair?o={}&c={}&v={}",
        urlencode(origin),
        urlencode(code),
        urlencode(env!("CARGO_PKG_VERSION")),
    );
    if let Some(f) = fingerprint {
        uri.push_str("&f=");
        uri.push_str(&urlencode(f));
    }
    uri
}

/// The QR as inline SVG. Error-correction level M: the code is under 200
/// characters and a laptop screen is a clean surface, so H would only make
/// the modules smaller for nothing.
fn qr_svg(uri: &str) -> Result<String, Error> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)
        .map_err(|e| Error::Internal(format!("qr: {e}")))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .build())
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppTemplate {
    origin: String,
    /// The URI and its picture, present only on the render that follows the
    /// press. `None` on a GET, for the reason `extension.rs` gives: a
    /// credential that appears whenever a page is opened is one nobody
    /// remembers asking for.
    code: Option<(String, String)>,
}

impl AppTemplate {
    /// Reached from Settings, and its token is revoked there.
    fn section(&self) -> &'static str {
        "settings"
    }
}

async fn app_page(_tenant: Tenant, headers: HeaderMap) -> Response {
    HtmlTemplate(AppTemplate {
        origin: request_origin(&headers).unwrap_or_default(),
        code: None,
    })
    .into_response()
}

async fn app_grant(
    tenant: Tenant,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> UiResult<Response> {
    let origin = request_origin(&headers).unwrap_or_default();
    let code = crate::auth::grants::mint(&tenant.core.store.control, &tenant.user.subject).await?;
    // Validated at load, so `Err` here is unreachable; `ok().flatten()`
    // rather than an unwrap because a page must not panic over config.
    let fingerprint = st.config.server.fingerprint().ok().flatten();
    let uri = pair_uri(&origin, &code, fingerprint.as_deref());
    let svg = qr_svg(&uri)?;
    Ok(HtmlTemplate(AppTemplate {
        origin,
        code: Some((uri, svg)),
    })
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct Claim {
    #[serde(default)]
    pub code: String,
    /// How the app names itself — it is the token's name in Settings, and
    /// the only thing telling two phones apart there.
    #[serde(default)]
    pub device: String,
}

/// Spend a scanned code for a token. No bearer: the code is the credential.
///
/// The device name is checked before the grant is touched, so a malformed
/// request does not burn a code the person then has to press for again.
async fn claim(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(c): axum::Json<Claim>,
) -> crate::error::Result<(axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    let device = c.device.trim();
    if device.is_empty() {
        return Err(Error::Validation("device: empty".into()));
    }
    let token = crate::auth::grants::claim(
        st.tenants.control(),
        c.code.trim(),
        device,
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "token": token,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    ))
}

/// The API side, mounted under `/api/v1`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/pair/claim", post(claim))
}

pub fn app_router() -> Router<AppState> {
    Router::new()
        .route("/ui/app", get(app_page))
        .route("/ui/app/grant", post(app_grant))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn with_cookie(method: &str, uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(method)
            .header("cookie", cookie)
            .header("host", "engram.test")
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn the_uri_carries_origin_code_and_version_and_the_fingerprint_only_when_set() {
        let plain = pair_uri("https://engram.test", "abc", None);
        assert_eq!(
            plain,
            format!(
                "engram://pair?o=https%3A%2F%2Fengram.test&c=abc&v={}",
                env!("CARGO_PKG_VERSION")
            )
        );
        let pinned = pair_uri("https://engram.test", "abc", Some("FPFPFP"));
        assert!(pinned.ends_with("&f=FPFPFP"), "{pinned}");
    }

    #[test]
    fn the_qr_is_an_svg_that_encodes_something() {
        let svg = qr_svg("engram://pair?o=x&c=y&v=0").unwrap();
        assert!(svg.starts_with("<svg") || svg.starts_with("<?xml"), "{svg}");
        assert!(svg.contains("<path") || svg.contains("<rect"), "{svg}");
    }

    #[tokio::test]
    async fn the_page_and_the_press_need_a_session() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        for (method, path) in [("GET", "/ui/app"), ("POST", "/ui/app/grant")] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .method(method)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                res.status(),
                StatusCode::OK,
                "{method} {path} served a stranger"
            );
        }
    }

    #[tokio::test]
    async fn opening_the_page_mints_nothing() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let res = app
            .oneshot(with_cookie("GET", "/ui/app", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        assert!(!body.contains("engram://"), "a GET drew a code");
        assert!(
            body.contains("https://engram.test"),
            "the page says where the phone will point"
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pair_grants")
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn pressing_draws_a_code_for_this_origin() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(with_cookie("POST", "/ui/app/grant", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        // Askama escapes the query's `&` as `&#38;` in the text form; the
        // browser shows the URI with plain ampersands.
        assert!(
            body.contains("engram://pair?o=https%3A%2F%2Fengram.test&#38;c="),
            "no URI for this origin in: {body}"
        );
        assert!(body.contains("<svg"), "no picture");
        // The picture is not the credential.
        assert!(!body.contains("engram_"), "a token was drawn on the page");
    }

    fn claim_req(body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri("/api/v1/pair/claim")
            .method("POST")
            .header("content-type", "application/json")
            .header("user-agent", "engram-android/0.1 (Pixel 8)")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Press the button and read the code out of the page.
    async fn press_and_read_code(app: &axum::Router, cookie: &str) -> String {
        let res = app
            .clone()
            .oneshot(with_cookie("POST", "/ui/app/grant", cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        body.split("&#38;c=")
            .nth(1)
            .map(|rest| rest.split(['&', '<', '"', ' ']).next().unwrap().to_string())
            .expect("a code on the page")
    }

    #[tokio::test]
    async fn a_scanned_code_becomes_a_token_named_for_the_device() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let code = press_and_read_code(&app, &cookie).await;

        let res = app
            .clone()
            .oneshot(claim_req(serde_json::json!({
                "code": code, "device": "engram for Android · Pixel 8"
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body = crate::web::test_support::json_of(res).await;
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        let token = body["token"].as_str().expect("a token").to_string();
        assert!(token.starts_with("engram_"));

        // The token opens the door the app will post to.
        let res = app
            .clone()
            .oneshot(crate::web::api::tests::raw_post(
                "/api/v1/capture",
                &token,
                "text/plain",
                b"shared from the app",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        // And it is listed under the device's name, with its user agent.
        let listed = core.store.control.list_tokens("user-1").await.unwrap();
        let row = listed
            .iter()
            .find(|t| t.name == "engram for Android · Pixel 8")
            .expect("the app's token in the list");
        assert_eq!(
            row.user_agent.as_deref(),
            Some("engram-android/0.1 (Pixel 8)")
        );
    }

    #[tokio::test]
    async fn a_code_claims_once() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let code = press_and_read_code(&app, &cookie).await;
        let body = serde_json::json!({ "code": code, "device": "phone" });
        let first = app.clone().oneshot(claim_req(body.clone())).await.unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);
        let second = app.oneshot(claim_req(body)).await.unwrap();
        assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_expired_or_invented_code_is_unauthorized() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let code = press_and_read_code(&app, &cookie).await;
        // Age the row directly: the clock is not the test's to move.
        sqlx::query("UPDATE pair_grants SET expires_at = ? WHERE code_hash = ?")
            .bind(crate::store::now() - 1)
            .bind(crate::auth::grants::hash_code(&code))
            .execute(&core.store.control.pool)
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(claim_req(
                serde_json::json!({ "code": code, "device": "phone" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .oneshot(claim_req(
                serde_json::json!({ "code": "invented", "device": "phone" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unnamed_device_is_refused_before_the_code_is_spent() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let code = press_and_read_code(&app, &cookie).await;
        for body in [
            serde_json::json!({ "code": code }),
            serde_json::json!({ "code": code, "device": "   " }),
        ] {
            let res = app.clone().oneshot(claim_req(body)).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
        // The grant survived the malformed requests.
        let res = app
            .oneshot(claim_req(
                serde_json::json!({ "code": code, "device": "phone" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }
}
