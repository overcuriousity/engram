use crate::auth::oidc::{OidcClient, PendingStore};
use crate::config::{AuthMode, LocalConfig};
use crate::core::Core;
use std::sync::Arc;

pub struct AuthContext {
    pub mode: AuthMode,
    pub local: Option<LocalConfig>,
    pub oidc: Option<OidcClient>,
    pub pending: PendingStore,
    pub secure_cookies: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub core: Core,
    pub auth: Arc<AuthContext>,
}
