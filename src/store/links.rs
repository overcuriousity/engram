//! Artifacts needed together, as opposed to `artifact_pairs` (saying the same thing).
//! Strengths are stored with their stamp and read through `decayed`; nothing decays in place.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Three is what the pane shows; `queries` counts distinct cues beyond it.
pub const MAX_CUES: usize = 3;

/// A deferred `BEGIN` that reads then writes fails with `SQLITE_BUSY_SNAPSHOT`, which
/// `busy_timeout` does not retry; taking the write lock up front makes the loser wait.
const IMMEDIATE: &str = "BEGIN IMMEDIATE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Bound by use, nothing has ruled on it. Shown, prunable, judgeable.
    Learning,
    /// Never pruned by decay: a verified relation is about content, not use.
    Related,
    /// Kept so it is not asked again; reopened only if either text changes.
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

impl Store {
    /// Lands `related`; `dismissed` stays unbeatable and keeps its weight. Judged revisions must
    /// be stamped or `reopen_stale_judged_links` demotes it next sweep. `weight` must clear
    /// `associate.show_min` with room to decay; the conflict branch folds decay in before max.
    pub async fn relate_synthesized(
        &self,
        a: &str,
        b: &str,
        reason: &str,
        weight: f64,
        half_life_days: f64,
    ) -> Result<()> {
        let (a, b) = canonical(a, b);
        let at = now();
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let existing = sqlx::query!(
            "SELECT weight, bumped_at, state FROM artifact_links
                                     WHERE a_id = ? AND b_id = ?",
            a,
            b
        )
        .fetch_optional(&mut *tx)
        .await?;
        // Read inside the write's transaction, so the decay folds from the number being replaced.
        let dismissed = existing
            .as_ref()
            .is_some_and(|r| LinkState::parse(r.state.as_str()) == LinkState::Dismissed);
        let carried = existing
            .as_ref()
            .map(|r| decayed(r.weight, r.bumped_at, at, half_life_days))
            .unwrap_or(0.0);
        sqlx::query!(
            "INSERT INTO artifact_links
                   (a_id, b_id, weight, bumped_at, queries, cues, state, reason,
                    judged_rev_a, judged_rev_b, created_at)
             VALUES (?, ?, ?, ?, 0, '[]', 'related', ?,
                     (SELECT embed_rev FROM artifacts WHERE id = ?),
                     (SELECT embed_rev FROM artifacts WHERE id = ?), ?)
             ON CONFLICT(a_id, b_id) DO UPDATE SET
               weight = CASE WHEN artifact_links.state = 'dismissed'
                             THEN artifact_links.weight ELSE excluded.weight END,
               bumped_at = CASE WHEN artifact_links.state = 'dismissed'
                                THEN artifact_links.bumped_at ELSE excluded.bumped_at END,
               state = CASE WHEN artifact_links.state = 'dismissed'
                            THEN artifact_links.state ELSE 'related' END,
               reason = CASE WHEN artifact_links.state = 'dismissed'
                             THEN artifact_links.reason ELSE excluded.reason END,
               judged_rev_a = CASE WHEN artifact_links.state = 'dismissed'
                                   THEN artifact_links.judged_rev_a
                                   ELSE excluded.judged_rev_a END,
               judged_rev_b = CASE WHEN artifact_links.state = 'dismissed'
                                   THEN artifact_links.judged_rev_b
                                   ELSE excluded.judged_rev_b END",
            a,
            b,
            if dismissed {
                carried
            } else {
                weight.max(carried)
            },
            at,
            reason,
            a,
            b,
            at
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// `a_id < b_id` is a CHECK, so the order cannot be forgotten at a call site.
pub fn canonical<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

pub fn normalize_query(q: &str) -> String {
    q.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// An artifact with no origins counts as its own document, so the link stays judgeable.
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

/// `half_life_days <= 0` turns decay off. Elapsed time is floored at zero so a clock moving
/// backwards cannot grow a weight.
pub fn decayed(weight: f64, bumped_at: i64, at: i64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return weight;
    }
    let elapsed = (at - bumped_at).max(0) as f64;
    weight * 2f64.powf(-elapsed / (half_life_days * 86_400.0))
}

/// One definition: priming, promotion and the insights page must agree on it.
pub const ACTIVATION_BASELINE: f64 = 1.0;

/// The baseline is subtracted decayed to now from `created_at`, not as a flat `1.0`: both
/// terms decay at the same rate. `stamp` moves with bumps; the baseline's age does not.
pub fn engagement_at(value: f64, stamp: i64, created_at: i64, at: i64, half_life_days: f64) -> f64 {
    let now = decayed(value, stamp, at, half_life_days);
    let baseline = decayed(ACTIVATION_BASELINE, created_at, at, half_life_days);
    (now - baseline).max(0.0)
}

#[derive(sqlx::FromRow)]
pub(crate) struct LinkRow {
    pub a_id: String,
    pub b_id: String,
    pub weight: f64,
    pub bumped_at: i64,
    pub queries: i64,
    pub cues: String,
    pub state: String,
    pub reason: Option<String>,
    pub judged_rev_a: Option<i64>,
    pub judged_rev_b: Option<i64>,
    pub judge_attempts: i64,
    pub created_at: i64,
}

pub(crate) fn row_to_link(r: LinkRow) -> Link {
    Link {
        a_id: r.a_id,
        b_id: r.b_id,
        weight: r.weight,
        bumped_at: r.bumped_at,
        queries: r.queries,
        cues: serde_json::from_str(&r.cues).unwrap_or_default(),
        state: LinkState::parse(r.state.as_str()),
        reason: r.reason,
        judged_rev_a: r.judged_rev_a,
        judged_rev_b: r.judged_rev_b,
        judge_attempts: r.judge_attempts,
        created_at: r.created_at,
    }
}

/// Only `MAX_CUES` survive, so `queries` is a floor on distinct cues, not an exact count.
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

#[derive(Debug, Clone)]
pub struct LinkedTo {
    pub via: String,
    pub other: String,
    /// Already decayed to the caller's clock. There is no stale number here.
    pub weight: f64,
    pub state: LinkState,
    pub reason: Option<String>,
    pub cues: Vec<Cue>,
    /// A merge belongs to no document and always counts as differing.
    pub cross_corpus: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LinkCounts {
    pub total: i64,
    pub related: i64,
    pub judge_queue: i64,
}

impl Store {
    /// `cue: None` strengthens without claiming a new binding question. For many pairs use
    /// `bump_links`.
    pub async fn bump_link(
        &self,
        a: &str,
        b: &str,
        delta: f64,
        cue: Option<&str>,
        half_life_days: f64,
        at: i64,
    ) -> Result<()> {
        // The CHECK would refuse it; caught here so callers enumerating pairs need not.
        if a == b {
            return Ok(());
        }
        let (a, b) = canonical(a, b);
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        bump_one(&mut tx, a, b, delta, cue, half_life_days, at).await?;
        tx.commit().await?;
        Ok(())
    }

    /// One exclusive transaction per event, not per pair: up to 1225 pairs each. A pair that
    /// fails is warned and skipped; SQLite aborts the statement, not the transaction.
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
                // Not "deleted": WAL contention is another cause. `warn`: the bump is dropped.
                tracing::warn!(error = %err, a, b, "could not bind a pair");
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Shared by `bump_link` and `bump_links` so the two cannot drift.
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
            sqlx::query!(
                "UPDATE artifact_links
                        SET weight = ?, bumped_at = ?, queries = ?, cues = ?
                      WHERE a_id = ? AND b_id = ?",
                weight,
                at,
                r.get::<i64, _>("queries") + i64::from(fresh),
                serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()),
                a,
                b
            )
            .execute(&mut *tx)
            .await?;
        }
        None => {
            let mut cues = Vec::new();
            if let Some(q) = cue {
                bump_cue(&mut cues, q);
            }
            sqlx::query!(
                "INSERT INTO artifact_links
                       (a_id, b_id, weight, bumped_at, queries, cues, state, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, 'learning', ?)",
                a,
                b,
                delta,
                at,
                cues.len() as i64,
                serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()),
                now()
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(())
}

impl Store {
    pub async fn links_touching(&self, id: &str) -> Result<Vec<Link>> {
        let rows = sqlx::query_as!(
            crate::store::links::LinkRow,
            "SELECT * FROM artifact_links WHERE a_id = ? OR b_id = ?",
            id,
            id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_link).collect())
    }

    /// Weight carries decayed to `at`; on collision take the max (not sum), merge cues,
    /// `dismissed` wins, otherwise `learning`, and `judged_rev_*` cleared. Self-pairs are skipped.
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
        let existing = sqlx::query_as!(
            crate::store::links::LinkRow,
            "SELECT * FROM artifact_links WHERE a_id = ? AND b_id = ?",
            a,
            b
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(row_to_link);
        match existing {
            None => {
                let state = if from.state == LinkState::Dismissed {
                    LinkState::Dismissed
                } else {
                    LinkState::Learning
                };
                sqlx::query!(
                    "INSERT INTO artifact_links
                       (a_id, b_id, weight, bumped_at, queries, cues, state, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    a,
                    b,
                    incoming,
                    at,
                    from.queries,
                    serde_json::to_string(&from.cues).unwrap_or_else(|_| "[]".into()),
                    state.as_str(),
                    now()
                )
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
                sqlx::query!(
                    "UPDATE artifact_links
                        SET weight = ?, bumped_at = ?, queries = ?, cues = ?, state = ?,
                            reason = NULL, judged_rev_a = NULL, judged_rev_b = NULL
                      WHERE a_id = ? AND b_id = ?",
                    weight,
                    at,
                    have.queries.max(from.queries),
                    serde_json::to_string(&cues).unwrap_or_else(|_| "[]".into()),
                    state.as_str(),
                    a,
                    b
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_link(&self, a: &str, b: &str) -> Result<Option<Link>> {
        let (a, b) = canonical(a, b);
        Ok(sqlx::query_as!(
            crate::store::links::LinkRow,
            "SELECT * FROM artifact_links WHERE a_id = ? AND b_id = ?",
            a,
            b
        )
        .fetch_optional(&self.pool)
        .await?
        .map(row_to_link))
    }

    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT value FROM meta WHERE key = ?", key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query!(
            "INSERT INTO meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            key,
            value
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One statement per anchor: a link is found from either end, an `OR` over two indexed
    /// columns. Fetches four times `limit` so the exact decay test in Rust does not eat the budget.
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
                // Necessary, not sufficient: decay lowers it, and not every SQLite build has `pow`.
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
            // SQLite reads a negative LIMIT as unbounded.
            .bind(limit.max(0).saturating_mul(4))
            .fetch_all(&self.pool)
            .await?;

            for r in &rows {
                let state = LinkState::parse(r.get::<String, _>("state").as_str());
                // Enforced here, not by callers' `states`: the operator's "not related" is final.
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
                    // Merges are decided below through origins.
                    cross_corpus: match (a_corpus, b_corpus) {
                        (Some(x), Some(y)) => x != y,
                        _ => true,
                    },
                });
            }
        }
        out.sort_by(|x, y| y.weight.total_cmp(&x.weight));
        out.truncate(limit.max(0) as usize);
        // A merge belongs to every corpus it drew from: "same document" means origins intersect.
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

    /// Cross-corpus is decided over origins in Rust: a merge belongs to every corpus it drew from.
    pub async fn links_to_judge(
        &self,
        min_weight: f64,
        min_queries: i64,
        half_life_days: f64,
        at: i64,
        limit: i64,
    ) -> Result<Vec<Link>> {
        // SQLite reads a negative LIMIT as unbounded.
        let cap = limit.max(0).saturating_mul(4);
        let rows = sqlx::query_as!(
            LinkRow,
            "SELECT l.* FROM artifact_links l
               JOIN artifacts a ON a.id = l.a_id
               JOIN artifacts b ON b.id = l.b_id
              WHERE l.state = 'learning'
                AND l.weight >= ? AND l.queries >= ?
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL
              ORDER BY l.weight DESC LIMIT ?",
            min_weight,
            min_queries,
            cap
        )
        .fetch_all(&self.pool)
        .await?;

        let strong: Vec<Link> = rows
            .into_iter()
            .map(row_to_link)
            .filter(|l| decayed(l.weight, l.bumped_at, at, half_life_days) >= min_weight)
            .collect();
        // A same-document link is shown and never judged.
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

    /// Exact tests are capped at `scan_limit` per sweep; the next sweep prunes the rest.
    pub async fn prune_learning_links(
        &self,
        below: f64,
        half_life_days: f64,
        at: i64,
        scan_limit: i64,
    ) -> Result<u64> {
        let mut dropped = sqlx::query!(
            "DELETE FROM artifact_links WHERE state = 'learning' AND weight < ?",
            below
        )
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
            dropped += sqlx::query!(
                "DELETE FROM artifact_links WHERE a_id = ? AND b_id = ?",
                r.get::<String, _>("a_id"),
                r.get::<String, _>("b_id")
            )
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

    /// The judge read text that no longer exists.
    pub async fn reopen_stale_judged_links(&self, limit: i64) -> Result<u64> {
        Ok(sqlx::query(
            // `judge_attempts` is kept: a re-embed gets one more call, not a fresh three.
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

    /// `judged_revs` is what `reopen_stale_judged_links` compares against later.
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
        sqlx::query!(
            "UPDATE artifact_links
                SET state = ?, reason = ?, judged_rev_a = ?, judged_rev_b = ?
              WHERE a_id = ? AND b_id = ?",
            state.as_str(),
            reason,
            rev_a,
            rev_b,
            a,
            b
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Only `state` moves: weight and reason stay as the evidence the operator overruled.
    pub async fn dismiss_link(&self, a: &str, b: &str) -> Result<()> {
        let (a, b) = canonical(a, b);
        sqlx::query!(
            "UPDATE artifact_links SET state = ? WHERE a_id = ? AND b_id = ?",
            LinkState::Dismissed.as_str(),
            a,
            b
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Counted only for an unreadable reply: an outage says nothing about the link.
    pub async fn record_link_judge_attempt(&self, a: &str, b: &str) -> Result<i64> {
        let (a, b) = canonical(a, b);
        Ok(sqlx::query_scalar!(
            "UPDATE artifact_links SET judge_attempts = judge_attempts + 1
              WHERE a_id = ? AND b_id = ? RETURNING judge_attempts",
            a,
            b
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0))
    }

    /// `created_at` gives the baseline's age, which `activated_at` does not. One statement for
    /// the whole list (the query path); only `?` placeholders are generated.
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

    /// Read-then-write, since not every SQLite build has math functions, in one transaction
    /// per artifact so interleaved bumps do not drop a delta.
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
            sqlx::query!(
                "UPDATE artifacts SET activation = ?, activated_at = ? WHERE id = ?",
                decayed(value, stamp, at, half_life_days) + delta,
                at,
                id
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn link_counts(&self) -> Result<LinkCounts> {
        Ok(LinkCounts {
            total: sqlx::query_scalar!("SELECT COUNT(*) FROM artifact_links")
                .fetch_one(&self.pool)
                .await?,
            related: sqlx::query_scalar!(
                "SELECT COUNT(*) FROM artifact_links WHERE state = 'related'"
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

    async fn two(store: &Store) -> (String, String) {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = ["alpha", "beta"]
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).into(),
                title: Some((*t).into()),
                ..Default::default()
            })
            .collect();
        let made = store.insert_artifacts(&src.id, &new).await.unwrap();
        (made[0].id.clone(), made[1].id.clone())
    }

    #[test]
    fn a_pair_is_filed_the_same_way_round_however_it_is_named() {
        assert_eq!(canonical("b", "a"), ("a", "b"));
        assert_eq!(canonical("a", "b"), ("a", "b"));
    }

    #[test]
    fn a_query_is_the_same_cue_however_it_was_typed() {
        assert_eq!(normalize_query("  Loop   Device \n"), "loop device");
    }

    #[test]
    fn weight_halves_over_one_half_life() {
        // Lazy decay is the only place decay happens: forgetting costs no writes.
        let day = 86_400;
        assert!((decayed(4.0, 0, 30 * day, 30.0) - 2.0).abs() < 1e-9);
        assert!((decayed(4.0, 0, 60 * day, 30.0) - 1.0).abs() < 1e-9);
        assert!((decayed(4.0, 100, 100, 30.0) - 4.0).abs() < 1e-9);
        assert!((decayed(4.0, 100, 0, 30.0) - 4.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn bumping_folds_the_decay_in_rather_than_adding_to_a_stale_number() {
        // Without folding decay in, a link strong a year ago and used once today would read strong.
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
        assert_eq!(l.a_id, a.min(b.clone()));
    }

    #[tokio::test]
    async fn the_same_query_twice_is_one_binding_query() {
        // The cue count still climbs, because that is how the top three are chosen.
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

    async fn two_corpora(store: &Store) -> (String, String) {
        let mut ids = Vec::new();
        for (raw, text) in [("raw one", "alpha"), ("raw two", "beta")] {
            let src = store.insert_corpus(raw, "web", None).await.unwrap();
            let made = store
                .insert_artifacts(
                    &src.id,
                    &[NewArtifact {
                        text: text.into(),
                        title: Some(text.into()),
                        ..Default::default()
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
        // Filtering on the stored weight would keep showing a link unused for a year.
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
    async fn a_synthesized_relation_is_not_reopened_by_the_next_sweep() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        store
            .relate_synthesized(&a, &b, "both describe the same procedure", 4.0, 30.0)
            .await
            .unwrap();

        assert_eq!(
            store.reopen_stale_judged_links(100).await.unwrap(),
            0,
            "the judged revisions were stamped, so nothing is stale"
        );
        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(
            l.reason.as_deref(),
            Some("both describe the same procedure")
        );
        assert_eq!((l.judged_rev_a, l.judged_rev_b), (Some(0), Some(0)));

        store
            .update_artifact_text(&a, "alpha, rewritten")
            .await
            .unwrap();
        assert_eq!(store.reopen_stale_judged_links(100).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_dismissed_pair_keeps_its_dismissal_and_its_stamp() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        store
            .bump_link(&a, &b, 5.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        store.dismiss_link(&a, &b).await.unwrap();
        store
            .relate_synthesized(&a, &b, "the model disagrees", 4.0, 30.0)
            .await
            .unwrap();
        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(l.state, LinkState::Dismissed);
        assert_ne!(l.reason.as_deref(), Some("the model disagrees"));
        assert_eq!(
            l.weight, 5.0,
            "a dismissal keeps the evidence it was made against"
        );
    }

    #[tokio::test]
    async fn a_synthesized_relation_clears_the_floor_it_has_to_be_shown_over() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        let show_min = 2.0;
        store
            .relate_synthesized(
                &a,
                &b,
                "both describe the same procedure",
                show_min * 2.0,
                30.0,
            )
            .await
            .unwrap();
        let at = now();
        let shown = store
            .links_from(
                std::slice::from_ref(&a),
                &[LinkState::Related],
                30.0,
                at,
                show_min,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            shown.len(),
            1,
            "the band never showed what the model asserted"
        );
        assert_eq!(shown[0].other, b);

        // Still above the floor a half-life later: the weight is not the floor itself.
        let shown = store
            .links_from(
                std::slice::from_ref(&a),
                &[LinkState::Related],
                30.0,
                at + 29 * 86_400,
                show_min,
                10,
            )
            .await
            .unwrap();
        assert_eq!(shown.len(), 1, "it fell under the floor within the month");
    }

    #[tokio::test]
    async fn re_asserting_folds_the_decay_in_and_never_lowers_an_earned_link() {
        let store = Store::memory().await.unwrap();
        let (a, b) = two_corpora(&store).await;
        store
            .bump_link(&a, &b, 9.0, Some("q"), 30.0, now())
            .await
            .unwrap();
        store
            .relate_synthesized(&a, &b, "the model agrees", 4.0, 30.0)
            .await
            .unwrap();
        let l = store.get_link(&a, &b).await.unwrap().unwrap();
        assert_eq!(
            l.weight, 9.0,
            "an assertion must not lower what use has earned"
        );

        // Two half-lives ago: worth 2.25 now, not 9.0.
        let (c, d) = two_corpora(&store).await;
        store
            .bump_link(&c, &d, 9.0, Some("q"), 30.0, now() - 60 * 86_400)
            .await
            .unwrap();
        store
            .relate_synthesized(&c, &d, "the model agrees", 4.0, 30.0)
            .await
            .unwrap();
        let l = store.get_link(&c, &d).await.unwrap().unwrap();
        assert_eq!(l.weight, 4.0, "the decay was not folded in");
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
        // Unstamped, every artifact reads as decayed since 1970.
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        let act = store.activation_of(std::slice::from_ref(&a)).await.unwrap();
        let (value, stamp, created_at) = act
            .get(&a)
            .copied()
            .expect("an artifact carries activation");
        assert!((value - 1.0).abs() < 1e-9);
        assert!(stamp > 0, "activated_at was never set at insert");
        assert_eq!(created_at, stamp);
    }

    #[tokio::test]
    async fn a_bump_folds_the_decay_in_like_a_link_does() {
        let store = Store::memory().await.unwrap();
        let (a, _) = two(&store).await;
        sqlx::query!(
            "UPDATE artifacts SET activation = 4.0, activated_at = 0 WHERE id = ?",
            &a
        )
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

    async fn third(store: &Store) -> String {
        let src = store.insert_corpus("raw2", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "the artifact".into(),
                    ..Default::default()
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
        assert!((copied.weight - 1.0).abs() < 1e-6, "{}", copied.weight);
        assert_eq!(copied.bumped_at, 1_000 + 14 * 86_400);
        assert_eq!(copied.queries, from.queries);
        assert_eq!(copied.cues.len(), 1);
        assert_eq!(copied.state, LinkState::Learning);
        assert!(copied.judged_rev_a.is_none() && copied.judged_rev_b.is_none());
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
        // Same document, however the merge's own `corpus_id` reads.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("one", "web", None).await.unwrap();
        let na = |o: i64, t: &str| NewArtifact {
            ordinal: o,
            text: t.into(),
            ..Default::default()
        };
        let made = store
            .insert_artifacts(&src.id, &[na(0, "a"), na(1, "b"), na(2, "c")])
            .await
            .unwrap();
        let m = store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "a and b".into(),
                    ..Default::default()
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
        let armed = store.links_to_judge(4.0, 3, 30.0, 0, 10).await.unwrap();
        assert_eq!(armed.len(), 1, "{armed:?}");
        assert!(armed[0].a_id == far || armed[0].b_id == far);
    }
}
