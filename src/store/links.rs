//! What was reached together.
//!
//! `artifact_pairs` is about two texts saying the same thing; this is about two
//! texts being *needed* together — the config passage and the troubleshooting
//! passage for one subsystem are strangers to the embedding and inseparable to
//! the person who needed both to answer one question.
//!
//! Every strength here is stored as a value and the stamp it was true at, and
//! read through `decayed`. Nothing is ever decayed in place: learning is one
//! UPDATE and forgetting costs no writes, which is what lets a sweep run every
//! half hour on a base of any size.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Binding queries kept per link. Three is what a person reads in the pane;
/// the count of *distinct* ones is `queries`, which is not bounded by this.
pub const MAX_CUES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Bound by use, nothing has ruled on it. Shown, prunable, judgeable.
    Learning,
    /// The judge named the relation. Shown with its reason, and never pruned by
    /// decay: a verified relation is about content, not use.
    Related,
    /// A coincidence of retrieval. Kept so it is not asked again, hidden from
    /// the pane, reopened only if either text changes.
    Unrelated,
    /// The operator said no. Never shown, never judged, never pruned.
    Dismissed,
}

impl LinkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkState::Learning => "learning",
            LinkState::Related => "related",
            LinkState::Unrelated => "unrelated",
            LinkState::Dismissed => "dismissed",
        }
    }
    pub fn parse(s: &str) -> LinkState {
        match s {
            "related" => LinkState::Related,
            "unrelated" => LinkState::Unrelated,
            "dismissed" => LinkState::Dismissed,
            _ => LinkState::Learning,
        }
    }
}

/// One binding query and how often it bound this pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cue {
    pub q: String,
    pub n: i64,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub a_id: String,
    pub b_id: String,
    /// Strength as of `bumped_at`. Read it through `decayed`, never directly.
    pub weight: f64,
    pub bumped_at: i64,
    pub queries: i64,
    pub cues: Vec<Cue>,
    pub state: LinkState,
    pub reason: Option<String>,
    pub judged_rev_a: Option<i64>,
    pub judged_rev_b: Option<i64>,
    pub judge_attempts: i64,
    pub created_at: i64,
}

/// The pair in the order the table stores it. `a_id < b_id` is a CHECK, so this
/// is not a convention that can be forgotten at one call site.
pub fn canonical<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Two spellings of one question are one cue: lowercased, whitespace collapsed.
pub fn normalize_query(q: &str) -> String {
    q.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Strength now, from strength then. `half_life_days <= 0` turns decay off.
///
/// A clock that moved backwards — a restored database, an NTP correction —
/// must not *grow* a weight, so the elapsed time is floored at zero.
pub fn decayed(weight: f64, bumped_at: i64, at: i64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return weight;
    }
    let elapsed = (at - bumped_at).max(0) as f64;
    weight * 2f64.powf(-elapsed / (half_life_days * 86_400.0))
}

pub(crate) fn row_to_link(r: &sqlx::sqlite::SqliteRow) -> Link {
    Link {
        a_id: r.get("a_id"),
        b_id: r.get("b_id"),
        weight: r.get("weight"),
        bumped_at: r.get("bumped_at"),
        queries: r.get("queries"),
        cues: serde_json::from_str(&r.get::<String, _>("cues")).unwrap_or_default(),
        state: LinkState::parse(r.get::<String, _>("state").as_str()),
        reason: r.get("reason"),
        judged_rev_a: r.get("judged_rev_a"),
        judged_rev_b: r.get("judged_rev_b"),
        judge_attempts: r.get("judge_attempts"),
        created_at: r.get("created_at"),
    }
}

/// Fold one query into the cue list. Returns whether it was new to this link.
///
/// Only the busiest `MAX_CUES` survive, and a cue that falls off the end can be
/// counted as new again later — so `queries` is a floor on the number of
/// distinct questions that bound this pair rather than an exact count. Exactness
/// would cost a second table for a number that only gates the judge.
fn bump_cue(cues: &mut Vec<Cue>, q: &str) -> bool {
    let is_new = if let Some(c) = cues.iter_mut().find(|c| c.q == q) {
        c.n += 1;
        false
    } else {
        cues.push(Cue {
            q: q.to_string(),
            n: 1,
        });
        true
    };
    cues.sort_by_key(|c| std::cmp::Reverse(c.n));
    cues.truncate(MAX_CUES);
    is_new
}

impl Store {
    /// Strengthen the link between two artifacts, folding the decay in.
    ///
    /// `cue` is the normalised query that bound them, where there is one. A
    /// bump with no cue — a confirmation replayed for an event whose
    /// co-appearance was already folded in — strengthens without claiming a new
    /// binding question.
    ///
    /// One transaction per pair. An event with ten shown candidates is 45 of
    /// these, which is a few milliseconds of local SQLite and no network at all.
    pub async fn bump_link(
        &self,
        a: &str,
        b: &str,
        delta: f64,
        cue: Option<&str>,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        // An artifact is not linked to itself, and the CHECK would refuse it
        // anyway — caught here so a caller enumerating pairs need not.
        if a == b {
            return Ok(());
        }
        let (a, b) = canonical(a, b);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT weight, bumped_at, queries, cues FROM artifact_links
              WHERE a_id = ? AND b_id = ?",
        )
        .bind(a)
        .bind(b)
        .fetch_optional(&mut *tx)
        .await?;

