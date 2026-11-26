use crate::config::AuthMode;
use crate::error::{Error, Result};
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};

/// askama 0.16 ships no axum integration, so templates get a thin wrapper.
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
    oidc: bool,
    error: Option<String>,
    /// Already checked by `safe_next`; carried through the form so signing in returns there.
    next: Option<String>,
}

#[derive(serde::Deserialize)]
struct LoginQuery {
    #[serde(default)]
    go: Option<String>,
}

/// Only a page inside `/ui/` and never `/ui//host`, which a browser reads as protocol-relative.
/// `None` means the caller's default, which `go=1` also lands on.
fn safe_next(raw: Option<&str>) -> Option<String> {
    match raw {
        Some(p) if p.starts_with("/ui/") && !p.starts_with("/ui//") => Some(p.to_string()),
        _ => None,
    }
}

const AFTER_LOGIN: &str = "/ui/search";

/// Where the pre-login session keeps the `state` it was sent off with.
const OIDC_STATE: &str = "oidc_state";

async fn login_page(
    State(st): State<AppState>,
    session: tower_sessions::Session,
    Query(q): Query<LoginQuery>,
) -> Result<Response> {
    match st.auth.mode {
        AuthMode::Oidc => {
            let client = st
                .auth
                .oidc
                .as_ref()
                .ok_or_else(|| Error::Validation("oidc not configured".into()))?;
            if q.go.is_some() {
                let (url, mut pending) = client.authorize_url()?;
                // The provider hands back only `state`, so the destination is kept with the pending attempt.
                pending.next = safe_next(q.go.as_deref());
                // The callback is honoured only in this browser: a `code` and `state` passed
                // to someone else would otherwise sign them in as the sender.
                session
                    .insert(OIDC_STATE, &pending.csrf)
                    .await
                    .map_err(|e| Error::Store(format!("session: {e}")))?;
                if !st.auth.pending.put(pending) {
                    let busy = (StatusCode::TOO_MANY_REQUESTS, "too many sign-ins in progress");
                    return Ok(busy.into_response());
                }
                return Ok(Redirect::to(&url).into_response());
            }
            Ok(HtmlTemplate(LoginTemplate {
                oidc: true,
                error: None,
                next: None,
            })
            .into_response())
        }
        AuthMode::Local => Ok(HtmlTemplate(LoginTemplate {
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
    /// Survives a mistyped password, since the form is re-rendered with it.
    #[serde(default)]
    next: Option<String>,
}

async fn login_submit(
    State(st): State<AppState>,
    session: tower_sessions::Session,
    Form(f): Form<LoginForm>,
) -> Result<Response> {
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
                oidc: false,
                error: Some("Incorrect username or password.".into()),
                next: safe_next(f.next.as_deref()),
            }),
        )
            .into_response());
    };

    start_session(&session, &identity, safe_next(f.next.as_deref())).await
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

async fn callback(
    State(st): State<AppState>,
    session: tower_sessions::Session,
    Query(q): Query<CallbackQuery>,
) -> Result<Response> {
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
    let began: Option<String> = session
        .remove(OIDC_STATE)
        .await
        .map_err(|e| Error::Store(format!("session: {e}")))?;
    if began.as_deref() != Some(state_param.as_str()) {
        tracing::warn!("a sign-in came back to a browser that did not begin it");
        return Err(Error::Unauthorized);
    }
    // Single use: this also defeats a replayed callback URL.
    let pending = st
        .auth
        .pending
        .take(&state_param)
        .ok_or(Error::Unauthorized)?;
    let identity = client.exchange(&pending, &code, &state_param).await?;
    bind_to_issuer(&st, &identity, client.issuer()).await?;
    // Re-checked on the way out: the pending store is only as trustworthy as its input.
    let next = safe_next(pending.next.as_deref());
    start_session(&session, &identity, next).await
}

/// A subject is one person only under the issuer that first vouched for it.
async fn bind_to_issuer(
    st: &AppState,
    identity: &crate::auth::Identity,
    issuer: &str,
) -> Result<()> {
    let control = st.tenants.control();
    if control
        .issuer_of(&identity.subject)
        .await?
        .is_some_and(|bound| bound != issuer)
    {
        tracing::warn!(subject = %identity.subject, "sign-in from another issuer refused");
        return Err(Error::Forbidden);
    }
    control
        .provision(&identity.subject, identity.email.as_deref())
        .await?;
    if !control.bind_issuer(&identity.subject, issuer).await? {
        return Err(Error::Forbidden);
    }
    Ok(())
}

