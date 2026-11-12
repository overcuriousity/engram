//! What the current sitting has touched: in memory only, web door only.
//! An access token is not a conversation; two agents sharing one would share a sitting.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// A bound on the map for a long afternoon, not a judgement of what is worth remembering.
pub const CARRY: usize = 20;

/// An artifact this sitting has been in.
#[derive(Debug, Clone)]
pub struct Touched {
    pub artifact_id: String,
    pub at: i64,
}

#[derive(Debug, Default)]
struct Sitting {
    /// Most recent first, capped at `CARRY`.
    touched: VecDeque<Touched>,
    /// Most recent first, capped at `CARRY`.
    queries: VecDeque<String>,
    last_at: i64,
}

/// A copy, so nothing holds the lock across a render.
#[derive(Debug, Default, Clone)]
pub struct Carried {
    pub touched: Vec<String>,
    pub queries: Vec<String>,
}

impl Carried {
    /// Nothing happened, or it went quiet; pages render nothing for one.
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
    /// Read by `Working::is_idle` so the registry can drop a departed subject's state.
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

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

    /// Search-as-you-type sends every prefix, so a typing burst folds into its longest query.
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
            // Only the head can be the run being typed; an earlier query that is a prefix is a
            // separate search.
            if s.queries
                .front()
                .is_some_and(|f| query.starts_with(f.as_str()))
            {
                s.queries.pop_front();
            }
            s.queries.retain(|q| q != query);
            s.queries.push_front(query.to_string());
            s.queries.truncate(CARRY);
        });
    }

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

    /// The locking and the expiry: an entry idle past `idle_secs` (the pursuit sweep's number) is
    /// dropped. Every entry, not only this session's, or a closed tab would stay in the map for the
    /// process's life.
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
        s.touched("sess", "a", 30, IDLE);

        let c = s.read("sess", 40, IDLE);
        assert_eq!(c.touched, vec!["a", "b"]);
    }

    #[test]
    fn a_sitting_that_went_quiet_is_gone() {
        let s = Sittings::default();
        s.queried("sess", "how do I mount an E01", 100, IDLE);

        assert!(!s.read("sess", 100 + IDLE, IDLE).is_cold());
        assert!(s.read("sess", 100 + IDLE + 1, IDLE).is_cold());
    }

    #[test]
    fn a_sitting_nobody_comes_back_to_is_swept_by_somebody_elses_write() {
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
        let s = Sittings::default();
        for q in ["h", "how", "how do I mount an E01"] {
            s.queried("sess", q, 10, IDLE);
        }
        s.queried("sess", "how do I mount an E0", 11, IDLE);

        assert_eq!(
            s.read("sess", 20, IDLE).queries,
            vec!["how do I mount an E01"]
        );
    }

    #[test]
    fn a_later_query_does_not_delete_an_earlier_one_it_happens_to_begin_with() {
        let s = Sittings::default();
        s.queried("sess", "E01", 10, IDLE);
        s.queried("sess", "write blocker", 20, IDLE);
        s.queried("sess", "E01 mounting on Linux", 30, IDLE);

        assert_eq!(
            s.read("sess", 40, IDLE).queries,
            vec!["E01 mounting on Linux", "write blocker", "E01"]
        );
    }

    #[test]
    fn asking_the_same_thing_twice_moves_it_up_rather_than_listing_it_twice() {
        let s = Sittings::default();
        s.queried("sess", "E01", 10, IDLE);
        s.queried("sess", "write blocker", 20, IDLE);
        s.queried("sess", "E01", 30, IDLE);

        assert_eq!(
            s.read("sess", 40, IDLE).queries,
            vec!["E01", "write blocker"]
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