        match row {
            Some(r) => {
                let mut cues: Vec<Cue> =
                    serde_json::from_str(&r.get::<String, _>("cues")).unwrap_or_default();
                let fresh = cue.is_some_and(|q| bump_cue(&mut cues, q));
                let weight =
                    decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days) + delta;
                sqlx::query(
                    "UPDATE artifact_links
                        SET weight = ?, bumped_at = ?, queries = ?, cues = ?
                      WHERE a_id = ? AND b_id = ?",
                )
                .bind(weight)
                .bind(at)
                .bind(r.get::<i64, _>("queries") + i64::from(fresh))
                .bind(serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()))
                .bind(a)
                .bind(b)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                let mut cues = Vec::new();
                if let Some(q) = cue {
                    bump_cue(&mut cues, q);
                }
                sqlx::query(
                    "INSERT INTO artifact_links
                       (a_id, b_id, weight, bumped_at, queries, cues, state, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, 'learning', ?)",
                )
                .bind(a)
                .bind(b)
                .bind(delta)
                .bind(at)
                .bind(cues.len() as i64)
                .bind(serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()))
                .bind(now())
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// One link, whichever way round it is named.
    pub async fn get_link(&self, a: &str, b: &str) -> Result<Option<Link>> {
        let (a, b) = canonical(a, b);
        Ok(
            sqlx::query("SELECT * FROM artifact_links WHERE a_id = ? AND b_id = ?")
                .bind(a)
                .bind(b)
                .fetch_optional(&self.pool)
                .await?
                .as_ref()
                .map(row_to_link),
        )
    }

    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewArtifact;

    /// Two artifacts in one corpus, returned as their ids.
    async fn two(store: &Store) -> (String, String) {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = ["alpha", "beta"]
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).into(),
                corpus_span: None,
                title: Some((*t).into()),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = store.insert_artifacts(&src.id, &new).await.unwrap();
        (made[0].id.clone(), made[1].id.clone())
    }

    #[test]
    fn a_pair_is_filed_the_same_way_round_however_it_is_named() {
        // The primary key is (a_id, b_id) with a CHECK that a < b, so a lookup
        // by either side is two indexed reads and there is no "which way round"
        // bug to have.
        assert_eq!(canonical("b", "a"), ("a", "b"));
        assert_eq!(canonical("a", "b"), ("a", "b"));
    }

    #[test]
    fn a_query_is_the_same_cue_however_it_was_typed() {
        assert_eq!(normalize_query("  Loop   Device \n"), "loop device");
    }

    #[test]
    fn weight_halves_over_one_half_life() {
        // Lazy decay is what makes forgetting free: no sweep walks every row to
        // subtract from it, so this function is the only place decay happens.
        let day = 86_400;
        assert!((decayed(4.0, 0, 30 * day, 30.0) - 2.0).abs() < 1e-9);
        assert!((decayed(4.0, 0, 60 * day, 30.0) - 1.0).abs() < 1e-9);
        // Not yet moved, and never grown by a clock running backwards.
        assert!((decayed(4.0, 100, 100, 30.0) - 4.0).abs() < 1e-9);
        assert!((decayed(4.0, 100, 0, 30.0) - 4.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn bumping_folds_the_decay_in_rather_than_adding_to_a_stale_number() {
        // `weight` means "strength as of bumped_at". Adding to it without
        // folding the decay in would make a link that was strong a year ago and
        // used once today as strong as one used constantly.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        let day = 86_400;
        store
            .bump_link(&a, &b, 4.0, Some("fat32"), 30.0, 0)
            .await
            .unwrap();
        store
            .bump_link(&b, &a, 1.0, Some("fat32"), 30.0, 30 * day)
            .await
            .unwrap();

        let l = store.get_link(&a, &b).await.unwrap().expect("the link");
        assert!((l.weight - 3.0).abs() < 1e-6, "weight was {}", l.weight);
        assert_eq!(l.bumped_at, 30 * day);
        // Named the other way round the second time, and still one row.
        assert_eq!(l.a_id, a.min(b.clone()));
    }

    #[tokio::test]
    async fn the_same_query_twice_is_one_binding_query() {
        // What separates a link from one search typed twice. The cue count
        // still climbs, because that is how the top three are chosen.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        for _ in 0..3 {
            store
                .bump_link(&a, &b, 1.0, Some("fat32"), 30.0, 0)
                .await
                .unwrap();
        }
        store
            .bump_link(&a, &b, 1.0, Some("ntfs"), 30.0, 0)
            .await
            .unwrap();

        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.queries, 2);
        assert_eq!(l.cues[0].q, "fat32");
        assert_eq!(l.cues[0].n, 3);
        assert_eq!(l.cues.len(), 2);
    }

    #[tokio::test]
    async fn only_three_binding_queries_are_kept_and_the_busiest_lead() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        for q in ["one", "two", "three", "four"] {
            store
                .bump_link(&a, &b, 1.0, Some(q), 30.0, 0)
                .await
                .unwrap();
        }
        store
            .bump_link(&a, &b, 1.0, Some("two"), 30.0, 0)
            .await
            .unwrap();

        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.cues.len(), MAX_CUES);
        assert_eq!(l.cues[0].q, "two", "the busiest cue must lead");
    }

    #[tokio::test]
    async fn an_artifact_linking_to_itself_is_not_a_link() {
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        store
            .bump_link(&a, &a, 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        assert!(store.get_link(&a, &a).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_watermark_reads_back_what_was_written_and_nothing_otherwise() {
        let store = Store::memory().await.unwrap();
        assert_eq!(
            store.meta_get("associate.events_after").await.unwrap(),
            None
        );
        store
            .meta_set("associate.events_after", "42")
            .await
            .unwrap();
        store
            .meta_set("associate.events_after", "99")
            .await
            .unwrap();
        assert_eq!(
            store.meta_get("associate.events_after").await.unwrap(),
            Some("99".to_string())
        );
    }
}
