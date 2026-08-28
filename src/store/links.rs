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

/// How the two read-then-write transactions here open.
///
/// SQLite's default `BEGIN` is deferred: it takes no lock until the first
/// statement, so a transaction that reads and *then* writes holds a read
/// snapshot when it asks for the write lock. Under WAL, if another connection
/// committed in between, that upgrade fails immediately with
/// `SQLITE_BUSY_SNAPSHOT` — the one busy error `busy_timeout` does not retry,
/// because waiting cannot make a stale snapshot fresh.
///
/// Both `bump_link` and `bump_activation` are exactly that shape — read the
/// current weight, decay it, write it back — and both run from background
/// tasks while a sweep is writing the same tables from another connection in
/// the same pool. Taking the write lock up front makes the loser wait instead
/// of fail, which is what `busy_timeout` is for.
const IMMEDIATE: &str = "BEGIN IMMEDIATE";

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

/// Do two artifacts share no corpus? An artifact with no origins at all —
/// lineage lost — is treated as its own document, which keeps such a link
/// judgeable rather than silently shown-only.
fn origins_disjoint(
    origins: &std::collections::BTreeMap<String, Vec<crate::store::lineage::Origin>>,
    a: &str,
    b: &str,
) -> bool {
    let of = |id: &str| -> std::collections::BTreeSet<&str> {
        origins
            .get(id)
            .map(|o| o.iter().map(|x| x.corpus_id.as_str()).collect())
            .unwrap_or_default()
    };
    of(a).is_disjoint(&of(b))
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

/// One end of a link, seen from an anchor that is already in the result list.
#[derive(Debug, Clone)]
pub struct LinkedTo {
    /// The ranked hit that recalled it.
    pub via: String,
    pub other: String,
    /// Already decayed to the caller's clock. There is no stale number here.
    pub weight: f64,
    pub state: LinkState,
    pub reason: Option<String>,
    pub cues: Vec<Cue>,
    /// The two sides come from different documents — or one of them is a merge,
    /// which belongs to no document and always counts as differing.
    pub cross_corpus: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LinkCounts {
    pub total: i64,
    pub related: i64,
    /// Links whose judgement is queued or running.
    pub judge_queue: i64,
}

impl Store {
    /// Strengthen the link between two artifacts, folding the decay in.
    ///
    /// `cue` is the normalised query that bound them, where there is one. A
    /// bump with no cue — a confirmation replayed for an event whose
    /// co-appearance was already folded in — strengthens without claiming a new
    /// binding question.
    ///
    /// One transaction. Callers with a whole event's worth of pairs to bind
    /// should use `bump_links` rather than a loop over this — see its note on
    /// what a loop costs.
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
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        bump_one(&mut tx, a, b, delta, cue, half_life_days, at).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every pair of one event's shown candidates, in a single transaction.
    ///
    /// The count is quadratic in what the searcher saw — ten shown candidates
    /// are 45 pairs, and `search.limit` is capped at 50, which is 1225 — and
    /// each of these transactions is an exclusive one (see `IMMEDIATE`). Bound
    /// a pair at a time, a single catch-up sweep of `REPLAY_LIMIT` events could
    /// take the write lock and give it back a million times over, with every
    /// concurrent search's `bump_activation` queued behind it. Per event it is
    /// one.
    ///
    /// A pair that cannot be written is warned about and stepped over rather
    /// than failing the batch: one side deleted between the search and the
    /// sweep is the ordinary cause, and it says nothing about the rest of what
    /// that event showed. SQLite aborts the statement, not the transaction, so
    /// the pairs around it still commit.
    pub async fn bump_links(
        &self,
        pairs: &[(&str, &str)],
        delta: f64,
        cue: Option<&str>,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        for (a, b) in pairs {
            if a == b {
                continue;
            }
            let (a, b) = canonical(a, b);
            if let Err(err) = bump_one(&mut tx, a, b, delta, cue, half_life_days, at).await {
                // Named as what it is and not as a guess: one side being gone
                // is one cause, write contention under WAL is another, and an
                // operator sent looking for a deleted artifact by a line that
                // means "the database was busy" loses the afternoon. `warn`
                // rather than `debug` because the bump is dropped, not retried
                // — this is learning that did not happen.
                tracing::warn!(error = %err, a, b, "could not bind a pair");
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

/// One pair's bump, on a connection the caller has already opened a transaction
/// on. Shared by `bump_link` and `bump_links` so the two can never drift.
async fn bump_one(
    tx: &mut sqlx::SqliteConnection,
    a: &str,
    b: &str,
    delta: f64,
    cue: Option<&str>,
    half_life_days: f64,
    at: i64,
) -> Result<()> {
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
            let weight = decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days) + delta;
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
    Ok(())
}

impl Store {
    /// Every link naming `id` at either end, in any state.
    pub async fn links_touching(&self, id: &str) -> Result<Vec<Link>> {
        let rows = sqlx::query("SELECT * FROM artifact_links WHERE a_id = ? OR b_id = ?")
            .bind(id)
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_link).collect())
    }

    /// Copy a link that touches `old` onto `new` and the same other endpoint,
    /// leaving the original row where it is.
    ///
    /// Learned access carries forward; recorded history does not. Weight comes
    /// across decayed to `at` and stamped `at`; `queries` and `cues` come
    /// across as they are. On collision with a link `new` already has: the
    /// larger decayed weight (max, not sum — one search returning three
    /// passages of one section is one piece of evidence), the larger
    /// `queries`, the cues merged and cut to three, `dismissed` if either side
    /// was (the operator's "not related" is final), otherwise `learning` — a
    /// verdict passed on the passage's text does not transfer to a rewrite —
    /// and `judged_rev_*` cleared. A link whose other end *is* `new`, or that
    /// would pair `new` with itself, is skipped.
    pub async fn carry_link(
        &self,
        from: &Link,
        old: &str,
        new: &str,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        let other = if from.a_id == old {
            &from.b_id
        } else {
            &from.a_id
        };
        if other == new || other == old {
            return Ok(());
        }
        let (a, b) = canonical(new, other);
        let incoming = decayed(from.weight, from.bumped_at, at, half_life_days);
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let existing = sqlx::query("SELECT * FROM artifact_links WHERE a_id = ? AND b_id = ?")
            .bind(a)
            .bind(b)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| row_to_link(&r));
        match existing {
            None => {
                let state = if from.state == LinkState::Dismissed {
                    LinkState::Dismissed
                } else {
                    LinkState::Learning
                };
                sqlx::query(
                    "INSERT INTO artifact_links
                       (a_id, b_id, weight, bumped_at, queries, cues, state, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(a)
                .bind(b)
                .bind(incoming)
                .bind(at)
                .bind(from.queries)
                .bind(serde_json::to_string(&from.cues).unwrap_or_else(|_| "[]".into()))
                .bind(state.as_str())
                .bind(now())
                .execute(&mut *tx)
                .await?;
            }
            Some(have) => {
                let weight = decayed(have.weight, have.bumped_at, at, half_life_days).max(incoming);
                let mut cues = have.cues.clone();
                for c in &from.cues {
                    match cues.iter_mut().find(|x| x.q == c.q) {
                        Some(x) => x.n = x.n.max(c.n),
                        None => cues.push(c.clone()),
                    }
                }
                cues.sort_by_key(|x| std::cmp::Reverse(x.n));
                cues.truncate(3);
                let state =
                    if have.state == LinkState::Dismissed || from.state == LinkState::Dismissed {
                        LinkState::Dismissed
                    } else {
                        LinkState::Learning
                    };
                sqlx::query(
                    "UPDATE artifact_links
                        SET weight = ?, bumped_at = ?, queries = ?, cues = ?, state = ?,
                            reason = NULL, judged_rev_a = NULL, judged_rev_b = NULL
                      WHERE a_id = ? AND b_id = ?",
                )
                .bind(weight)
                .bind(at)
                .bind(have.queries.max(from.queries))
                .bind(serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()))
                .bind(state.as_str())
                .bind(a)
                .bind(b)
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

    /// Every live link out of these anchors, strongest first, capped at
    /// `limit`.
    ///
    /// One statement per anchor rather than one `IN` over all of them: the
    /// anchor list is `spread_from` long (three), the repo builds no SQL
    /// strings from data, and a link has to be found from either end — which is
    /// an `OR` over two indexed columns, not a set membership test.
    ///
    /// The endpoint's status is joined rather than stored, so undoing a merge
    /// brings its links back without a write.
    ///
    /// Same two-step as `links_to_judge`: SQL fetches four times `limit` so
    /// that rows the exact decayed-weight test in Rust rejects do not eat the
    /// budget, and the final truncate is what actually bounds a hub artifact
    /// — one co-shown with many others — to a fixed, small read on every
    /// search rather than every link above `show_min`.
    pub async fn links_from(
        &self,
        anchors: &[String],
        states: &[LinkState],
        half_life_days: f64,
        at: i64,
        min_weight: f64,
        limit: i64,
    ) -> Result<Vec<LinkedTo>> {
        let allowed: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let mut out: Vec<LinkedTo> = Vec::new();
        for anchor in anchors {
            let rows = sqlx::query(
                // `weight >= ?` is a necessary condition, not the answer: decay
                // only ever lowers it, so this narrows the scan with the index
                // and the exact test happens below in Rust. Doing it in SQL
                // would need a `pow` that is not in every SQLite build.
                "SELECT l.a_id AS a_id, l.b_id AS b_id, l.weight AS weight,
                        l.bumped_at AS bumped_at, l.state AS state, l.reason AS reason,
                        l.cues AS cues,
                        a.corpus_id AS a_corpus, b.corpus_id AS b_corpus
                   FROM artifact_links l
                   JOIN artifacts a ON a.id = l.a_id
                   JOIN artifacts b ON b.id = l.b_id
                  WHERE (l.a_id = ? OR l.b_id = ?)
                    AND l.weight >= ?
                    AND a.status = 'active' AND a.superseded_by IS NULL
                    AND b.status = 'active' AND b.superseded_by IS NULL
                  ORDER BY l.weight DESC LIMIT ?",
            )
            .bind(anchor)
            .bind(anchor)
            .bind(min_weight)
            // SQLite reads a negative LIMIT as unbounded, so the cap must be
            // clamped at zero — a negative `limit` must still cap the fetch.
            .bind(limit.max(0).saturating_mul(4))
            .fetch_all(&self.pool)
            .await?;

            for r in &rows {
                let state = LinkState::parse(r.get::<String, _>("state").as_str());
                // The operator's "not related" is final. It does not depend on
                // callers remembering to leave `Dismissed` out of `states` — a
                // caller that could opt back into it would make the control a
                // suggestion, not an invariant.
                if state == LinkState::Dismissed || !allowed.contains(&state.as_str()) {
                    continue;
                }
                let weight = decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days);
                if weight < min_weight {
                    continue;
                }
                let a_id: String = r.get("a_id");
                let b_id: String = r.get("b_id");
                let other = if &a_id == anchor { b_id } else { a_id };
                let a_corpus: Option<String> = r.get("a_corpus");
                let b_corpus: Option<String> = r.get("b_corpus");
                out.push(LinkedTo {
                    via: anchor.clone(),
                    other,
                    weight,
                    state,
                    reason: r.get("reason"),
                    cues: serde_json::from_str(&r.get::<String, _>("cues")).unwrap_or_default(),
                    // Decided below through origins where either side is a
                    // merge; here only the case both rows answer for
                    // themselves.
                    cross_corpus: match (a_corpus, b_corpus) {
                        (Some(x), Some(y)) => x != y,
                        _ => true,
                    },
                });
            }
        }
        out.sort_by(|x, y| y.weight.total_cmp(&x.weight));
        out.truncate(limit.max(0) as usize);
        // A merged or synthesized artifact belongs to every corpus it drew
        // from, so "the same document" is "the two origin sets intersect" —
        // read through lineage, one batched query for the page of links.
        let mut ids: Vec<String> = out
            .iter()
            .flat_map(|l| [l.via.clone(), l.other.clone()])
            .collect();
        ids.sort();
        ids.dedup();
        let origins = self.origins_of(&ids).await?;
        for l in &mut out {
            l.cross_corpus = origins_disjoint(&origins, &l.via, &l.other);
        }
        Ok(out)
    }

    /// Links strong enough, various enough, live enough and cross-corpus enough
    /// to be worth one model call. Strongest first.
    ///
    /// Same two-step as `links_from`: the raw weight narrows with the index and
    /// the decayed weight decides. Four times the caller's limit is fetched so
    /// that rows failing the exact test do not eat the budget. Cross-corpus is
    /// decided in Rust over origins rather than in SQL over `corpus_id`: a
    /// merge has none of its own and belongs to every corpus it drew from.
    pub async fn links_to_judge(
        &self,
        min_weight: f64,
        min_queries: i64,
        half_life_days: f64,
        at: i64,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let rows = sqlx::query(
            "SELECT l.* FROM artifact_links l
               JOIN artifacts a ON a.id = l.a_id
               JOIN artifacts b ON b.id = l.b_id
              WHERE l.state = 'learning'
                AND l.weight >= ? AND l.queries >= ?
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL
              ORDER BY l.weight DESC LIMIT ?",
        )
        .bind(min_weight)
        .bind(min_queries)
        // SQLite reads a negative LIMIT as unbounded, so the cap must be
        // clamped at zero — a negative `limit` must still cap the fetch, not
        // remove it, even though `take(limit.max(0))` below empties it anyway.
        .bind(limit.max(0).saturating_mul(4))
        .fetch_all(&self.pool)
        .await?;

        let strong: Vec<Link> = rows
            .iter()
            .map(row_to_link)
            .filter(|l| decayed(l.weight, l.bumped_at, at, half_life_days) >= min_weight)
            .collect();
        // Two passages of one document being related is not information, so a
        // same-document link is shown and never judged — "same document" read
        // through origins, so a merge is the documents it drew from.
        let mut ids: Vec<String> = strong
            .iter()
            .flat_map(|l| [l.a_id.clone(), l.b_id.clone()])
            .collect();
        ids.sort();
        ids.dedup();
        let origins = self.origins_of(&ids).await?;
        Ok(strong
            .into_iter()
            .filter(|l| origins_disjoint(&origins, &l.a_id, &l.b_id))
            .take(limit.max(0) as usize)
            .collect())
    }

    /// Delete `learning` links that have faded below the floor.
    ///
    /// The only write a quiet link ever costs. Rows whose *stored* weight is
    /// already under the floor go in one statement; the rest are read back and
    /// tested exactly, capped at `scan_limit` so one sweep cannot walk an
    /// unbounded table. Whatever the cap leaves is pruned by the next sweep.
    pub async fn prune_learning_links(
        &self,
        below: f64,
        half_life_days: f64,
        at: i64,
        scan_limit: i64,
    ) -> Result<u64> {
        let mut dropped =
            sqlx::query("DELETE FROM artifact_links WHERE state = 'learning' AND weight < ?")
                .bind(below)
                .execute(&self.pool)
                .await?
                .rows_affected();

        let rows = sqlx::query(
            "SELECT a_id, b_id, weight, bumped_at FROM artifact_links
              WHERE state = 'learning' ORDER BY bumped_at ASC LIMIT ?",
        )
        .bind(scan_limit)
        .fetch_all(&self.pool)
        .await?;
        for r in &rows {
            if decayed(r.get("weight"), r.get("bumped_at"), at, half_life_days) >= below {
                continue;
            }
            dropped += sqlx::query("DELETE FROM artifact_links WHERE a_id = ? AND b_id = ?")
                .bind(r.get::<String, _>("a_id"))
                .bind(r.get::<String, _>("b_id"))
                .execute(&self.pool)
                .await?
                .rows_affected();
        }
        if rows.len() as i64 == scan_limit {
            tracing::info!(
                scan_limit,
                "prune scan hit its cap; the rest waits for the next sweep"
            );
        }
        Ok(dropped)
    }

    /// Put judged links back to `learning` where either side has been re-embedded
    /// since. The judge read text that no longer exists.
    pub async fn reopen_stale_judged_links(&self, limit: i64) -> Result<u64> {
        Ok(sqlx::query(
            // `judge_attempts` is deliberately left alone here. A link shelved
            // as `unrelated`/"unreadable" after three attempts and then
            // reopened by a text change gets exactly one more call before
            // `judge` (src/jobs/associate.rs) would shelve it again at the
            // same ceiling — the model has already failed three times on this
            // pair, and that history is not something a re-embed should erase.
            "UPDATE artifact_links
                SET state = 'learning', reason = NULL,
                    judged_rev_a = NULL, judged_rev_b = NULL
              WHERE (a_id, b_id) IN (
                SELECT l.a_id, l.b_id FROM artifact_links l
                  JOIN artifacts a ON a.id = l.a_id
                  JOIN artifacts b ON b.id = l.b_id
                 WHERE l.state IN ('related', 'unrelated')
                   AND (a.embed_rev IS NOT l.judged_rev_a OR b.embed_rev IS NOT l.judged_rev_b)
                 LIMIT ?
              )",
        )
        .bind(limit)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Record a verdict. `judged_revs` is `(embed_rev of a, embed_rev of b)` as
    /// read by whoever judged, which is what `reopen_stale_judged_links`
    /// compares against later.
    pub async fn set_link_state(
        &self,
        a: &str,
        b: &str,
        state: LinkState,
        reason: Option<&str>,
        judged_revs: Option<(i64, i64)>,
    ) -> Result<()> {
        let (a, b) = canonical(a, b);
        let (rev_a, rev_b) = match judged_revs {
            Some((x, y)) => (Some(x), Some(y)),
            None => (None, None),
        };
        sqlx::query(
            "UPDATE artifact_links
                SET state = ?, reason = ?, judged_rev_a = ?, judged_rev_b = ?
              WHERE a_id = ? AND b_id = ?",
        )
        .bind(state.as_str())
        .bind(reason)
        .bind(rev_a)
        .bind(rev_b)
        .bind(a)
        .bind(b)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The operator saying this pair does not belong together.
    ///
    /// Deliberately narrower than `set_link_state`: dismissal records a
    /// decision, it does not erase what the decision was made about. Weight
    /// stays so the decision is auditable against the evidence that produced
    /// it, and the judge's `reason` — where there is one — stays too, because
    /// it is part of that evidence: an operator overruling "the config and the
    /// errors it produces" should leave a row that still says both what was
    /// claimed and that a person overruled it. Only `state` moves.
    pub async fn dismiss_link(&self, a: &str, b: &str) -> Result<()> {
        let (a, b) = canonical(a, b);
        sqlx::query("UPDATE artifact_links SET state = ? WHERE a_id = ? AND b_id = ?")
            .bind(LinkState::Dismissed.as_str())
            .bind(a)
            .bind(b)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Count one call against a link and report the new total.
    ///
    /// Counted only where the answer said something about the *link* — an
    /// unreadable reply. A call an outage ate says something about the endpoint,
    /// and shelving a link for that would empty the pane every time the model is
    /// down.
    pub async fn record_link_judge_attempt(&self, a: &str, b: &str) -> Result<i64> {
        let (a, b) = canonical(a, b);
        Ok(sqlx::query_scalar(
            "UPDATE artifact_links SET judge_attempts = judge_attempts + 1
              WHERE a_id = ? AND b_id = ? RETURNING judge_attempts",
        )
        .bind(a)
        .bind(b)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0))
    }

    /// Each artifact's stored activation, the stamp it was true at, and when
    /// the artifact arrived.
    ///
    /// `created_at` is here because the stored number is a sum of two things
    /// that decay at the same rate: the capture baseline, which every artifact
    /// carries from the moment it exists, and what use has added since. Telling
    /// them apart at read time needs the baseline's own age, and `activated_at`
    /// is not it — a bump moves that stamp forward and the baseline's age does
    /// not move with it.
    ///
    /// One statement for the whole candidate list: this is on the query path,
    /// and fifty round trips to answer one search is exactly the layer crossing
    /// the design promises not to be. The SQL is built only from `?`
    /// placeholders — one per id, never a value — so nothing from a request
    /// reaches the statement text.
    pub async fn activation_of(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (f64, i64, i64)>> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let holes = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, activation, activated_at, created_at FROM artifacts WHERE id IN ({holes})"
        )));
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    (
                        r.get::<f64, _>("activation"),
                        r.get::<i64, _>("activated_at"),
                        r.get::<i64, _>("created_at"),
                    ),
                )
            })
            .collect())
    }

    /// Raise the accessibility of these artifacts, folding the decay in.
    ///
    /// Read-then-write per artifact rather than one arithmetic UPDATE, for the
    /// same reason `bump_link` does it: the decay is an exponential, and not
    /// every SQLite build ships the math functions to express one in SQL.
    ///
    /// One transaction per artifact, the same shape as `bump_link`: the
    /// sweep's confirmed bump and a search's retrieval bump can interleave on
    /// the same artifact, as can two concurrent searches sharing a hit, and
    /// without a transaction the loser's read is stale by the time it writes,
    /// silently dropping its delta. An id with no row is skipped, same as
    /// before.
    pub async fn bump_activation(
        &self,
        ids: &[String],
        delta: f64,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        if ids.is_empty() || delta == 0.0 {
            return Ok(());
        }
        for id in ids {
            let mut tx = self.pool.begin_with(IMMEDIATE).await?;
            let row = sqlx::query("SELECT activation, activated_at FROM artifacts WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            let Some(row) = row else {
                continue;
            };
            let value: f64 = row.get("activation");
            let stamp: i64 = row.get("activated_at");
            sqlx::query("UPDATE artifacts SET activation = ?, activated_at = ? WHERE id = ?")
                .bind(decayed(value, stamp, at, half_life_days) + delta)
                .bind(at)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// The three numbers Ops shows.
    pub async fn link_counts(&self) -> Result<LinkCounts> {
        Ok(LinkCounts {
            total: sqlx::query_scalar("SELECT COUNT(*) FROM artifact_links")
                .fetch_one(&self.pool)
                .await?,
            related: sqlx::query_scalar(
                "SELECT COUNT(*) FROM artifact_links WHERE state = 'related'",
            )
            .fetch_one(&self.pool)
            .await?,
            judge_queue: self
                .control
                .live_count(&self.subject, crate::store::jobs::Stage::LinkJudge)
                .await?,
        })
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

    /// Two artifacts in two different corpora, so a link between them is a
    /// cross-corpus one — the only kind the judge is ever asked about.
    async fn two_corpora(store: &Store) -> (String, String) {
        let mut ids = Vec::new();
        for (raw, text) in [("raw one", "alpha"), ("raw two", "beta")] {
            let src = store.insert_corpus(raw, "web", None).await.unwrap();
            let made = store
                .insert_artifacts(
                    &src.id,
                    &[NewArtifact {
                        ordinal: 0,
                        text: text.into(),
                        corpus_span: None,
                        title: Some(text.into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    }],
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        (ids[0].clone(), ids[1].clone())
    }

    #[tokio::test]
    async fn a_link_is_found_from_either_of_its_ends() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 3.0, Some("q"), 30.0, 0)
            .await
            .unwrap();

        for anchor in [&a, &b] {
            let out = store
                .links_from(
                    std::slice::from_ref(anchor),
                    &[LinkState::Learning],
                    30.0,
                    0,
                    2.0,
                    10,
                )
                .await
                .unwrap();
            assert_eq!(out.len(), 1, "anchored at {anchor}");
            assert_eq!(&out[0].via, anchor);
            assert_ne!(&out[0].other, anchor);
        }
    }

    #[tokio::test]
    async fn a_link_below_the_threshold_once_decayed_is_not_shown() {
        // The stored weight is strength as of `bumped_at`. Filtering on it
        // directly would keep showing a link that has not been used in a year.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 3.0, Some("q"), 30.0, 0)
            .await
            .unwrap();

        let now = store
            .links_from(
                std::slice::from_ref(&a),
                &[LinkState::Learning],
                30.0,
                0,
                2.0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(now.len(), 1);
        let later = store
            .links_from(
                std::slice::from_ref(&a),
                &[LinkState::Learning],
                30.0,
                60 * 86_400,
                2.0,
                10,
            )
            .await
            .unwrap();
        assert!(later.is_empty(), "a link decayed to 0.75 was still shown");
    }

    #[tokio::test]
    async fn a_link_whose_other_side_is_hidden_is_not_shown() {
        // Superseded and deprecated endpoints are filtered at read time, so
        // undoing a merge brings its links back without a write.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 3.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        assert!(
            store
                .links_from(&[a], &[LinkState::Learning], 30.0, 0, 2.0, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_dismissed_link_is_never_returned() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 9.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_link_state(&a, &b, LinkState::Dismissed, None, None)
            .await
            .unwrap();

        assert!(
            store
                .links_from(
                    &[a],
                    &[LinkState::Learning, LinkState::Related],
                    30.0,
                    0,
                    0.0,
                    10
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn dismissing_a_link_records_the_decision_without_erasing_the_evidence() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 9.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_link_state(
                &a,
                &b,
                LinkState::Related,
                Some("the config and the errors it produces"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        store.dismiss_link(&a, &b).await.unwrap();

        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Dismissed);
        assert_eq!(
            l.weight, 9.0,
            "the evidence was thrown away with the decision"
        );
        assert_eq!(
            l.reason.as_deref(),
            Some("the config and the errors it produces"),
            "the judge's assessment is part of the evidence, not decoration"
        );
    }

    #[tokio::test]
    async fn a_dismissed_link_stays_hidden_even_from_a_caller_that_asks_for_it() {
        // The operator's decision is final, and an invariant that holds only
        // while every call site remembers the right argument is not one.
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 9.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_link_state(&a, &b, LinkState::Dismissed, None, None)
            .await
            .unwrap();

        assert!(
            store
                .links_from(
                    &[a],
                    &[LinkState::Dismissed, LinkState::Learning],
                    30.0,
                    0,
                    0.0,
                    10
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn only_a_strong_cross_corpus_link_is_offered_to_the_judge() {
        // Two passages of one document being related is not information, so a
        // same-corpus link is shown and never judged.
        let store = Store::memory().await.unwrap();
        let (same_a, same_b) = two(&store).await;
        let (cross_a, cross_b) = two_corpora(&store).await;
        for (a, b) in [(&same_a, &same_b), (&cross_a, &cross_b)] {
            for q in ["one", "two", "three"] {
                store.bump_link(a, b, 2.0, Some(q), 30.0, 0).await.unwrap();
            }
        }

        let armed = store.links_to_judge(4.0, 3, 30.0, 0, 10).await.unwrap();
        assert_eq!(armed.len(), 1, "got {armed:?}");
        assert_eq!(
            canonical(&cross_a, &cross_b),
            (armed[0].a_id.as_str(), armed[0].b_id.as_str())
        );
    }

    #[tokio::test]
    async fn a_link_bound_by_too_few_questions_is_not_judged() {
        // One question asked six times is one question. The judge is the only
        // thing here that costs a model call, and this is what bounds it.
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        for _ in 0..6 {
            store
                .bump_link(&a, &b, 1.0, Some("same"), 30.0, 0)
                .await
                .unwrap();
        }
        assert!(
            store
                .links_to_judge(4.0, 3, 30.0, 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pruning_drops_faded_learning_links_and_spares_judged_ones() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        let (c, d) = two_corpora(&store).await;
        store
            .bump_link(&a, &b, 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .bump_link(&c, &d, 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_link_state(
                &c,
                &d,
                LinkState::Related,
                Some("both about disks"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        // A year on, both have decayed to almost nothing.
        let dropped = store
            .prune_learning_links(0.5, 30.0, 365 * 86_400, 5_000)
            .await
            .unwrap();
        assert_eq!(dropped, 1);
        assert!(store.get_link(&a, &b).await.unwrap().is_none());
        assert!(
            store.get_link(&c, &d).await.unwrap().is_some(),
            "a verified relation is about content, not use"
        );
    }

    #[tokio::test]
    async fn a_judged_link_reopens_when_either_text_changes_under_it() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_link_state(&a, &b, LinkState::Related, Some("a reason"), Some((0, 0)))
            .await
            .unwrap();

        assert_eq!(store.reopen_stale_judged_links(100).await.unwrap(), 0);

        store
            .update_artifact_text(&a, "alpha, rewritten")
            .await
            .unwrap();
        assert_eq!(store.reopen_stale_judged_links(100).await.unwrap(), 1);
        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Learning);
        assert_eq!(l.reason, None, "the judge read text that no longer exists");
    }

    #[tokio::test]
    async fn the_counts_say_how_many_links_there_are_and_how_many_are_named() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        let (c, d) = two_corpora(&store).await;
        store
            .bump_link(&a, &b, 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .bump_link(&c, &d, 9.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store
            .set_link_state(&c, &d, LinkState::Related, Some("why"), Some((0, 0)))
            .await
            .unwrap();

        let n = store.link_counts().await.unwrap();
        assert_eq!((n.total, n.related), (2, 1));
    }

    #[tokio::test]
    async fn a_judge_attempt_is_counted_and_reported_back() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two(&store).await;
        store
            .bump_link(&a, &b, 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        assert_eq!(store.record_link_judge_attempt(&a, &b).await.unwrap(), 1);
        assert_eq!(store.record_link_judge_attempt(&b, &a).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_fresh_artifact_starts_fully_activated_and_stamped() {
        // Left unstamped, every artifact in the base reads as having decayed
        // since 1970 — equally inaccessible, which is the opposite of true.
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        let act = store.activation_of(std::slice::from_ref(&a)).await.unwrap();
        let (value, stamp, created_at) = act
            .get(&a)
            .copied()
            .expect("an artifact carries activation");
        assert!((value - 1.0).abs() < 1e-9);
        assert!(stamp > 0, "activated_at was never set at insert");
        // The baseline's own age, which a bump moves `activated_at` away from
        // and must not move: at insert the two agree.
        assert_eq!(created_at, stamp);
    }

    #[tokio::test]
    async fn a_bump_folds_the_decay_in_like_a_link_does() {
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        sqlx::query("UPDATE artifacts SET activation = 4.0, activated_at = 0 WHERE id = ?")
            .bind(&a)
            .execute(&store.pool)
            .await
            .unwrap();

        store
            .bump_activation(std::slice::from_ref(&a), 1.0, 14.0, 14 * 86_400)
            .await
            .unwrap();

        let (value, stamp, _) = store.activation_of(std::slice::from_ref(&a)).await.unwrap()[&a];
        assert!((value - 3.0).abs() < 1e-6, "value was {value}");
        assert_eq!(stamp, 14 * 86_400);
    }

    #[tokio::test]
    async fn bumping_nothing_is_not_a_write() {
        let store = Store::memory().await.unwrap();
        store.bump_activation(&[], 1.0, 14.0, 0).await.unwrap();
        assert!(store.activation_of(&[]).await.unwrap().is_empty());
    }

    /// An artifact in a corpus of its own, for a link to be carried onto.
    async fn third(store: &Store) -> String {
        let src = store.insert_corpus("raw2", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "the artifact".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone()
    }

    #[tokio::test]
    async fn carrying_a_link_copies_it_and_leaves_the_original() {
        let s = Store::memory().await.unwrap();
        let (p, x) = two(&s).await; // passage p, some artifact x
        let a = third(&s).await;
        s.bump_links(
            &[(p.as_str(), x.as_str())],
            2.0,
            Some("how to mount"),
            14.0,
            1_000,
        )
        .await
        .unwrap();
        let from = s.get_link(&p, &x).await.unwrap().unwrap();

        s.carry_link(&from, &p, &a, 14.0, 1_000 + 14 * 86_400)
            .await
            .unwrap();

        let copied = s.get_link(&a, &x).await.unwrap().expect("the copy exists");
        // Decayed to the carry moment: one half-life later, half the weight.
        assert!((copied.weight - 1.0).abs() < 1e-6, "{}", copied.weight);
        assert_eq!(copied.bumped_at, 1_000 + 14 * 86_400);
        assert_eq!(copied.queries, from.queries);
        assert_eq!(copied.cues.len(), 1);
        assert_eq!(copied.state, LinkState::Learning);
        assert!(copied.judged_rev_a.is_none() && copied.judged_rev_b.is_none());
        // The original is still there, untouched.
        let orig = s.get_link(&p, &x).await.unwrap().unwrap();
        assert_eq!(orig.weight, from.weight);
    }

    #[tokio::test]
    async fn carrying_onto_an_existing_link_takes_the_max_not_the_sum_and_dismissed_wins() {
        let s = Store::memory().await.unwrap();
        let (p, x) = two(&s).await;
        let a = third(&s).await;
        let at = 5_000;
        s.bump_links(&[(p.as_str(), x.as_str())], 3.0, Some("q one"), 14.0, at)
            .await
            .unwrap();
        s.bump_links(&[(a.as_str(), x.as_str())], 2.0, Some("q two"), 14.0, at)
            .await
            .unwrap();
        // The operator dismissed the artifact's own link.
        s.set_link_state(&a, &x, LinkState::Dismissed, None, None)
            .await
            .unwrap();
        let from = s.get_link(&p, &x).await.unwrap().unwrap();

        s.carry_link(&from, &p, &a, 14.0, at).await.unwrap();

        let merged = s.get_link(&a, &x).await.unwrap().unwrap();
        assert!(
            (merged.weight - 3.0).abs() < 1e-6,
            "max, not 5.0: {}",
            merged.weight
        );
        assert_eq!(merged.queries, 1, "max of 1 and 1");
        assert_eq!(merged.cues.len(), 2, "cues merged");
        assert_eq!(
            merged.state,
            LinkState::Dismissed,
            "the operator's no is final"
        );
    }

    #[tokio::test]
    async fn links_touching_finds_a_link_from_either_end() {
        let s = Store::memory().await.unwrap();
        let (p, x) = two(&s).await;
        s.bump_links(&[(p.as_str(), x.as_str())], 1.0, None, 14.0, 1)
            .await
            .unwrap();
        assert_eq!(s.links_touching(&p).await.unwrap().len(), 1);
        assert_eq!(s.links_touching(&x).await.unwrap().len(), 1);
        assert!(s.links_touching("nobody").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cross_corpus_is_read_through_origins_for_a_merge() {
        // A merge of two artifacts from corpus 1, linked to a third artifact of
        // corpus 1: same document, however the merge's own `corpus_id` reads.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("one", "web", None).await.unwrap();
        let na = |o: i64, t: &str| NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        };
        let made = store
            .insert_artifacts(&src.id, &[na(0, "a"), na(1, "b"), na(2, "c")])
            .await
            .unwrap();
        let m = store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "a and b".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[made[0].id.clone(), made[1].id.clone()],
            )
            .await
            .unwrap();
        let other_src = store.insert_corpus("two", "web", None).await.unwrap();
        let far = store
            .insert_artifacts(&other_src.id, &[na(0, "far")])
            .await
            .unwrap()[0]
            .id
            .clone();
        for q in ["one", "two", "three"] {
            store
                .bump_link(&m.id, &made[2].id, 2.0, Some(q), 30.0, 0)
                .await
                .unwrap();
            store
                .bump_link(&m.id, &far, 2.0, Some(q), 30.0, 0)
                .await
                .unwrap();
        }
        let out = store
            .links_from(
                std::slice::from_ref(&m.id),
                &[LinkState::Learning],
                30.0,
                0,
                0.0,
                10,
            )
            .await
            .unwrap();
        let same = out.iter().find(|l| l.other == made[2].id).unwrap();
        let cross = out.iter().find(|l| l.other == far).unwrap();
        assert!(!same.cross_corpus, "a merge of corpus-1 roots is corpus 1");
        assert!(cross.cross_corpus);
        // And the judge is offered only the cross-corpus one.
        let armed = store.links_to_judge(4.0, 3, 30.0, 0, 10).await.unwrap();
        assert_eq!(armed.len(), 1, "{armed:?}");
        assert!(armed[0].a_id == far || armed[0].b_id == far);
    }
}
