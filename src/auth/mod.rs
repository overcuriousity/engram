pub mod local;
pub mod oidc;
pub mod tokens;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
}

/// Sliding session lifetime, 30 days.
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;
pub const SESSION_COOKIE: &str = "pkdb_session";
