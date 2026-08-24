//! Handing the browser extension a token, without ever writing one down.
//!
//! Bearer tokens and the UI that mints them already exist. What the extension
//! needs on top of that is a way to receive one that does not involve the
//! operator selecting a credential from a page and pasting it somewhere: the
//! browser's own auth-flow window opens this page, and the redirect carries
//! the token back into the extension that started the flow.

use crate::tenants::Tenant;
use crate::auth::Identity;
use crate::error::{Error, Result};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use askama::Template;
use axum::Form;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// Hosts a browser reserves for extension redirect sinks.
///
/// `browser.identity.launchWebAuthFlow` hands the response back to the
/// extension that started the flow instead of loading these; nothing is served
/// from them. That is what makes a redirect to one safe and a redirect
/// anywhere else an open redirect carrying a credential.
const EXTENSION_REDIRECT_HOSTS: [&str; 2] = ["chromiumapp.org", "extensions.allizom.org"];

/// Whether a redirect target is one of those sinks.
///
/// Matched on the parsed host, never on the raw string:
/// `https://evil.test/#.chromiumapp.org` ends with the right characters and is
/// not the right host.
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

/// The origin this deployment is being reached at.
///
/// Learned from the request rather than configured, because the signed
/// extension is one artifact serving every deployment: an XPI is signed over
/// its contents, so a manifest rewritten per host would invalidate the
/// signature that makes one-click install work. The download and pairing pages
/// carry the origin instead, and the extension asks for host permission for
/// that one origin.
pub fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if host.is_empty() {
        return None;
    }
    // `https` when the proxy did not say, because the proxy not saying is the
    // common case — nginx forwards no `X-Forwarded-Proto` unless told to — and
    // guessing wrong in that direction is the expensive one. This origin is
    // shown to the operator as the address to pair with, and a deployment
    // named `http://` there is a bearer token sent in cleartext. Loopback is
    // the exception, and the one deployment genuinely reached without TLS.
    let scheme = match headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None if is_loopback(host) => "http",
        None => "https",
    };
    Some(format!("{scheme}://{host}"))
}

/// Whether a `Host` names this machine, port and all.
fn is_loopback(host: &str) -> bool {
    // An IPv6 literal is bracketed and full of colons of its own, so the port
    // separator is the one after the bracket rather than the first one.
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => host.split(':').next().unwrap_or(host),
    };
    name == "localhost" || name == "::1" || name.starts_with("127.")
}

#[derive(Template)]
#[template(path = "pair.html")]
struct PairTemplate {
    judge_pending: Option<i64>,
    origin: String,
    redirect_uri: String,
    state: String,
    /// Whether this window has a session. `false` renders the way back in
    /// rather than the Pair button; see `pair_page`.
    signed_in: bool,
}

#[derive(serde::Deserialize)]
pub struct PairParams {
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub state: String,
}

/// The page the extension opens through `launchWebAuthFlow`.
///
/// A button rather than an automatic mint. The operator sees which origin they
/// are pairing with and presses something; a flow that minted on page load
/// would hand a token to anything that could get this URL opened.
///
/// `Option<Identity>` rather than `Identity`, because this page is opened
/// inside `launchWebAuthFlow`'s own window and a 401 there is a dead end:
/// `redirect_unauthenticated_browsers` rewrites it into `/auth/login?go=1`,
/// which carries no return path, so signing in lands on `/ui/search` and the
/// flow never reaches its redirect sink — `pair()` rejects with nothing the
/// operator can act on. First run is exactly when there is no session, so that
/// is the case that has to work. Rendered signed-out, the page says what is
/// missing and offers the way back; minting still requires an `Identity`, on
/// the POST.
async fn pair_page(
    State(st): State<AppState>,
    tenant: Option<Tenant>,
    headers: HeaderMap,
    Query(p): Query<PairParams>,
) -> Result<Response> {
    if !is_extension_redirect(&p.redirect_uri) {
        return Err(Error::Validation(
            "that redirect does not belong to a browser extension".into(),
        ));
    }
    Ok(HtmlTemplate(PairTemplate {
        judge_pending: match &tenant {
            Some(t) => crate::web::state::judge_pending(t).await,
            None => None,
        },
        origin: request_origin(&headers).unwrap_or_default(),
        redirect_uri: p.redirect_uri,
        state: p.state,
        signed_in: tenant.is_some(),
    })
    .into_response())
}

