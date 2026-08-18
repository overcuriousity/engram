use crate::auth::oidc::{OidcClient, PendingStore};
use crate::config::{AuthMode, LocalConfig};
use crate::core::Core;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a parked question stays askable.
///
/// Long enough for a page load and an `EventSource` handshake, short enough
/// that a tab closed between the two leaves nothing behind worth finding.
pub const ASK_HANDOFF_TTL: Duration = Duration::from_secs(60);

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
    /// Questions parked between the POST that creates them and the GET that
    /// streams them.
    ///
    /// `EventSource` is GET-only, and a GET that runs a model call and writes
    /// an `ask_events` row is a mutating GET — the kind browser history and
    /// prefetchers replay. An opaque one-shot id costs no schema and cannot be
    /// replayed. Entries are removed on consumption and swept on insert.
    pub ask_handoff: Arc<Mutex<HashMap<String, (crate::core::ask::AskRequest, Instant)>>>,
}

impl AppState {
    /// Swept on every park rather than on a timer, because the map only grows
    /// when someone parks: a page that was never streamed leaves one entry,
    /// and the next ask is what collects it.
    pub fn ask_handoff_park(&self, req: crate::core::ask::AskRequest) -> String {
        let id = crate::store::new_id();
        if let Ok(mut m) = self.ask_handoff.lock() {
            let now = Instant::now();
            m.retain(|_, (_, at)| now.duration_since(*at) < ASK_HANDOFF_TTL);
            m.insert(id.clone(), (req, now));
        }
        id
    }

    /// One shot: the entry is removed whether or not the stream succeeds, so a
    /// reload of the streaming URL cannot spend a second model call.
    pub fn ask_handoff_take(&self, id: &str) -> Option<crate::core::ask::AskRequest> {
        let mut m = self.ask_handoff.lock().ok()?;
        let (req, at) = m.remove(id)?;
        (Instant::now().duration_since(at) < ASK_HANDOFF_TTL).then_some(req)
    }
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
