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
    /// The configuration file this server was started with.
    ///
    /// Held because applying a tuning recommendation writes it: the running
    /// parameters and the file have to agree, or a restart would quietly undo
    /// a change the tuning history says was made.
    pub config_path: Arc<std::path::PathBuf>,
    /// Questions parked between the POST that creates them and the GET that
    /// streams them.
    ///
    /// `EventSource` is GET-only, and a GET that runs a model call and writes
    /// an `ask_events` row is a mutating GET — the kind browser history and
    /// prefetchers replay. An opaque one-shot id costs no schema and cannot be
    /// replayed. Entries are removed on consumption and swept on insert.
    ///
    /// Each entry remembers who parked it. The id is unguessable, but a URL is
    /// the kind of thing that ends up in a log or another tab, and a question
    /// belongs to the person who asked it: it is answered — and recorded — for
    /// that subject alone.
    pub ask_handoff: Arc<Mutex<HashMap<String, ParkedAsk>>>,
}

/// One question waiting for its stream: what was asked, by whom, and when.
pub struct ParkedAsk {
    pub req: crate::core::ask::AskRequest,
    pub subject: String,
    pub at: Instant,
}

impl AppState {
    /// Swept on every park rather than on a timer, because the map only grows
    /// when someone parks: a page that was never streamed leaves one entry,
    /// and the next ask is what collects it.
    pub fn ask_handoff_park(&self, req: crate::core::ask::AskRequest, subject: &str) -> String {
        let id = crate::store::new_id();
        if let Ok(mut m) = self.ask_handoff.lock() {
            let now = Instant::now();
            m.retain(|_, p| now.duration_since(p.at) < ASK_HANDOFF_TTL);
            m.insert(
                id.clone(),
                ParkedAsk {
                    req,
                    subject: subject.to_string(),
                    at: now,
                },
            );
        }
        id
    }

    /// One shot: the entry is removed whether or not the stream succeeds, so a
    /// reload of the streaming URL cannot spend a second model call.
    ///
    /// Only for the subject that parked it. Anyone else gets the same answer an
    /// unknown id gets, and the entry stays where it is: the asker's own stream
    /// may still be on its way, and a stranger's guess must not spend it.
    pub fn ask_handoff_take(
        &self,
        id: &str,
        subject: &str,
    ) -> Option<crate::core::ask::AskRequest> {
        let mut m = self.ask_handoff.lock().ok()?;
        if m.get(id).is_some_and(|p| p.subject != subject) {
            return None;
        }
        let p = m.remove(id)?;
        (Instant::now().duration_since(p.at) < ASK_HANDOFF_TTL).then_some(p.req)
    }
}

/// Whether the ask door is open: `[infer.ask]` is configured. The nav reads
/// it through every page's template, the same way it reads `judge_pending`.
pub fn ask_enabled(st: &AppState) -> bool {
    st.core.asks()
}

/// What the nav needs to know about judging: how many searches are waiting, or
/// `None` when nothing is being captured and the entry does not belong there.
///
/// Every full page carries this, because judging is a habit and a habit needs a
/// door you pass rather than a page you remember. The count is one indexed
/// `count(*)`; a failure returns `None`, since a broken badge is not a reason to
/// fail the page it sits on.
pub async fn judge_pending(st: &AppState) -> Option<i64> {
    if !st.core.learn.enabled {
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
