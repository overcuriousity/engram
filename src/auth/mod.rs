pub mod grants;
pub mod local;
pub mod oidc;
pub mod tokens;

use crate::error::Error;
use crate::web::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    /// The web session this request came in on, where there is one.
    ///
    /// `None` for a bearer token, and that absence is load-bearing: it is what
    /// keeps the live sitting at the web door. An access token is not a
    /// conversation, and two agent sessions sharing one token would share a
    /// sitting — worse than having none. Giving those doors a real session
    /// identity is a change to the doors.
    pub session: Option<String>,
}

pub const SESSION_IDLE_DAYS: i64 = 30;
pub const SUBJECT_KEY: &str = "engram.subject";
pub const EMAIL_KEY: &str = "engram.email";

/// `__Host-` needs `Secure`, so plain http gets the bare name.
pub fn session_cookie_name(dep: &crate::web::security::Deployment) -> &'static str {
    if dep.secure_cookies() {
        "__Host-engram-session"
    } else {
        "engram-session"
    }
}

pub fn session_layer(
    control: &crate::store::control::Control,
    dep: &crate::web::security::Deployment,
) -> tower_sessions::SessionManagerLayer<tower_sessions_sqlx_store::SqliteStore> {
    use tower_sessions::cookie::SameSite;
    use tower_sessions::cookie::time::Duration;
    tower_sessions::SessionManagerLayer::new(tower_sessions_sqlx_store::SqliteStore::new(
        control.pool.clone(),
    ))
    .with_name(session_cookie_name(dep))
    .with_secure(dep.secure_cookies())
    .with_http_only(true)
    .with_same_site(SameSite::Lax)
    .with_path("/")
    .with_expiry(tower_sessions::Expiry::OnInactivity(Duration::days(
        SESSION_IDLE_DAYS,
    )))
}

/// Signs `identity` in on `session`, under a fresh id.
pub async fn sign_in(session: &tower_sessions::Session, identity: &Identity) -> Result<(), Error> {
    let fail = |e: tower_sessions::session::Error| Error::Store(format!("session: {e}"));
    session.cycle_id().await.map_err(fail)?;
    session
        .insert(SUBJECT_KEY, &identity.subject)
        .await
        .map_err(fail)?;
    session
        .insert(EMAIL_KEY, &identity.email)
        .await
        .map_err(fail)?;
    Ok(())
}

pub fn bearer(header: &str) -> Option<String> {
    let (scheme, value) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| value.trim().to_string())
}

/// Both authentication paths converge here. Handlers ask for an `Identity` and
/// get 401 automatically if there isn't one; `core` never sees a cookie or a
/// token.
impl FromRequestParts<AppState> for Identity {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Answered once per request, however many extractors ask.
        //
        // Two of them routinely do: `Tenant` resolves an `Identity` to find
        // whose base to open, and a handler that also wants the session id
        // names `Identity` beside it. Each ask was a full authentication — a
        // session read and a sliding `extend_session` write per keystroke on
        // the incremental search route, and for a bearer caller a second walk
        // of the token table with an argon2 verify on every row. Neither
        // repetition can produce a different answer: nothing in a request's
        // lifetime changes the credential it arrived with.
        if let Some(id) = parts.extensions.get::<Identity>() {
            return Ok(id.clone());
        }
        let id = Self::authenticate(parts, state).await?;
        parts.extensions.insert(id.clone());
        Ok(id)
    }
}

impl Identity {
    /// The authentication itself, with no memo in front of it.
    async fn authenticate(parts: &mut Parts, state: &AppState) -> Result<Self, Error> {
        if let Some(h) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            && let Some(token) = bearer(h)
        {
            if token.starts_with(tokens::TOKEN_PREFIX) {
                return tokens::verify(state.tenants.control(), &token).await;
            }
            if parts.uri.path() == "/mcp"
                && let Some(oidc) = &state.auth.oidc
                && let Some((iss, sub)) = oidc.verify_jwt(&token).await?
                && state.tenants.control().issuer_of(&sub).await?.as_deref() == Some(iss.as_str())
            {
                return Ok(Identity {
                    subject: sub,
                    email: None,
                    session: None,
                });
            }
            return Err(Error::Unauthorized);
        }

        let Some(session) = parts.extensions.get::<tower_sessions::Session>().cloned() else {
            return Err(Error::Unauthorized);
        };
        let fail = |e: tower_sessions::session::Error| Error::Store(format!("session: {e}"));
        let Some(subject) = session.get::<String>(SUBJECT_KEY).await.map_err(fail)? else {
            return Err(Error::Unauthorized);
        };
        let email = session
            .get::<Option<String>>(EMAIL_KEY)
            .await
            .map_err(fail)?
            .flatten();
        Ok(Identity {
            subject,
            email,
            session: session.id().map(|id| id.to_string()),
        })
    }
}

/// `Option<Identity>` for the one page that has to render for a browser with
/// no session rather than be bounced to a login it cannot come back from —
/// see `src/web/pair.rs`. Only "no credentials" becomes `None`; a store
/// failure while checking is still an error, because treating one as
/// signed-out would silently show the signed-out page to someone who is not.
impl axum::extract::OptionalFromRequestParts<AppState> for Identity {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        match <Identity as FromRequestParts<AppState>>::from_request_parts(parts, state).await {
            Ok(id) => Ok(Some(id)),
            Err(Error::Unauthorized) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_request_authenticates_once_however_many_extractors_ask() {
        let core = crate::core::test_support::test_core().await;
        let state =
            crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await;
        let control = state.tenants.control();
        let (row, token) = tokens::mint(control, "t", "dev", None).await.unwrap();
        let req = || {
            axum::http::Request::builder()
                .uri("/")
                .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                .body(())
                .unwrap()
                .into_parts()
                .0
        };
        let mut parts = req();
        let first = Identity::from_request_parts(&mut parts, &state)
            .await
            .expect("the token is live");
        tokens::revoke(control, &row.id, "dev").await.unwrap();
        let second = Identity::from_request_parts(&mut parts, &state)
            .await
            .expect("the second extractor re-authenticated instead of reading the memo");
        assert_eq!(first, second);
        assert!(matches!(
            Identity::from_request_parts(&mut req(), &state).await,
            Err(Error::Unauthorized)
        ));
    }

    #[test]
    fn bearer_header_is_parsed_case_insensitively() {
        assert_eq!(bearer("Bearer engram_x").as_deref(), Some("engram_x"));
        assert_eq!(bearer("bearer engram_x").as_deref(), Some("engram_x"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("engram_x"), None);
    }
}