async fn start_session(
    session: &tower_sessions::Session,
    identity: &crate::auth::Identity,
    next: Option<String>,
) -> Result<Response> {
    crate::auth::sign_in(session, identity).await?;
    Ok(Redirect::to(&next.unwrap_or_else(|| AFTER_LOGIN.to_string())).into_response())
}

async fn logout(session: tower_sessions::Session) -> Result<Response> {
    // The record goes, not just the cookie: a copied cookie must stop working.
    session
        .flush()
        .await
        .map_err(|e| Error::Store(format!("session: {e}")))?;
    Ok(Redirect::to("/auth/login").into_response())
}

/// Each login begun under OIDC holds a pending attempt for ten minutes, so the page is paced too.
pub const LOGIN_PAGE_PER_MINUTE: u32 = 30;
pub const LOGIN_PAGE_BURST: u32 = 20;
pub const SIGN_IN_PER_MINUTE: u32 = 10;
pub const SIGN_IN_BURST: u32 = 5;

pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route(
            "/auth/login",
            get(login_page)
                .layer(crate::web::security::rate_limit(
                    LOGIN_PAGE_PER_MINUTE,
                    LOGIN_PAGE_BURST,
                ))
                .merge(post(login_submit).layer(crate::web::security::rate_limit(
                    SIGN_IN_PER_MINUTE,
                    SIGN_IN_BURST,
                ))),
        )
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
    async fn the_login_page_is_reachable_without_credentials_and_shows_no_chrome() {
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
        let body = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        for chrome in [
            r#"class="oc-tabbar""#,
            r#"class="installnudge""#,
            r#"class="oc-topbar""#,
        ] {
            assert!(
                !body.contains(chrome),
                "{chrome} on the login page:\n{body}"
            );
        }
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
        assert!(cookie.contains("engram-session="));
        assert!(cookie.contains("HttpOnly"));
        assert_eq!(res.headers()["location"], "/ui/search");
    }

    #[tokio::test]
    async fn signing_in_lands_on_the_page_that_asked_for_the_login() {
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
        // A redirect that follows user input is a phishing hop.
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

    async fn local_app_on(
        dep: crate::web::security::Deployment,
    ) -> (axum::Router, crate::web::state::AppState) {
        let core = crate::core::test_support::test_core().await;
        let mut st =
            crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await;
        st.auth = std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Local,
            local: Some(crate::config::LocalConfig {
                username: "dev".into(),
                password_hash: crate::auth::local::hash_password("hunter2").unwrap(),
            }),
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            deployment: dep,
        });
        (crate::web::router(st.clone()), st)
    }

    fn on(public_url: &str, plain: bool) -> crate::web::security::Deployment {
        crate::web::security::Deployment {
            public_url: url::Url::parse(public_url).ok(),
            allow_plain_http: plain,
            ..crate::web::security::Deployment::loopback(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                8080,
            )))
        }
    }

    #[tokio::test]
    async fn the_session_cookie_is_host_prefixed_secure_and_lax() {
        let (app, _) = local_app_on(on("https://engram.example", false)).await;
        let res = app
            .oneshot(form("/auth/login", "username=dev&password=hunter2"))
            .await
            .unwrap();
        let c = res.headers()["set-cookie"].to_str().unwrap();
        assert!(c.starts_with("__Host-engram-session="), "{c}");
        for flag in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
            assert!(c.contains(flag), "{flag} missing: {c}");
        }
        assert!(!c.contains("Domain"), "{c}");
    }

    #[tokio::test]
    async fn the_cookie_loses_secure_and_prefix_over_allowed_plain_http() {
        let (app, _) = local_app_on(on("http://engram.lan", true)).await;
        let res = app
            .oneshot(form("/auth/login", "username=dev&password=hunter2"))
            .await
            .unwrap();
        let c = res.headers()["set-cookie"].to_str().unwrap();
        assert!(c.starts_with("engram-session="), "{c}");
        assert!(!c.contains("Secure"), "{c}");
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax"), "{c}");
    }

    #[tokio::test]
    async fn the_session_id_rotates_on_sign_in() {
        let dep = on("https://engram.example", false);
        let (app, st) = local_app_on(dep.clone()).await;
        let before =
            crate::web::test_support::session_cookie(st.tenants.control(), &dep, "someone").await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/auth/login")
                    .method("POST")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", &before)
                    .body(Body::from("username=dev&password=hunter2"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let after = res.headers()["set-cookie"].to_str().unwrap();
        let id = |c: &str| {
            c.split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap()
                .1
                .to_string()
        };
        assert_ne!(
            id(&before),
            id(after),
            "the pre-sign-in id survived sign-in"
        );
    }

    async fn oidc_app() -> (
        axum::Router,
        crate::web::state::AppState,
        wiremock::MockServer,
    ) {
        let core = crate::core::test_support::test_core().await;
        let mut st =
            crate::web::test_support::state_over(core, crate::config::AuthMode::Oidc).await;
        let (idp, client) = crate::auth::oidc::fake_client(&st.auth.deployment.base()).await;
        st.auth = std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Oidc,
            local: None,
            oidc: Some(client),
            pending: crate::auth::oidc::PendingStore::new(),
            deployment: st.auth.deployment.clone(),
        });
        (crate::web::router(st.clone()), st, idp)
    }

    /// Starts a login; the `state` the provider will hand back, and the browser's cookie.
    async fn begin(app: &axum::Router) -> (String, String) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/login?go=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(res.status().is_redirection(), "{}", res.status());
        let to = res.headers()["location"].to_str().unwrap().to_string();
        let state = url::Url::parse(&to)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string())
            .expect("the provider is sent a state");
        let cookie = res
            .headers()
            .get("set-cookie")
            .map(|c| c.to_str().unwrap().split(';').next().unwrap().to_string())
            .unwrap_or_default();
        (state, cookie)
    }

    async fn come_back(app: &axum::Router, state: &str, cookie: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/auth/callback?code=x&state={state}"))
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_callback_is_good_only_in_the_browser_that_began_the_login() {
        let (app, st, _idp) = oidc_app().await;

        // Handed to someone else: their browser never began this login.
        let (sent_on, _) = begin(&app).await;
        come_back(&app, &sent_on, "").await;
        assert!(
            st.auth.pending.take(&sent_on).is_some(),
            "a browser that never began the login got as far as the code exchange"
        );

        // The browser that began it is let through to the exchange, which spends the attempt.
        let (own, cookie) = begin(&app).await;
        come_back(&app, &own, &cookie).await;
        assert!(st.auth.pending.take(&own).is_none());
    }

    #[tokio::test]
    async fn beginning_logins_without_end_is_slowed_down() {
        use axum::extract::connect_info::MockConnectInfo;
        let (_router, st, _idp) = oidc_app().await;
        // The secured app: the limiter keys on the client address `secure` works out.
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 4000));
        let app = crate::web::app(st.clone(), &st.auth.deployment).layer(MockConnectInfo(peer));
        let mut refused = 0;
        for _ in 0..40 {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/auth/login?go=1")
                        .header("host", "engram.test")
                        .header("x-forwarded-for", "203.0.113.9")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if res.status() == StatusCode::TOO_MANY_REQUESTS {
                refused += 1;
            }
        }
        assert!(refused > 0, "forty logins begun at once, none refused");
    }

    #[tokio::test]
    async fn an_oidc_user_from_another_issuer_is_refused() {
        let core = crate::core::test_support::test_core().await;
        let st = crate::web::test_support::state_over(core, crate::config::AuthMode::Oidc).await;
        let who = crate::auth::Identity {
            subject: "sub-1".into(),
            email: Some("same@example.com".into()),
            session: None,
        };
        super::bind_to_issuer(&st, &who, "https://idp.example")
            .await
            .unwrap();
        super::bind_to_issuer(&st, &who, "https://idp.example")
            .await
            .unwrap();
        assert!(matches!(
            super::bind_to_issuer(&st, &who, "https://other.example").await,
            Err(crate::error::Error::Forbidden)
        ));
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