async fn pair_submit(
    State(st): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    Form(p): Form<PairParams>,
) -> Result<Response> {
    if !is_extension_redirect(&p.redirect_uri) {
        return Err(Error::Validation(
            "that redirect does not belong to a browser extension".into(),
        ));
    }
    let origin = request_origin(&headers).unwrap_or_default();
    // The browser that asked, recorded with the token: every extension token
    // carries the same name, so this is the only thing telling one row from
    // another on the settings page.
    let (_, plaintext) = crate::auth::tokens::mint(
        &tenant.core.store.control,
        "browser extension",
        &tenant.user.subject,
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
    )
    .await?;

    // The fragment, not the query: a fragment is never sent to a server and
    // does not land in a proxy log or in browsing history the way a query
    // string does. `launchWebAuthFlow` hands the whole URL to the extension,
    // fragment included.
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

/// Percent-encode everything outside the unreserved set. Small and local
/// rather than a dependency: three values, all of them ASCII.
fn urlencode(s: &str) -> String {
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
    use axum::http::{HeaderValue, Request};
    use tower::ServiceExt;

    #[test]
    fn only_a_browser_extension_redirect_sink_is_accepted() {
        // These two hosts do not serve anything: the browser intercepts them
        // and routes the response back into the extension that started the
        // flow. Anything else is somewhere a token could actually be read.
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

    #[test]
    fn the_pairing_page_carries_its_own_origin() {
        // The extension must learn which origin to request host permission
        // for without the operator typing it. The deployment knows it; the
        // static, signed manifest cannot.
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("engram.example"));
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(
            request_origin(&h).as_deref(),
            Some("https://engram.example")
        );

        // No `X-Forwarded-Proto`, which is what nginx sends unless it has been
        // told otherwise. Guessing `http` there names the deployment by a
        // scheme it does not answer on, and this origin is the address the
        // operator is shown to pair with — a bearer token in cleartext.
        let mut unforwarded = HeaderMap::new();
        unforwarded.insert("host", HeaderValue::from_static("engram.example"));
        assert_eq!(
            request_origin(&unforwarded).as_deref(),
            Some("https://engram.example")
        );

        // Loopback is the exception: the one deployment genuinely reached
        // without TLS, and telling an operator to pair with `https://localhost`
        // would send them somewhere nothing is listening.
        for host in ["localhost:8080", "127.0.0.1:8080", "[::1]:8080"] {
            let mut plain = HeaderMap::new();
            plain.insert("host", HeaderValue::from_str(host).unwrap());
            assert_eq!(
                request_origin(&plain).as_deref(),
                Some(format!("http://{host}").as_str()),
                "{host}"
            );
        }

        // An explicit header still wins in both directions.
        let mut forwarded_plain = HeaderMap::new();
        forwarded_plain.insert("host", HeaderValue::from_static("engram.example"));
        forwarded_plain.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert_eq!(
            request_origin(&forwarded_plain).as_deref(),
            Some("http://engram.example")
        );

        assert_eq!(request_origin(&HeaderMap::new()), None);
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
        // In the fragment, not the query: a query string reaches server logs
        // and the browser's history in a way a fragment does not.
        let fragment = location.split_once('#').unwrap().1;
        let token = fragment
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .unwrap();
        assert!(fragment.contains("state=nonce123"));
        assert!(fragment.contains("origin=https%3A%2F%2Fengram.example"));

        let id = crate::auth::tokens::verify(&core.store.control, &percent_decode(token))
            .await
            .unwrap();
        assert_eq!(id.subject, "user-1");
    }

    #[tokio::test]
    async fn pairing_without_a_session_offers_the_way_back_instead_of_a_dead_end() {
        // This page opens inside `launchWebAuthFlow`'s own window. A 401 there
        // is rewritten into `/auth/login?go=1`, which carries no return path —
        // so signing in lands on the search page, the flow never reaches its
        // redirect sink, and `pair()` rejects with nothing to act on. First run
        // is exactly the case with no session, so it has to render.
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
        assert!(html.contains("not signed in"), "got {html}");
        // The state carries through, so continuing after sign-in resumes this
        // flow rather than starting a new one.
        assert!(html.contains("value=\"n1\""), "got {html}");
        // And no button that would ask for a token there is nobody to mint for.
        assert!(!html.contains(">Pair<"), "got {html}");

        // Minting still needs an identity: the POST is unchanged.
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

    /// The inverse of `urlencode`, for the one value a test has to read back.
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
