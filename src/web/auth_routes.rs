use crate::auth::{
    SESSION_COOKIE, SESSION_TTL_SECS, clear_session_cookie, cookie_value, set_session_cookie,
};
use crate::config::AuthMode;
use crate::error::{Error, Result};
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};

/// askama 0.16 no longer ships an axum integration crate, so templates get a
/// thin wrapper of their own.
pub struct HtmlTemplate<T>(pub T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(body) => Html(body).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "template render failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
            }
        }
    }
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    /// Always `None`: the sign-in page has no nav, and a count of what is
    /// waiting inside is not something to tell someone who is still outside.
    judge_pending: Option<i64>,
    oidc: bool,
    error: Option<String>,
    /// The page that asked for this login, already checked by `safe_next`.
    /// Handed back through the form so signing in returns there.
    next: Option<String>,
}

#[derive(serde::Deserialize)]
struct LoginQuery {
    #[serde(default)]
    go: Option<String>,
}

/// Where to land after signing in, when the destination can be trusted.
///
/// The destination is user input on every path it travels — a query parameter
/// the middleware wrote, a hidden form field, a value carried across an OIDC
/// round trip — and a redirect that follows anything it is handed is a
/// phishing hop off this site. Only a page of this UI is accepted, by the same
/// rule `ui::ReturnTo` uses: inside `/ui/`, and never `/ui//host`, which a
/// browser reads as protocol-relative and resolves somewhere else entirely.
///
/// `None` means the caller's default, which is the page the app starts on. The
/// older `go=1` — a flag meaning "begin the login", not a place — lands there
/// too, without a special case.
fn safe_next(raw: Option<&str>) -> Option<String> {
    match raw {
        Some(p) if p.starts_with("/ui/") && !p.starts_with("/ui//") => Some(p.to_string()),
        _ => None,
    }
}

/// Where a login ends when nothing better was asked for.
const AFTER_LOGIN: &str = "/ui/search";

async fn login_page(State(st): State<AppState>, Query(q): Query<LoginQuery>) -> Result<Response> {
    match st.auth.mode {
        AuthMode::Oidc => {
            let client = st
                .auth
                .oidc
                .as_ref()
                .ok_or_else(|| Error::Validation("oidc not configured".into()))?;
            if q.go.is_some() {
                let (url, mut pending) = client.authorize_url()?;
                // Held with the rest of the in-flight attempt: the provider
                // hands back only `state`, so there is nowhere else to keep it
                // across the round trip.
                pending.next = safe_next(q.go.as_deref());
                st.auth.pending.put(pending);
                return Ok(Redirect::to(&url).into_response());
            }
            Ok(HtmlTemplate(LoginTemplate {
                judge_pending: None,
                oidc: true,
                error: None,
                next: None,
            })
            .into_response())
        }
        AuthMode::Local => Ok(HtmlTemplate(LoginTemplate {
            judge_pending: None,
            oidc: false,
            error: None,
            next: safe_next(q.go.as_deref()),
        })
        .into_response()),
    }
}

#[derive(serde::Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    /// The page that asked for the login. Survives a mistyped password: the
    /// form is re-rendered with it, or one typo costs the destination.
    #[serde(default)]
    next: Option<String>,
}

async fn login_submit(State(st): State<AppState>, Form(f): Form<LoginForm>) -> Result<Response> {
    // Only reachable in local mode; in OIDC mode there is no password to post.
    let AuthMode::Local = st.auth.mode else {
        return Err(Error::NotFound);
    };
    let cfg = st.auth.local.as_ref().ok_or(Error::NotFound)?;

    let Some(identity) = crate::auth::local::check_credentials(cfg, &f.username, &f.password)
    else {
        tracing::warn!(username = %f.username, "failed local sign-in");
        return Ok((
            StatusCode::UNAUTHORIZED,
            HtmlTemplate(LoginTemplate {
                judge_pending: None,
                oidc: false,
                error: Some("Incorrect username or password.".into()),
                next: safe_next(f.next.as_deref()),
            }),
        )
            .into_response());
    };

    start_session(&st, &identity, safe_next(f.next.as_deref())).await
}

#[derive(serde::Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn callback(State(st): State<AppState>, Query(q): Query<CallbackQuery>) -> Result<Response> {
    if let Some(e) = q.error {
        tracing::warn!(error = %e, "identity provider returned an error");
        return Err(Error::Unauthorized);
    }
    let (code, state_param) = match (q.code, q.state) {
        (Some(c), Some(s)) => (c, s),
        _ => {
            return Err(Error::Validation(
                "callback is missing code or state".into(),
            ));
        }
    };

    let client = st.auth.oidc.as_ref().ok_or(Error::NotFound)?;
    // Single use: this also defeats a replayed callback URL.
    let pending = st
        .auth
        .pending
        .take(&state_param)
        .ok_or(Error::Unauthorized)?;
    let identity = client.exchange(&pending, &code, &state_param).await?;
    // Re-checked on the way out as well as on the way in: what comes back out
    // of the pending store is only as trustworthy as what went into it.
    let next = safe_next(pending.next.as_deref());
    start_session(&st, &identity, next).await
}

