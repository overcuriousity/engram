//! What this sitting has touched.
//!
//! The base could already name what you were working on last Tuesday and not
//! what you are working on now: `jobs/pursuit.rs` reconstructs a sitting from
//! idle gaps in the search log, which means it exists only once it is over.
//! This is the live half — a working memory, read while the sitting is
//! happening, carried between the doors, gone when it goes.
//!
//! **In memory only.** No table, no migration, no expiry sweep. It dies with
//! the process, and a deploy mid-afternoon costs the operator their carried
//! context. That is the accepted price: a working memory that survives a
//! restart is a long-term memory, and engram has one of those already.
//!
//! It does not write activation, it does not open a pursuit, and it exists at
//! the web door only — for the API and `/mcp` an access token is not a
//! conversation, and two agent sessions sharing a token would share a sitting,
//! which is worse than having none.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// How much of a sitting is carried. Twenty is well past what any page shows;
/// the cap is here so a long afternoon cannot grow the map without bound, not
/// to decide what is worth remembering.
pub const CARRY: usize = 20;

/// An artifact this sitting has been in.
#[derive(Debug, Clone)]
pub struct Touched {
    pub artifact_id: String,
    pub at: i64,
}

/// One live sitting.
#[derive(Debug, Default)]
struct Sitting {
    /// Most recent first, capped at `CARRY`.
    touched: VecDeque<Touched>,
    /// Most recent first, capped at `CARRY`.
    queries: VecDeque<String>,
    last_at: i64,
}

/// What a door reads: a copy, so nothing holds the lock across a render.
#[derive(Debug, Default, Clone)]
pub struct Carried {
    /// Artifact ids, most recently touched first.
    pub touched: Vec<String>,
    /// Queries typed in this sitting, most recent first.
    pub queries: Vec<String>,
}

impl Carried {
    /// A cold sitting: nothing has happened, or what happened has gone quiet.
    /// The pages render nothing at all for one, rather than an empty box.
    pub fn is_cold(&self) -> bool {
        self.touched.is_empty() && self.queries.is_empty()
    }
}

/// Every live sitting, keyed by web session id.
#[derive(Debug, Default)]
pub struct Sittings {
    inner: Mutex<HashMap<String, Sitting>>,
}

impl Sittings {
    /// This sitting opened an artifact.
    pub fn touched(&self, session: &str, artifact_id: &str, at: i64, idle_secs: i64) {
        self.with(session, at, idle_secs, |s| {
            s.touched.retain(|t| t.artifact_id != artifact_id);
            s.touched.push_front(Touched {
                artifact_id: artifact_id.to_string(),
                at,
            });
            s.touched.truncate(CARRY);
        });
    }

