use crate::auth::oidc::{OidcClient, PendingStore};
use crate::config::{AuthMode, LocalConfig};
use crate::core::Core;
use std::sync::Arc;

pub struct AuthContext {
    pub mode: AuthMode,
    pub local: Option<LocalConfig>,
    pub oidc: Option<OidcClient>,
    pub pending: PendingStore,
    /// Whether to set the `Secure` flag on session cookies.
    pub secure_cookies: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub core: Core,
    pub auth: Arc<AuthContext>,
}

/// What the nav needs to know about judging: how many searches are waiting, or
/// `None` when nothing is being captured and the entry does not belong there.
///
/// Every full page carries this, because judging is a habit and a habit needs a
/// door you pass rather than a page you remember. The count is one indexed
/// `count(*)`; a failure returns `None`, since a broken badge is not a reason to
/// fail the page it sits on.
pub async fn judge_pending(st: &AppState) -> Option<i64> {
    if !st.core.feedback.enabled {
        return None;
    }
    match st.core.store.pending_count().await {
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!(error = %e, "could not count searches waiting to be judged");
            None
        }
    }
}