async fn start_session(
    st: &AppState,
    identity: &crate::auth::Identity,
    next: Option<String>,
) -> Result<Response> {
    let sid = crate::store::new_id();
    st.tenants
        .control()
        .insert_session(
            &sid,
            &identity.subject,
            identity.email.as_deref(),
            SESSION_TTL_SECS,
        )
        .await?;
    Ok((
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                set_session_cookie(&sid, st.auth.secure_cookies),
            ),
            (
                header::LOCATION,
                next.unwrap_or_else(|| AFTER_LOGIN.to_string()),
            ),
        ],
    )
        .into_response())
}

async fn logout(State(st): State<AppState>, headers: axum::http::HeaderMap) -> Result<Response> {
    if let Some(h) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
        && let Some(sid) = cookie_value(h, SESSION_COOKIE)
    {
        // Delete the row, not just the cookie: a copied cookie must stop working.
        st.tenants.control().delete_session(&sid).await?;
    }
    Ok((
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, clear_session_cookie()),
            (header::LOCATION, "/auth/login".to_string()),
        ],
    )
        .into_response())
}

pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(login_page).post(login_submit))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn local_app() -> axum::Router {
        crate::web::test_support::router(
            crate::core::test_support::test_core().await,
            Some(crate::config::LocalConfig {
                username: "dev".into(),
                password_hash: crate::auth::local::hash_password("hunter2").unwrap(),
            }),
        )
        .await
    }

    fn form(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("POST")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn the_login_page_is_reachable_without_credentials() {
        let res = local_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/auth/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn correct_local_credentials_set_a_session_cookie() {
        let res = local_app()
            .await
            .oneshot(form("/auth/login", "username=dev&password=hunter2"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie = res.headers()["set-cookie"].to_str().unwrap();
        assert!(cookie.contains("engram_session="));
        assert!(cookie.contains("HttpOnly"));
        assert_eq!(res.headers()["location"], "/ui/search");
    }

    #[tokio::test]
    async fn signing_in_lands_on_the_page_that_asked_for_the_login() {
        // A bookmarked artifact with an expired session is the whole case:
        // bouncing through the login and then landing on Search loses the page
        // that was actually wanted.
        let res = local_app()
            .await
            .oneshot(form(
                "/auth/login",
                "username=dev&password=hunter2&next=%2Fui%2Fcorpora%2Fabc",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/corpora/abc");
    }

    #[tokio::test]
    async fn a_login_will_not_be_talked_into_leaving_the_site() {
        // The destination arrives as user input on every path it travels. A
        // redirect that follows anything it is handed is a phishing hop.
        for hostile in [
            "next=https%3A%2F%2Fevil.example%2F",
            "next=%2F%2Fevil.example%2F",
            "next=%2Fauth%2Flogout",
            "next=%2Fui%2F%2Fevil.example",
        ] {
            let res = local_app()
                .await
                .oneshot(form(
                    "/auth/login",
                    &format!("username=dev&password=hunter2&{hostile}"),
                ))
                .await
                .unwrap();
            assert_eq!(res.headers()["location"], "/ui/search", "{hostile}");
        }
    }

    #[tokio::test]
    async fn the_local_form_carries_the_destination_through_the_login() {
        let res = local_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/auth/login?go=%2Fui%2Fops")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = crate::web::test_support::body_of(res).await;
        assert!(
            body.contains(r#"name="next" value="/ui/ops""#),
            "the form must hand the destination back: {body}"
        );
    }

    #[tokio::test]
    async fn wrong_credentials_do_not_set_a_cookie() {
        let res = local_app()
            .await
            .oneshot(form("/auth/login", "username=dev&password=wrong"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get("set-cookie").is_none());
    }

    #[tokio::test]
    async fn a_session_cookie_authenticates_api_requests() {
        let app = local_app().await;
        let login = app
            .clone()
            .oneshot(form("/auth/login", "username=dev&password=hunter2"))
            .await
            .unwrap();
        let cookie = login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn logout_clears_the_cookie_and_kills_the_session() {
        let app = local_app().await;
        let login = app
            .clone()
            .oneshot(form("/auth/login", "username=dev&password=hunter2"))
            .await
            .unwrap();
        let cookie = login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let out = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/logout")
                    .method("POST")
                    .header("cookie", cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            out.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );

        // The server-side row is gone, so the old cookie is worthless.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_local_login_form_is_refused_in_oidc_mode() {
        let core = crate::core::test_support::test_core().await;
        let state = crate::web::test_support::state_over(core, crate::config::AuthMode::Oidc).await;
        let res = crate::web::router(state)
            .oneshot(form("/auth/login", "username=dev&password=hunter2"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