    /// This sitting typed a query.
    ///
    /// A typing burst folds into one entry, the same way `record_search` folds
    /// one into a single event: the search box searches as you type, so a
    /// finished query arrives as every prefix of itself, and a rail listing
    /// "h", "ho", "how" would be a rail of one query spelled slowly. What is
    /// kept is the longest of a run — the query that was actually meant.
    pub fn queried(&self, session: &str, query: &str, at: i64, idle_secs: i64) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        self.with(session, at, idle_secs, |s| {
            if let Some(front) = s.queries.front()
                && front.starts_with(query)
            {
                // A backspace, or a repeat. The longer spelling stands.
                return;
            }
            s.queries
                .retain(|q| !query.starts_with(q.as_str()) && q != query);
            s.queries.push_front(query.to_string());
            s.queries.truncate(CARRY);
        });
    }

    /// What this sitting has touched, or nothing if it has gone quiet.
    ///
    /// Expiry happens here and on write rather than on a timer: there is no
    /// sweep to run, because a sitting is dropped by the next read that finds
    /// it stale — or, for the sitting nobody ever comes back to, by the next
    /// write from any session at all. See `with`.
    pub fn read(&self, session: &str, at: i64, idle_secs: i64) -> Carried {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(session) {
            Some(s) if at - s.last_at <= idle_secs => Carried {
                touched: s.touched.iter().map(|t| t.artifact_id.clone()).collect(),
                queries: s.queries.iter().cloned().collect(),
            },
            Some(_) => {
                map.remove(session);
                Carried::default()
            }
            None => Carried::default(),
        }
    }

    /// The whole of the locking, once, and the whole of the expiry with it. An
    /// entry idle for longer than `idle_secs` is not extended, it is dropped —
    /// the same number that already defines a sitting for the pursuit sweep, so
    /// the live definition and the reconstructed one agree by construction.
    ///
    /// Every entry, not only this session's, because expiring only the one
    /// being touched leaves out the case that matters: a browser tab closed and
    /// never opened again is a session no later read or write ever names, so
    /// nothing would find it stale and the map would grow for the life of the
    /// process, one abandoned sitting at a time. The lock is already held here,
    /// so the walk costs one integer comparison per live session — and the map
    /// is only ever large in exactly the case where the walk is worth doing.
    /// This session's own stale entry goes the same way, which is what makes a
    /// returning sitting a new one rather than a continued one.
    fn with(&self, session: &str, at: i64, idle_secs: i64, f: impl FnOnce(&mut Sitting)) {
        if session.is_empty() {
            return;
        }
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, s| at - s.last_at <= idle_secs);
        let entry = map.entry(session.to_string()).or_default();
        entry.last_at = at;
        f(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: i64 = 900;

    #[test]
    fn what_was_touched_comes_back_most_recent_first() {
        let s = Sittings::default();
        s.touched("sess", "a", 10, IDLE);
        s.touched("sess", "b", 20, IDLE);
        // Touching something again moves it, rather than listing it twice.
        s.touched("sess", "a", 30, IDLE);

        let c = s.read("sess", 40, IDLE);
        assert_eq!(c.touched, vec!["a", "b"]);
    }

    #[test]
    fn a_sitting_that_went_quiet_is_gone() {
        // The same number the pursuit sweep uses to decide a sitting ended.
        let s = Sittings::default();
        s.queried("sess", "how do I mount an E01", 100, IDLE);

        assert!(!s.read("sess", 100 + IDLE, IDLE).is_cold());
        assert!(s.read("sess", 100 + IDLE + 1, IDLE).is_cold());
    }

    #[test]
    fn a_sitting_nobody_comes_back_to_is_swept_by_somebody_elses_write() {
        // The abandoned tab is the case expiring-on-touch cannot reach: no
        // later read or write ever names that session, so nothing would find it
        // stale and the map would grow for the life of the process.
        let s = Sittings::default();
        s.touched("gone", "a", 100, IDLE);
        assert_eq!(s.inner.lock().unwrap().len(), 1);

        s.touched("here", "b", 100 + IDLE + 1, IDLE);

        let live = s.inner.lock().unwrap();
        assert_eq!(
            live.keys().collect::<Vec<_>>(),
            vec!["here"],
            "the session that went quiet must not outlive the process"
        );
    }

    #[test]
    fn coming_back_after_a_gap_starts_a_new_sitting() {
        let s = Sittings::default();
        s.queried("sess", "old subject", 100, IDLE);
        s.queried("sess", "new subject", 100 + IDLE + 1, IDLE);

        let c = s.read("sess", 100 + IDLE + 2, IDLE);
        assert_eq!(c.queries, vec!["new subject"]);
    }

    #[test]
    fn a_typing_burst_is_one_query() {
        // The search box searches as you type, so a finished query arrives as
        // every prefix of itself.
        let s = Sittings::default();
        for q in ["h", "how", "how do I mount an E01"] {
            s.queried("sess", q, 10, IDLE);
        }
        // And a backspace afterwards does not undo it.
        s.queried("sess", "how do I mount an E0", 11, IDLE);

        assert_eq!(
            s.read("sess", 20, IDLE).queries,
            vec!["how do I mount an E01"]
        );
    }

    #[test]
    fn two_sittings_do_not_see_each_other() {
        let s = Sittings::default();
        s.queried("one", "mine", 10, IDLE);
        s.queried("two", "theirs", 10, IDLE);

        assert_eq!(s.read("one", 20, IDLE).queries, vec!["mine"]);
        assert_eq!(s.read("two", 20, IDLE).queries, vec!["theirs"]);
    }

    #[test]
    fn a_door_with_no_session_carries_nothing() {
        // An access token is not a conversation, and two agent sessions sharing
        // one would share a sitting — worse than having none.
        let s = Sittings::default();
        s.queried("", "from a token", 10, IDLE);
        assert!(s.read("", 20, IDLE).is_cold());
    }

    #[test]
    fn a_long_afternoon_stays_bounded() {
        let s = Sittings::default();
        for i in 0..(CARRY + 10) {
            s.touched("sess", &format!("a-{i}"), i as i64, IDLE);
        }
        assert_eq!(s.read("sess", 100, IDLE).touched.len(), CARRY);
    }
}
