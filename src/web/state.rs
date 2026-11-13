use crate::auth::oidc::{OidcClient, PendingStore};
use crate::config::{AuthMode, LocalConfig};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Long enough for a page load and an `EventSource` handshake.
pub const ASK_HANDOFF_TTL: Duration = Duration::from_secs(60);

pub struct AuthContext {
    pub mode: AuthMode,
    pub local: Option<LocalConfig>,
    pub oidc: Option<OidcClient>,
    pub pending: PendingStore,
    pub deployment: crate::web::security::Deployment,
}

#[derive(Clone)]
pub struct AppState {
    /// The only way a handler reaches data: there is no instance-wide core to fall back on.
    pub tenants: Arc<crate::tenants::Tenants>,
    pub auth: Arc<AuthContext>,
    pub config: Arc<crate::config::Config>,
    /// `EventSource` is GET-only, and a mutating GET is replayable; a one-shot id is not.
    /// Each entry is answered for the subject who parked it alone.
    pub ask_handoff: Arc<Mutex<HashMap<String, ParkedAsk>>>,
    /// Decide's, parked and spent as an ask's is.
    pub decide_handoff: Arc<Mutex<HashMap<String, ParkedDecide>>>,
}

pub struct Parked<T> {
    pub req: T,
    pub subject: String,
    pub at: Instant,
}

pub type ParkedAsk = Parked<crate::core::ask::AskRequest>;
pub type ParkedDecide = Parked<crate::core::decide::DecideRequest>;

/// Swept on every park: the map only grows when someone parks.
fn park<T>(map: &Mutex<HashMap<String, Parked<T>>>, req: T, subject: &str) -> String {
    let id = crate::store::new_id();
    if let Ok(mut m) = map.lock() {
        let now = Instant::now();
        m.retain(|_, p| now.duration_since(p.at) < ASK_HANDOFF_TTL);
        m.insert(
            id.clone(),
            Parked {
                req,
                subject: subject.to_string(),
                at: now,
            },
        );
    }
    id
}

/// One shot, so a reload cannot spend a second model call.
/// Another subject gets the unknown-id answer and the entry stays.
fn take<T>(map: &Mutex<HashMap<String, Parked<T>>>, id: &str, subject: &str) -> Option<T> {
    let mut m = map.lock().ok()?;
    if m.get(id).is_some_and(|p| p.subject != subject) {
        return None;
    }
    let p = m.remove(id)?;
    (Instant::now().duration_since(p.at) < ASK_HANDOFF_TTL).then_some(p.req)
}

impl AppState {
    pub fn ask_handoff_park(&self, req: crate::core::ask::AskRequest, subject: &str) -> String {
        park(&self.ask_handoff, req, subject)
    }

    pub fn ask_handoff_take(
        &self,
        id: &str,
        subject: &str,
    ) -> Option<crate::core::ask::AskRequest> {
        take(&self.ask_handoff, id, subject)
    }

    pub fn decide_handoff_park(
        &self,
        req: crate::core::decide::DecideRequest,
        subject: &str,
    ) -> String {
        park(&self.decide_handoff, req, subject)
    }

    pub fn decide_handoff_take(
        &self,
        id: &str,
        subject: &str,
    ) -> Option<crate::core::decide::DecideRequest> {
        take(&self.decide_handoff, id, subject)
    }
}

/// Whether `[infer.ask]` is configured; the nav reads it on every page.
pub fn ask_enabled(t: &crate::tenants::Tenant) -> bool {
    t.core.asks()
}

/// The account setting, else `Accept-Language`, else English.
/// Resolved at the door and stamped on the corpus: background jobs know no subject.
pub async fn capture_lang(
    tenant: &crate::tenants::Tenant,
    headers: &axum::http::HeaderMap,
) -> crate::infer::lang::Lang {
    use crate::infer::lang::Lang;
    // An unreadable control database must not refuse a capture.
    if let Ok(Some(set)) = tenant.core.store.control.lang(&tenant.user.subject).await {
        return set;
    }
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map(Lang::from_accept_language)
        .unwrap_or_default()
}
