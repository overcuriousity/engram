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

/// Sliding session lifetime, 30 days.
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;
pub const SESSION_COOKIE: &str = "engram_session";

pub fn set_session_cookie(id: &str, secure: bool) -> String {
    let mut c = format!(
        "{SESSION_COOKIE}={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
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
        if let Some(h) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            && let Some(token) = bearer(h)
        {
            return tokens::verify(&state.core.store, &token).await;
        }

        if let Some(h) = parts
            .headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            && let Some(sid) = cookie_value(h, SESSION_COOKIE)
            && let Some(session) = state.core.store.get_session(&sid).await?
        {
            // Sliding expiry: active use keeps the session alive.
            state
                .core
                .store
                .extend_session(&sid, SESSION_TTL_SECS)
                .await?;
            return Ok(Identity {
                subject: session.subject,
                email: session.email,
                session: Some(sid),
            });
        }

        Err(Error::Unauthorized)
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

    #[test]
    fn session_cookie_carries_the_required_flags() {
        let c = set_session_cookie("abc123", true);
        assert!(c.contains("engram_session=abc123"));
        assert!(c.contains("HttpOnly"), "{c}"); // no JS access
        assert!(c.contains("SameSite=Lax"), "{c}"); // CSRF mitigation
        assert!(c.contains("Secure"), "{c}"); // HTTPS only
        assert!(c.contains("Path=/"), "{c}");
        assert!(c.contains("Max-Age="), "{c}");
    }

    #[test]
    fn insecure_deployments_omit_secure_but_keep_httponly() {
        // Over plain HTTP a Secure cookie is never sent back and login breaks.
        let c = set_session_cookie("abc123", false);
        assert!(!c.contains("Secure"));
        assert!(c.contains("HttpOnly"));
    }

    #[test]
    fn clearing_expires_the_cookie_immediately() {
        let c = clear_session_cookie();
        assert!(c.contains("engram_session="));
        assert!(c.contains("Max-Age=0"), "{c}");
    }

    #[test]
    fn cookie_value_is_read_from_a_multi_cookie_header() {
        let header = "theme=dark; engram_session=wanted; other=1";
        assert_eq!(
            cookie_value(header, SESSION_COOKIE).as_deref(),
            Some("wanted")
        );
        assert_eq!(cookie_value("theme=dark", SESSION_COOKIE), None);
        assert_eq!(cookie_value("", SESSION_COOKIE), None);
    }

    #[test]
    fn a_similarly_named_cookie_is_not_mistaken_for_the_session() {
        assert_eq!(cookie_value("xengram_session=nope", SESSION_COOKIE), None);
        assert_eq!(
            cookie_value("engram_session_old=nope", SESSION_COOKIE),
            None
        );
    }

    #[test]
    fn bearer_header_is_parsed_case_insensitively() {
        assert_eq!(bearer("Bearer engram_x").as_deref(), Some("engram_x"));
        assert_eq!(bearer("bearer engram_x").as_deref(), Some("engram_x"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("engram_x"), None);
    }
}
