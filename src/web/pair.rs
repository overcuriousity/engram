//! Handing the browser extension a token through its auth-flow window, without the operator copying it.

use crate::error::Error;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Form;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// `launchWebAuthFlow` hands these redirects to the extension and loads nothing; any other target is an open redirect with a credential.
const EXTENSION_REDIRECT_HOSTS: [&str; 2] = ["chromiumapp.org", "extensions.allizom.org"];

/// Matched on the parsed host: `https://evil.test/#.chromiumapp.org` ends right and is the wrong host.
pub fn is_extension_redirect(raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw) else {
        return false;
    };
    if u.scheme() != "https" {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    EXTENSION_REDIRECT_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

impl PairTemplate {
    fn redirect_uri_attr(&self) -> String {
        crate::web::markdown::inert(&self.redirect_uri)
    }
}

#[derive(Template)]
#[template(path = "pair.html")]
struct PairTemplate {
    origin: String,
    redirect_uri: String,
    state: String,
    /// `false` renders the way back in rather than the Pair button.
    signed_in: bool,
}

impl PairTemplate {
    fn section(&self) -> &'static str {
        ""
    }
}

#[derive(serde::Deserialize)]
pub struct PairParams {
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub state: String,
}

/// A button, not a mint on load, so opening this URL hands nobody a token.
/// `Option<Identity>`: a 401 inside `launchWebAuthFlow`'s window is a dead end, so signed-out renders the way back in.
async fn pair_page(
    tenant: Option<Tenant>,
    State(st): State<AppState>,
    Query(p): Query<PairParams>,
) -> UiResult<Response> {
    if !is_extension_redirect(&p.redirect_uri) {
        return Err(Error::Validation(
            "that redirect does not belong to a browser extension".into(),
        )
        .into());
    }
    Ok(HtmlTemplate(PairTemplate {
        origin: st.auth.deployment.base(),
        redirect_uri: p.redirect_uri,
        state: p.state,
        signed_in: tenant.is_some(),
    })
    .into_response())
}

async fn pair_submit(
    tenant: Tenant,
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(p): Form<PairParams>,
) -> UiResult<Response> {
    if !is_extension_redirect(&p.redirect_uri) {
        return Err(Error::Validation(
            "that redirect does not belong to a browser extension".into(),
        )
        .into());
    }
    let origin = st.auth.deployment.base();
    // Every extension token has the same name; the user agent tells the rows apart.
    let (_, plaintext) = crate::auth::tokens::mint(
        &tenant.core.store.control,
        "browser extension",
        &tenant.user.subject,
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
    )
    .await?;

    // The fragment, not the query: it never reaches a server log or history; `launchWebAuthFlow` passes it through.
    let location = format!(
        "{}#token={}&state={}&origin={}",
        p.redirect_uri,
        urlencode(&plaintext),
        urlencode(&p.state),
        urlencode(&origin),
    );
    tracing::info!(subject = %tenant.user.subject, "extension paired");
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response())
}

/// Local rather than a dependency: three ASCII values.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn pair_router() -> Router<AppState> {
    Router::new().route("/ui/pair", get(pair_page).post(pair_submit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn only_a_browser_extension_redirect_sink_is_accepted() {
        assert!(is_extension_redirect("https://abcdefg.chromiumapp.org/"));
        assert!(is_extension_redirect(
            "https://abcdefg.extensions.allizom.org/"
        ));
        for bad in [
            "https://evil.test/",
            "http://abcdefg.chromiumapp.org/",    // not https
            "https://chromiumapp.org.evil.test/", // suffix in the wrong place
            "https://evil.test/#.chromiumapp.org",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!is_extension_redirect(bad), "accepted {bad}");
        }
    }

    #[tokio::test]
    async fn pairing_mints_a_working_token_and_hands_it_back_through_the_browser() {
        let (app, token, core) = crate::web::api::tests::app_token_and_core().await;
        let redirect = "https://abcdefg.chromiumapp.org/";
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/pair")
                    .method("POST")
                    .header("host", "engram.example")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "redirect_uri={}&state=nonce123",
                        urlencode(redirect)
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers()["location"].to_str().unwrap().to_string();
        assert!(location.starts_with(redirect), "got {location}");
        // A query string reaches server logs and history in a way a fragment does not.
        let fragment = location.split_once('#').unwrap().1;
        let token = fragment
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .unwrap();
        assert!(fragment.contains("state=nonce123"));
        assert!(fragment.contains("origin=http%3A%2F%2F127.0.0.1%3A8080"));

        let id = crate::auth::tokens::verify(&core.store.control, &percent_decode(token))
            .await
            .unwrap();
        assert_eq!(id.subject, "user-1");
    }

    #[tokio::test]
    async fn pairing_without_a_session_offers_the_way_back_instead_of_a_dead_end() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/pair?redirect_uri=https%3A%2F%2Fabc.chromiumapp.org%2F&state=n1")
                    .header("host", "engram.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Not signed in"), "got {html}");
        assert!(html.contains("value=\"n1\""), "got {html}");
        assert!(!html.contains(">Pair<"), "got {html}");

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/pair")
                    .method("POST")
                    .header("host", "engram.example")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "redirect_uri=https%3A%2F%2Fabc.chromiumapp.org%2F&state=n1",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn pairing_refuses_a_redirect_that_is_not_an_extension() {
        let (app, token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/pair")
                    .method("POST")
                    .header("host", "engram.example")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "redirect_uri=https%3A%2F%2Fevil.test%2F&state=x",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
            {
                out.push(v as char);
                i += 3;
                continue;
            }
            out.push(b[i] as char);
            i += 1;
        }
        out
    }
}
