//! Recorded searches and their verdicts, kept apart in time: the query must be recorded
//! before results are seen, and a label assigned while reading the answer contaminates it.

use super::{Store, new_id, now};
use crate::error::Result;

/// A parameter, not a `SearchQuery` field: a deserialised default would mislabel the door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    Ui,
    Api,
    Mcp,
    /// Never captured: composed in full knowledge of the answer.
    Judge,
    /// The strongest uncontaminated query: composed before anything came back.
    Extension,
    /// Distinguished for the reason `Extension` is.
    Cli,
    /// Never captured: its answer is a synthesis, so "which one was it" has no meaning.
    Ask,
    /// Recorded like `Ui`: the app draws the same verdict bars, so its opens count.
    App,
}

impl Door {
    pub fn as_str(&self) -> &'static str {
        match self {
            Door::Ui => "ui",
            Door::Api => "api",
            Door::Mcp => "mcp",
            Door::Extension => "extension",
            Door::Cli => "cli",
            Door::Judge => "judge",
            Door::Ask => "ask",
            Door::App => "app",
        }
    }

    pub fn captured(&self) -> bool {
        matches!(
            self,
            Door::Ui | Door::Api | Door::Mcp | Door::Extension | Door::Cli | Door::App
        )
    }

    pub const ALL: [Door; 8] = [
        Door::Ui,
        Door::Api,
        Door::Mcp,
        Door::Judge,
        Door::App,
        Door::Extension,
        Door::Cli,
        Door::Ask,
    ];

    /// The captured doors, and `Judge` because the idle pass replays through it: measurement
    /// and serving must fetch the same pool. `Ask` is excluded.
    pub fn fetches_the_capture_pool(&self) -> bool {
        self.captured() || matches!(self, Door::Judge)
    }

    /// Only the web UI writes `opened_at`; on other doors "unopened" holds by construction, so
    /// `jobs::observe` must not read it as a weak negative.
    pub fn records_opens(&self) -> bool {
        matches!(self, Door::Ui | Door::App)
    }

    /// Only `extension` and `cli`; anything else is `Api`, so no client can claim a clean door.
    pub fn from_client(raw: &str) -> Door {
        match raw {
            "extension" => Door::Extension,
            "cli" => Door::Cli,
            "app" => Door::App,
            _ => Door::Api,
        }
    }
}

/// A separate type: only the UI has a scope, and `From<Door>` covers the rest.
#[derive(Debug, Clone)]
pub struct Origin {
    pub door: Door,
    /// `None` folds only with other unscoped events from the same door.
    pub scope: Option<String>,
    /// A web session id; `None` on every other door, since a token is not a conversation.
    /// Read only by priming, when `sitting.prime` is on.
    pub session: Option<String>,
    /// See `NewEvent::fold_onto`.
    pub fold_onto: Option<String>,
    /// Set only by the idle pass, from `search_context`, so a replay sees what the searcher saw.
    pub replay: Option<crate::core::search::Priming>,
}

impl From<Door> for Origin {
    fn from(door: Door) -> Self {
        Origin {
            door,
            scope: None,
            session: None,
            fold_onto: None,
            replay: None,
        }
    }
}

impl Door {
    pub fn by(self, scope: impl Into<String>) -> Origin {
        Origin {
            door: self,
            scope: Some(scope.into()),
            session: None,
            fold_onto: None,
            replay: None,
        }
    }
}

impl Origin {
    /// Only a door with a real session identity may say so; see `Origin::session`.
    pub fn in_sitting(mut self, session: Option<String>) -> Origin {
        self.session = session;
        self
    }

    pub fn folding_onto(mut self, event_id: Option<String>) -> Origin {
        self.fold_onto = event_id;
        self
    }

    pub fn primed_as(mut self, p: crate::core::search::Priming) -> Origin {
        self.replay = Some(p);
        self
    }
}

#[derive(Debug, Clone)]
pub struct NewCandidate {
    pub artifact_id: String,
    pub score: f32,
    /// `None` for a verbatim lexical hit; see `SearchResult::weak`.
    pub similarity: Option<f32>,
    pub shown: bool,
    pub band: bool,
    /// `None` unless the search explored. See `search_candidates`.
    pub explored_from: Option<i64>,
}

/// Test-only: in production every field is a decision, so a new field must break every call site.
#[cfg(test)]
impl Default for NewCandidate {
    fn default() -> Self {
        Self {
            artifact_id: String::new(),
            score: 0.0,
            similarity: None,
            shown: false,
            band: false,
            explored_from: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub query: String,
    pub door: Door,
    /// Only coalescing reads it.
    pub scope: Option<String>,
    /// JSON, so a replay can reproduce the same narrowing.
    pub filters: String,
    pub query_vec: Vec<f32>,
    /// Vectors are only comparable under the model that produced them.
    pub embed_model: String,
    pub candidates: Vec<NewCandidate>,
    /// A synthesized artifact led above `weak_below`; the pursuit closes satisfied.
    pub answered: bool,
    /// The id the typing page was handed by its last answer. See the fold rule in `record_search`.
    pub fold_onto: Option<String>,
    /// Kept so the idle pass can replay the search at another lift.
    pub context: Option<crate::core::search::Priming>,
}

/// See `Store::band_use`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandUse {
    pub band_used: usize,
    pub tail_used: usize,
}

#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub id: String,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub created_at: i64,
    pub answered: bool,
    /// `expect_id` when the verdict is `hit`.
    pub confirmed: Option<String>,
    pub scope: Option<String>,
    /// The shown candidates: `(artifact_id, similarity)`, rank order.
    pub shown: Vec<(String, Option<f32>)>,
}

pub(crate) fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    // `.0` drops trailing bytes that are not a whole float, as `chunks_exact` did.
    let (chunks, _) = b.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

impl Store {
    /// Every query is captured, and a text box's query within `coalesce_secs` replaces the
    /// event it names (`fold_onto`). API and MCP calls are deliberate and never fold.
    pub async fn record_search(&self, ev: NewEvent, coalesce_secs: i64) -> Result<String> {
        // Immediate: a deferred read-then-write gets `SQLITE_BUSY_SNAPSHOT`, which no busy
        // timeout waits out, and the capture would be lost.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let at = now();

        // The web UI names the row to fold into, so tabs never collide; the extension names none,
        // so it folds by searcher (`scope IS ?`). A judged or opened event is never folded into.
        let prev = match ev.door {
            Door::Ui => match ev.fold_onto.as_deref() {
                Some(prev_id) => sqlx::query!(
                    r#"SELECT id AS "id!", created_at,
                                (SELECT COUNT(*) FROM search_candidates
                                  WHERE event_id = search_events.id) AS "pool!: i64"
                           FROM search_events
                          WHERE id = ? AND door = ? AND scope IS ?
                            AND judged_at IS NULL AND opened_at IS NULL"#,
                    prev_id,
                    ev.door.as_str(),
                    &ev.scope
                )
                .fetch_optional(&mut *tx)
                .await?
                .map(|r| (r.id, r.created_at, r.pool)),
                None => None,
            },
            Door::Extension | Door::App => sqlx::query!(
                r#"SELECT id AS "id!", created_at,
                            (SELECT COUNT(*) FROM search_candidates
                              WHERE event_id = search_events.id) AS "pool!: i64"
                       FROM search_events
                      WHERE door = ? AND scope IS ? AND judged_at IS NULL
                        AND opened_at IS NULL
                      ORDER BY created_at DESC, id DESC LIMIT 1"#,
                ev.door.as_str(),
                &ev.scope
            )
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| (r.id, r.created_at, r.pool)),
            _ => None,
        };

        // Every rewording inside the window folds forward, shorter ones included, so the stored
        // event matches the rendered rail. Only an open stops a fold: its pool is the one scored.
        enum Fold {
            Extends(String),
            New,
        }
        let fold = match prev {
            Some((id, created, pool)) => {
                // Zero means folding is off; a plain `<=` would fold within the same second.
                let fresh = coalesce_secs > 0 && at - created <= coalesce_secs;
                // Never fold a pool away to nothing: an empty narrowing starts its own event.
                let empties = ev.candidates.is_empty() && pool > 0;
                if !fresh || empties {
                    Fold::New
                } else {
                    Fold::Extends(id)
                }
            }
            None => Fold::New,
        };

        let extends = match fold {
            Fold::Extends(id) => Some(id),
            _ => None,
        };

        let id = match extends {
            Some(id) => {
                // `skips` and the generation reset with the rest: a skip was about
                // the earlier wording, and `dealable!` would hide the new one forever.
                sqlx::query!(
                    "UPDATE search_events
                     SET query = ?, filters = ?, query_vec = ?, vec_dim = ?,
                         embed_model = ?, created_at = ?, answered = ?, skips = 0,
                         generation_id = (SELECT id FROM generations WHERE state = 'live'
                                           ORDER BY created_at DESC, id DESC LIMIT 1)
                     WHERE id = ?",
                    &ev.query,
                    &ev.filters,
                    vec_to_blob(&ev.query_vec),
                    ev.query_vec.len() as i64,
                    &ev.embed_model,
                    at,
                    ev.answered as i64,
                    &id
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query!("DELETE FROM search_candidates WHERE event_id = ?", &id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query!("DELETE FROM search_context WHERE event_id = ?", &id)
                    .execute(&mut *tx)
                    .await?;
                id
            }
            None => {
                let id = new_id();
                // Read in the same statement, under the same write lock, as the row it stamps.
                sqlx::query!(
                    "INSERT INTO search_events
                       (id, query, door, scope, filters, query_vec, vec_dim, embed_model,
                        created_at, answered, generation_id)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                             (SELECT id FROM generations WHERE state = 'live'
                               ORDER BY created_at DESC, id DESC LIMIT 1))",
                    &id,
                    &ev.query,
                    ev.door.as_str(),
                    &ev.scope,
                    &ev.filters,
                    vec_to_blob(&ev.query_vec),
                    ev.query_vec.len() as i64,
                    &ev.embed_model,
                    at,
                    ev.answered as i64
                )
                .execute(&mut *tx)
                .await?;
                id
            }
        };

        for (rank, c) in ev.candidates.iter().enumerate() {
            sqlx::query!(
                "INSERT INTO search_candidates
                   (event_id, rank, artifact_id, score, similarity, shown, band, explored_from)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                &id,
                rank as i64,
                &c.artifact_id,
                c.score,
                c.similarity,
                c.shown as i64,
                c.band as i64,
                c.explored_from
            )
            .execute(&mut *tx)
            .await?;
        }
        if let Some(ctx) = &ev.context {
            let raw = serde_json::to_string(ctx)
                .map_err(|e| crate::error::Error::Store(format!("search_context: {e}")))?;
            sqlx::query!(
                "INSERT INTO search_context (event_id, context) VALUES (?, ?)",
                &id,
                raw
            )
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(id)
    }

    pub async fn band_use(&self, generation_id: &str, spread_max: usize) -> Result<BandUse> {
        let r = sqlx::query!(
            "SELECT
               COALESCE(SUM(CASE WHEN c.band = 1 THEN 1 ELSE 0 END), 0) AS band_used,
               COALESCE(SUM(CASE WHEN c.band = 0 AND c.rank >=
                   (SELECT COUNT(*) FROM search_candidates s
                     WHERE s.event_id = c.event_id AND s.band = 0 AND s.shown = 1) - ?
                 THEN 1 ELSE 0 END), 0) AS tail_used
             FROM observations o
             JOIN search_candidates c
               ON c.event_id = o.event_id AND c.artifact_id = o.artifact_id
              AND c.shown = 1
            WHERE o.generation_id = ? AND o.source = 'opened' AND o.excluded_at IS NULL",
            spread_max as i64,
            generation_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(BandUse {
            band_used: r.band_used as usize,
            tail_used: r.tail_used as usize,
        })
    }

    pub async fn search_context(
        &self,
        event_id: &str,
    ) -> Result<Option<crate::core::search::Priming>> {
        let raw: Option<String> = sqlx::query_scalar!(
            "SELECT context FROM search_context WHERE event_id = ?",
            event_id
        )
        .fetch_optional(&self.pool)
        .await?;
        raw.map(|s| {
            serde_json::from_str(&s)
                .map_err(|e| crate::error::Error::Store(format!("search_context: {e}")))
        })
        .transpose()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// It may be an artifact the search never returned, the most valuable case.
    Hit,
    /// Nothing in the base could have answered this. Not a pair; a finding.
    Gap,
    /// Not a real search — a typo, or poking at the box.
    Discard,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Hit => "hit",
            Verdict::Gap => "gap",
            Verdict::Discard => "discard",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Labeller {
    /// Retired; stored as NULL, like every verdict before the column existed.
    Deck,
    /// At the moment, by the person who searched.
    Confirm,
}

impl Labeller {
    fn as_sql(self) -> Option<&'static str> {
        match self {
            Labeller::Deck => None,
            Labeller::Confirm => Some("confirm"),
        }
    }
}

/// The answerable set `pending_count` counts. `EXISTS`: an empty pool is a gap, not a card
/// (even at `weak_below = 0.0`). An unscored candidate reads strongest, not zero. Skipped
/// searches and searches a capture answered are not waiting.
macro_rules! dealable {
    () => {
        "judged_at IS NULL AND skips = 0 AND length(query) >= 3
         AND NOT EXISTS (SELECT 1 FROM gap_coverage
                          WHERE kind IN ('search', 'unmatched')
                            AND gap_id = search_events.id
                            AND covered_by = 'capture')
         AND EXISTS (SELECT 1 FROM search_candidates WHERE event_id = search_events.id)
         AND COALESCE((SELECT max(COALESCE(similarity, 1.0)) FROM search_candidates
                        WHERE event_id = search_events.id AND band = 0), 0) >= ?"
    };
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub captured: i64,
    /// Only answerable ones: a count of unanswerable questions never empties.
    pub pending: i64,
    pub judged: i64,
    pub hits: i64,
    /// The only evidence that ranking, rather than the corpus, was at fault.
    pub finds: i64,
    pub gaps: i64,
    pub discards: i64,
    pub recall_at_10: f64,
    pub mrr: f64,
}

/// Carries the query vector so the idle pass replays without embedding.
#[derive(Debug, Clone)]
pub struct JudgedPair {
    pub event_id: String,
    pub query: String,
    pub expect: String,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    /// 1-based like `observations.rank`, charged where the ranking put it, not an exploration's
    /// loan. `None` where the answer was never in the pool.
    pub served_rank: Option<i64>,
}

impl Store {
    /// Retention and purges remove rows under open screens; an UPDATE matching nothing is not a
    /// recorded judgement.
    fn judged_one(res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// `AND judged_at IS NULL`: another tab may have answered meanwhile.
    pub async fn judge_hit(&self, event_id: &str, artifact_id: &str, by: Labeller) -> Result<()> {
        Self::judged_one(
            sqlx::query!(
                "UPDATE search_events
                 SET judged_at = ?, verdict = 'hit', expect_id = ?, judged_by = ?
                 WHERE id = ? AND judged_at IS NULL",
                now(),
                artifact_id,
                by.as_sql(),
                event_id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// `AND judged_at IS NULL` against the same race. `NotFound` reads as a refusal here.
    pub async fn judge(&self, event_id: &str, verdict: Verdict, by: Labeller) -> Result<()> {
        Self::judged_one(
            sqlx::query!(
                "UPDATE search_events
                 SET judged_at = ?, verdict = ?, expect_id = NULL, judged_by = ?
                 WHERE id = ? AND judged_at IS NULL",
                now(),
                verdict.as_str(),
                by.as_sql(),
                event_id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// An event id from the page is not a capability: the caller must own the row. `scope = ?`,
    /// not `IS`: the UI door always names its subject.
    pub async fn event_is_mine(&self, event_id: &str, scope: &str) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            "SELECT 1 FROM search_events WHERE id = ? AND scope = ?",
            event_id,
            scope
        )
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }

    /// Named by the page, not guessed. The pool is checked for the artifact: an in-flight
    /// rewording may have folded over the row. `false` when gone, judged, or folded.
    pub async fn open_event(&self, event_id: &str, artifact_id: &str) -> Result<bool> {
        let generation = self.live_generation().await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // Read before the stamp destroys "already opened". The rank recorded is the screen
        // position over shown rows (ranked window, then band), 1-based; pool rank is untouched.
        let before = sqlx::query!(
            "SELECT e.opened_at AS opened_at, e.query AS query,
                    e.query_vec AS query_vec, e.embed_model AS embed_model,
                    (SELECT COUNT(*) FROM search_candidates s
                      WHERE s.event_id = e.id AND s.shown = 1
                        AND (s.band < c.band
                             OR (s.band = c.band AND s.rank < c.rank))) AS shown_rank,
                    c.shown AS shown,
                    c.rank AS rank,
                    c.explored_from AS explored_from
               FROM search_events e
               JOIN search_candidates c
                 ON c.event_id = e.id AND c.artifact_id = ?
              WHERE e.id = ?
              ORDER BY c.shown DESC, c.rank ASC
              LIMIT 1",
            artifact_id,
            event_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        let stamped = sqlx::query!(
            "UPDATE search_events SET opened_at = ?
              WHERE id = ? AND judged_at IS NULL
                AND EXISTS (SELECT 1 FROM search_candidates
                             WHERE event_id = ? AND artifact_id = ?)",
            now(),
            event_id,
            event_id,
            artifact_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        // An open is the strongest thing a plain search says.
        if let (true, Some(g), Some(row)) = (stamped, &generation, &before)
            && row.opened_at.is_none()
        {
            crate::store::observations::insert(
                &mut *tx,
                &crate::store::observations::NewObservation {
                    generation_id: g.id.clone(),
                    query: row.query.clone(),
                    query_vec: blob_to_vec(&row.query_vec),
                    embed_model: row.embed_model.clone(),
                    artifact_id: Some(artifact_id.to_string()),
                    // 1-based screen position, or pool position if never shown. An explored row is
                    // charged where the ranking put it: the exploration's whole value.
                    rank: Some(match (row.explored_from, row.shown == 1) {
                        (Some(natural), _) => natural + 1,
                        (None, true) => row.shown_rank + 1,
                        (None, false) => row.rank + 1,
                    }),
                    source: crate::store::observations::Source::Opened,
                    event_id: Some(event_id.to_string()),
                },
            )
            .await?;
        }

        tx.commit().await?;
        Ok(stamped)
    }

    /// `AND judged_at IS NULL`: this clears the verdict, so a stale tab would delete a pair.
    pub async fn decline(&self, event_id: &str) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE search_events
             SET judged_at = NULL, verdict = NULL, expect_id = NULL, judged_by = 'confirm'
             WHERE id = ? AND judged_at IS NULL",
            event_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// `AND query = ?`: a trailing keystroke may have folded a later wording into the row.
    /// `false` when purged, judged, or reworded: all ordinary outcomes.
    pub async fn gap_event(&self, event_id: &str, query: &str) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE search_events
             SET judged_at = ?, verdict = ?, expect_id = NULL, judged_by = ?
             WHERE id = ? AND query = ? AND judged_at IS NULL",
            now(),
            Verdict::Gap.as_str(),
            Labeller::Confirm.as_sql(),
            event_id,
            query
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Clears `expect_id` too. Only this labeller's verdict (`judged_by IS ?`), so an undo
    /// cannot erase somebody else's.
    pub async fn unjudge(&self, event_id: &str, by: Labeller) -> Result<()> {
        Self::judged_one(
            // `skips` resets too, or the event stays invisible to `pending_count`.
            sqlx::query!(
                "UPDATE search_events
                 SET judged_at = NULL, verdict = NULL, expect_id = NULL, judged_by = NULL,
                     skips = 0
                 WHERE id = ? AND judged_by IS ?",
                event_id,
                by.as_sql()
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// Not a verdict: an honest "I don't remember" costs nothing but leaves the waiting figure.
    pub async fn skip_event(&self, event_id: &str) -> Result<()> {
        Self::judged_one(
            sqlx::query!(
                "UPDATE search_events SET skips = skips + 1 WHERE id = ?",
                event_id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// Two index seeks per unjudged event (`dealable!`); with `retain_days = 0` the set only grows.
    pub(crate) async fn pending_count(&self, weak_below: f32) -> Result<i64> {
        // runtime: shares the `dealable!` predicate with the other reads of the pool.
        Ok(sqlx::query_scalar(concat!(
            "SELECT count(*) FROM search_events WHERE ",
            dealable!()
        ))
        .bind(weak_below)
        .fetch_one(&self.pool)
        .await?)
    }

    /// No vector store involved, so the figure moves on every judgement.
    pub async fn feedback_stats(&self, weak_below: f32) -> Result<Stats> {
        let mut s = Stats {
            captured: sqlx::query_scalar!("SELECT count(*) FROM search_events")
                .fetch_one(&self.pool)
                .await?,
            pending: self.pending_count(weak_below).await?,
            ..Default::default()
        };

        for (field, verdict) in [
            (&mut s.hits, "hit"),
            (&mut s.gaps, "gap"),
            (&mut s.discards, "discard"),
        ] {
            *field = sqlx::query_scalar!(
                "SELECT count(*) FROM search_events WHERE verdict = ?",
                verdict
            )
            .fetch_one(&self.pool)
            .await?;
        }
        s.judged = s.hits + s.gaps + s.discards;

        // A correlated subquery: an artifact may sit in the pool twice (ranked and band). The band
        // counts; the place is the screen position, the same expression `open_event` stamps.
        let ranks: Vec<Option<i64>> = sqlx::query!(
            "SELECT (SELECT CASE WHEN c.shown = 1
                                 THEN (SELECT COUNT(*) FROM search_candidates s
                                        WHERE s.event_id = e.id AND s.shown = 1
                                          AND (s.band < c.band
                                               OR (s.band = c.band AND s.rank < c.rank)))
                                 ELSE c.rank END
                       FROM search_candidates c
                      WHERE c.event_id = e.id AND c.artifact_id = e.expect_id
                      ORDER BY c.shown DESC, c.band ASC, c.rank ASC
                      LIMIT 1) AS rank
               FROM search_events e
              WHERE e.verdict = 'hit'"
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| r.rank)
        .collect();

        s.finds = ranks.iter().filter(|r| r.is_none()).count() as i64;
        if !ranks.is_empty() {
            let n = ranks.len() as f64;
            s.recall_at_10 = ranks
                .iter()
                .filter(|r| matches!(r, Some(i) if *i < 10))
                .count() as f64
                / n;
            s.mrr = ranks
                .iter()
                .map(|r| r.map_or(0.0, |i| 1.0 / (i as f64 + 1.0)))
                .sum::<f64>()
                / n;
        }
        Ok(s)
    }

    /// Gaps and discards name no artifact. Scoped to one generation: other eras' vectors are
    /// not readable by the live index.
    pub async fn judged_pairs(&self, generation_id: &str, limit: usize) -> Result<Vec<JudgedPair>> {
        Ok(sqlx::query!(
            r#"SELECT e.id AS "id!", e.query AS "query!", e.expect_id AS "expect_id!", e.query_vec, e.embed_model, (SELECT MIN(COALESCE(c.explored_from, c.rank)) + 1
                       FROM search_candidates c
                      WHERE c.event_id = e.id AND c.artifact_id = e.expect_id) AS served_rank
               FROM search_events e
              WHERE e.verdict = 'hit' AND e.expect_id IS NOT NULL AND e.generation_id = ?
              ORDER BY e.judged_at DESC, e.id DESC
              LIMIT ?"#,
            generation_id,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| JudgedPair {
            event_id: r.id,
            query: r.query,
            expect: r.expect_id,
            query_vec: blob_to_vec(&r.query_vec),
            embed_model: r.embed_model,
            served_rank: r.served_rank,
        })
        .collect())
    }

    /// `None` for a row retention has removed.
    pub async fn search_query(&self, id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT query FROM search_events WHERE id = ?", id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn activity_since(&self, since: i64) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            r#"SELECT (SELECT COUNT(*) FROM search_events WHERE created_at > ?)
                  + (SELECT COUNT(*) FROM ask_events WHERE created_at > ?) AS "n!: i64""#,
            since,
            since
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    pub async fn judged_since(&self, since: i64) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT count(*) FROM search_events WHERE judged_at >= ?",
            since
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Its own ticker: a retention promise must not lapse with another feature. Judged events
    /// are exempt; `discard` is not. `purge_feedback` takes everything.
    pub async fn expire_feedback(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        let searches = sqlx::query!(
            "DELETE FROM search_events
                 WHERE created_at < ? AND (verdict IS NULL OR verdict = 'discard')",
            now() - retain_days * 86_400
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        // Every table holding typed words ages: questions and observations too.
        Ok(searches
            + self.expire_asks(retain_days).await?
            + self.expire_observations(retain_days).await?)
    }

    pub async fn purge_feedback(&self) -> Result<u64> {
        // `search_candidates` goes through ON DELETE CASCADE.
        let searches = sqlx::query!("DELETE FROM search_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        // Centroids are the situations averaged, so they go too. Their copies on the points are
        // cleared by `Core::forget_situations`.
        let situations = sqlx::query!("DELETE FROM context_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        let profiles = sqlx::query!("DELETE FROM context_clusters")
            .execute(&self.pool)
            .await?
            .rows_affected();
        // Observations hold the same queries and nothing cascades to them.
        Ok(searches
            + situations
            + profiles
            + self.purge_asks().await?
            + self.purge_pursuits().await?
            + self.purge_observations().await?)
    }

    /// The judge door is excluded: a benchmark query is not a need.
    pub async fn events_between(&self, from: i64, to: i64) -> Result<Vec<RecordedEvent>> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!", query, query_vec, created_at, answered, verdict, expect_id, scope
               FROM search_events
              WHERE created_at > ? AND created_at <= ? AND door <> 'judge'
              ORDER BY created_at, id"#,
            from,
            to
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.id;
            let shown: Vec<(String, Option<f32>)> = sqlx::query!(
                "SELECT artifact_id, similarity FROM search_candidates
                  WHERE event_id = ? AND shown = 1 AND band = 0 ORDER BY rank",
                &id
            )
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|c| (c.artifact_id, c.similarity.map(|x| x as f32)))
            .collect();
            let verdict: Option<String> = r.verdict;
            out.push(RecordedEvent {
                id,
                query: r.query,
                query_vec: blob_to_vec(&r.query_vec),
                created_at: r.created_at,
                answered: r.answered != 0,
                confirmed: if verdict.as_deref() == Some("hit") {
                    r.expect_id
                } else {
                    None
                },
                scope: r.scope,
                shown,
            });
        }
        Ok(out)
    }

    /// Every kind of event, not just searches: reading is session activity, and calling it idle
    /// drops its interactions. The judge door is excluded: a benchmark is not a session.
    pub async fn newest_event_at(&self) -> Result<Option<i64>> {
        let s: Option<i64> =
            sqlx::query_scalar!("SELECT MAX(created_at) FROM search_events WHERE door <> 'judge'")
                .fetch_one(&self.pool)
                .await?;
        let a: Option<i64> = sqlx::query_scalar!("SELECT MAX(created_at) FROM ask_events")
            .fetch_one(&self.pool)
            .await?;
        // `recommended_shown` is written on every page view, so it is not activity;
        // `recommended_open` is.
        let i: Option<i64> = sqlx::query_scalar!(
            "SELECT MAX(at) FROM interaction_events WHERE kind <> 'recommended_shown'"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok([s, a, i].into_iter().flatten().max())
    }
}

#[cfg(test)]
mod tests {

    async fn observed_base() -> (Store, String) {
        use crate::store::generations::{GenerationParams, NewGeneration};
        let store = Store::memory().await.unwrap();
        let generation = store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                    ..Default::default()
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        (store, generation)
    }

    fn event_with(artifacts: &[&str]) -> NewEvent {
        NewEvent {
            query: "loop device".into(),
            door: Door::Ui,
            scope: Some("me".into()),
            filters: "{}".into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "fake".into(),
            candidates: artifacts
                .iter()
                .enumerate()
                .map(|(i, a)| NewCandidate {
                    artifact_id: (*a).to_string(),
                    score: 1.0 - i as f32 * 0.1,
                    similarity: Some(0.9 - i as f32 * 0.1),
                    shown: true,
                    ..Default::default()
                })
                .collect(),
            answered: false,
            fold_onto: None,
            context: None,
        }
    }

    #[tokio::test]
    async fn a_recorded_search_keeps_what_priming_read_and_an_open_names_the_search() {
        let (store, generation) = observed_base().await;
        let mut ev = event_with(&["art-1", "art-2"]);
        ev.context = Some(crate::core::search::Priming {
            activation: [("art-1".to_string(), 0.7)].into_iter().collect(),
            sitting: ["art-2".to_string()].into_iter().collect(),
            due: Default::default(),
        });
        let id = store.record_search(ev, 0).await.unwrap();
        let ctx = store
            .search_context(&id)
            .await
            .unwrap()
            .expect("context stored");
        assert_eq!(ctx.activation.get("art-1"), Some(&0.7));
        assert!(ctx.sitting.contains("art-2"));

        assert!(store.open_event(&id, "art-1").await.unwrap());
        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs[0].event_id.as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn a_search_without_priming_stores_no_context_and_a_fold_drops_the_old_one() {
        let (store, _) = observed_base().await;
        let mut first = event_with(&["art-1"]);
        first.context = Some(crate::core::search::Priming::default());
        let id = store.record_search(first, 60).await.unwrap();
        assert!(store.search_context(&id).await.unwrap().is_some());

        let mut second = event_with(&["art-1"]);
        second.query = "loop devices".into();
        second.fold_onto = Some(id.clone());
        let folded = store.record_search(second, 60).await.unwrap();
        assert_eq!(folded, id, "this test needs the fold to happen");
        assert!(store.search_context(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_open_in_the_band_is_recorded_at_the_place_it_was_on_screen() {
        let (store, generation) = observed_base().await;
        let mut ev = event_with(&[]);
        ev.candidates = (0..20)
            .map(|i| NewCandidate {
                artifact_id: format!("art-{i}"),
                score: 1.0 - i as f32 * 0.01,
                similarity: Some(0.9),
                shown: i < 10,
                ..Default::default()
            })
            .chain(std::iter::once(NewCandidate {
                artifact_id: "recalled".into(),
                score: 0.4,
                shown: true,
                band: true,
                ..Default::default()
            }))
            .collect();
        let event = store.record_search(ev, 5).await.unwrap();

        assert!(store.open_event(&event, "recalled").await.unwrap());
        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(
            obs[0].rank,
            Some(11),
            "the first row under a window of ten, not the row after the pool"
        );
    }

    #[tokio::test]
    async fn a_confirmed_hit_in_the_band_is_a_find_at_the_place_it_was_on_screen() {
        let (store, _) = observed_base().await;
        let mut ev = event_with(&["art-1", "art-2"]);
        ev.candidates.push(NewCandidate {
            artifact_id: "recalled".into(),
            score: 0.4,
            shown: true,
            band: true,
            ..Default::default()
        });
        let event = store.record_search(ev, 5).await.unwrap();
        store
            .judge_hit(&event, "recalled", Labeller::Deck)
            .await
            .unwrap();

        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!(s.hits, 1);
        assert_eq!(s.finds, 0, "it was returned — under the ranked list");
        assert_eq!(s.recall_at_10, 1.0);
        // Third on screen: the same place `open_event` stamps, zero-based here.
        assert!((s.mrr - 1.0 / 3.0).abs() < 1e-9, "{}", s.mrr);
    }

    #[tokio::test]
    async fn a_ranked_open_keeps_its_place_whatever_is_appended_under_it() {
        let (store, generation) = observed_base().await;
        let mut ev = event_with(&["art-1", "art-2", "art-3"]);
        ev.candidates.push(NewCandidate {
            artifact_id: "recalled".into(),
            score: 0.4,
            shown: true,
            band: true,
            ..Default::default()
        });
        let event = store.record_search(ev, 5).await.unwrap();

        assert!(store.open_event(&event, "art-3").await.unwrap());
        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs[0].rank, Some(3));
    }

    #[tokio::test]
    async fn an_open_on_an_explored_row_is_charged_to_the_rank_the_ranking_chose() {
        use crate::store::observations::Source;
        let (store, generation) = observed_base().await;
        let mut ev = event_with(&["art-1", "art-2", "art-3"]);
        ev.candidates[2].artifact_id = "hidden".into();
        ev.candidates[2].explored_from = Some(5);
        let event = store.record_search(ev, 5).await.unwrap();

        assert!(store.open_event(&event, "hidden").await.unwrap());
        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs[0].source, Source::Opened);
        assert_eq!(
            obs[0].rank,
            Some(6),
            "credited to the row it borrowed instead of the rank it held"
        );

        let (store, generation) = observed_base().await;
        let event = store
            .record_search(event_with(&["art-1", "art-2", "art-3"]), 5)
            .await
            .unwrap();
        assert!(store.open_event(&event, "art-3").await.unwrap());
        assert_eq!(
            store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()[0]
                .rank,
            Some(3)
        );
    }

    #[tokio::test]
    async fn an_opened_result_is_an_observation_at_the_rank_it_was_listed() {
        use crate::store::observations::Source;
        let (store, generation) = observed_base().await;
        let event = store
            .record_search(event_with(&["art-1", "art-2", "art-3"]), 5)
            .await
            .unwrap();

        assert!(store.open_event(&event, "art-2").await.unwrap());

        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].artifact_id.as_deref(), Some("art-2"));
        assert_eq!(obs[0].rank, Some(2));
        assert_eq!(obs[0].source, Source::Opened);
        assert_eq!(obs[0].query, "loop device");
    }

    #[tokio::test]
    async fn opening_an_artifact_the_search_never_listed_writes_nothing() {
        // The observation must not outlive `open_event`'s refusal.
        let (store, generation) = observed_base().await;
        let event = store
            .record_search(event_with(&["art-1"]), 5)
            .await
            .unwrap();

        assert!(!store.open_event(&event, "art-9").await.unwrap());
        assert!(
            store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn opening_the_same_result_twice_leaves_one_observation() {
        // Stops a double click being double evidence.
        let (store, generation) = observed_base().await;
        let event = store
            .record_search(event_with(&["art-1"]), 5)
            .await
            .unwrap();
        store.open_event(&event, "art-1").await.unwrap();
        store.open_event(&event, "art-1").await.unwrap();

        assert_eq!(
            store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_client_may_claim_the_cli_door_and_still_nothing_else() {
        use super::Door;
        assert_eq!(Door::from_client("cli"), Door::Cli);
        assert_eq!(Door::from_client("extension"), Door::Extension);
        // A contaminated query still cannot label itself clean.
        assert_eq!(Door::from_client("ask"), Door::Api);
        assert_eq!(Door::from_client("judge"), Door::Api);
        assert!(
            Door::Cli.captured(),
            "a query typed at a shell is judgeable"
        );
        assert_eq!(Door::Cli.as_str(), "cli");
    }

    #[tokio::test]
    async fn an_offer_nobody_asked_for_does_not_keep_the_base_looking_busy() {
        // `recommended_shown` on every page view would keep the base looking active forever.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "x".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id;

        store
            .record_interaction(&aid, "opened", None, Some("me"), 1_000)
            .await
            .unwrap();
        store
            .record_recommendation(&aid, "recommended_shown", "{}", Some("me"), 9_000)
            .await
            .unwrap();
        assert_eq!(
            store.newest_event_at().await.unwrap(),
            Some(1_000),
            "the offer does not count as activity"
        );

        store
            .record_recommendation(&aid, "recommended_open", "{}", Some("me"), 9_500)
            .await
            .unwrap();
        assert_eq!(store.newest_event_at().await.unwrap(), Some(9_500));
    }
    use super::*;

    #[tokio::test]
    async fn activity_is_a_search_or_a_question_after_the_moment_asked_about() {
        let store = Store::memory().await.unwrap();
        let before = crate::store::now() - 10;
        assert!(
            !store.activity_since(before).await.unwrap(),
            "an empty base is quiet"
        );
        store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: "loop device".into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        assert!(store.activity_since(before).await.unwrap());
        assert!(
            !store
                .activity_since(crate::store::now() + 10)
                .await
                .unwrap(),
            "nothing has happened in the future"
        );
    }

    fn ev(query: &str, door: Door) -> NewEvent {
        scoped(query, door, None)
    }

    #[test]
    fn only_the_extension_may_name_its_own_door() {
        assert!(matches!(Door::from_client("extension"), Door::Extension));
        for other in ["ui", "judge", "ask", "mcp", "", "nonsense"] {
            assert!(
                matches!(Door::from_client(other), Door::Api),
                "client named {other}"
            );
        }
    }

    #[test]
    fn an_extension_search_is_captured_like_a_ui_one() {
        assert!(Door::Extension.captured());
        assert_eq!(Door::Extension.as_str(), "extension");
    }

    fn scoped(query: &str, door: Door, scope: Option<&str>) -> NewEvent {
        NewEvent {
            fold_onto: None,
            query: query.into(),
            door,
            scope: scope.map(str::to_string),
            filters: "{}".into(),
            query_vec: vec![0.5, -0.25],
            embed_model: "fake".into(),
            candidates: vec![NewCandidate {
                artifact_id: "a1".into(),
                score: 0.9,
                similarity: Some(0.8),
                shown: true,
                ..Default::default()
            }],
            answered: false,
            context: None,
        }
    }

    fn after(prev: &str, query: &str, door: Door) -> NewEvent {
        NewEvent {
            fold_onto: Some(prev.to_string()),
            ..ev(query, door)
        }
    }

    async fn queries(store: &Store) -> Vec<String> {
        sqlx::query!("SELECT query FROM search_events ORDER BY created_at")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.query)
            .collect()
    }

    #[tokio::test]
    async fn two_operators_typing_at_once_keep_their_own_events() {
        // Keyed on the door alone it folded across people.
        let store = Store::memory().await.unwrap();
        let alice = store
            .record_search(scoped("backup restore", Door::Ui, Some("alice")), 15)
            .await
            .unwrap();
        // An id off a page is not a capability; the scope refuses.
        let mut bob = scoped("backup", Door::Ui, Some("bob"));
        bob.fold_onto = Some(alice.clone());
        store.record_search(bob, 15).await.unwrap();

        let mut got = queries(&store).await;
        got.sort();
        assert_eq!(got, vec!["backup", "backup restore"]);
    }

    #[tokio::test]
    async fn deliberate_calls_through_the_other_doors_never_fold() {
        // Only a text box produces the keystroke bursts folding exists for.
        for door in [Door::Mcp, Door::Api] {
            let store = Store::memory().await.unwrap();
            store
                .record_search(ev("list files", door), 15)
                .await
                .unwrap();
            store
                .record_search(ev("list files in dir", door), 15)
                .await
                .unwrap();
            assert_eq!(
                queries(&store).await,
                vec!["list files", "list files in dir"],
                "{} folded a deliberate call",
                door.as_str()
            );
        }
    }

    #[tokio::test]
    async fn a_typing_burst_collapses_to_its_final_wording() {
        let store = Store::memory().await.unwrap();
        let mut id = store
            .record_search(ev("daten", Door::Ui), 15)
            .await
            .unwrap();
        for q in ["datentr", "datenträger nicht erkannt"] {
            id = store
                .record_search(after(&id, q, Door::Ui), 15)
                .await
                .unwrap();
        }
        assert_eq!(queries(&store).await, vec!["datenträger nicht erkannt"]);
    }

    #[tokio::test]
    async fn a_typing_burst_in_the_extension_panel_collapses_the_same_way() {
        // The panel debounces at 200ms, so one query arrives as several.
        let store = Store::memory().await.unwrap();
        for q in ["loop", "loop dev", "loop device"] {
            store
                .record_search(ev(q, Door::Extension), 15)
                .await
                .unwrap();
        }
        assert_eq!(queries(&store).await, vec!["loop device"]);
    }

    #[tokio::test]
    async fn a_shorter_query_folds_forward_like_any_other() {
        // Deleting a word is an ordinary edit; the record follows the box.
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(ev("fat32 mount", Door::Ui), 15)
            .await
            .unwrap();
        store
            .record_search(after(&id, "fat32", Door::Ui), 15)
            .await
            .unwrap();

        assert_eq!(queries(&store).await, vec!["fat32"]);
        let candidates: i64 = sqlx::query_scalar!("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(candidates, 1, "and the pool that answered it");
    }

    #[tokio::test]
    async fn a_typing_burst_recorded_concurrently_still_collapses() {
        // The in-memory store has one connection, so it cannot reproduce the busy-snapshot failure.
        let store = Store::memory().await.unwrap();
        let first = store.record_search(ev("d", Door::Ui), 15).await.unwrap();
        let mut tasks = Vec::new();
        for n in 2..="datenträger".chars().count() {
            let store = store.clone();
            let first = first.clone();
            let q: String = "datenträger".chars().take(n).collect();
            tasks.push(tokio::spawn(async move {
                store.record_search(after(&first, &q, Door::Ui), 15).await
            }));
        }
        for t in tasks {
            t.await.unwrap().expect("a capture was dropped");
        }

        assert_eq!(queries(&store).await, vec!["datenträger"]);
    }

    #[tokio::test]
    async fn the_same_search_arriving_twice_stays_one_event() {
        // The form re-fires on load, submit and filter change; repeats must fold, not multiply.
        let store = Store::memory().await.unwrap();
        let mut id = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        for _ in 0..2 {
            id = store
                .record_search(after(&id, "fat32", Door::Ui), 15)
                .await
                .unwrap();
        }

        assert_eq!(queries(&store).await, vec!["fat32"]);
        let candidates: i64 = sqlx::query_scalar!("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(candidates, 1, "the pool was written once per repeat");
    }

    #[tokio::test]
    async fn the_same_search_outside_the_window_is_asked_again() {
        // An hour later is a second occasion, judged as one.
        let store = Store::memory().await.unwrap();
        let first = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            now() - 3600,
            &first
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store
            .record_search(after(&first, "fat32", Door::Ui), 15)
            .await
            .unwrap();

        assert_eq!(queries(&store).await, vec!["fat32", "fat32"]);
    }

    #[tokio::test]
    async fn a_rewording_within_the_window_folds_into_one_event() {
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        store
            .record_search(after(&id, "ntfs", Door::Ui), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["ntfs"]);
    }

    async fn judged_by(store: &Store, id: &str) -> Option<String> {
        sqlx::query_scalar!("SELECT judged_by FROM search_events WHERE id = ?", id)
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn opening_a_result_stamps_the_search_the_page_named() {
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(scoped("fat32", Door::Ui, Some("me")), 15)
            .await
            .unwrap();
        assert!(store.open_event(&id, "a1").await.unwrap());
        assert!(
            !store.open_event("no-such-event", "a1").await.unwrap(),
            "an event retention or a purge took away is nothing to open"
        );

        store.judge_hit(&id, "a1", Labeller::Confirm).await.unwrap();
        assert!(!store.open_event(&id, "a1").await.unwrap());
    }

    #[tokio::test]
    async fn a_fold_onto_a_skipped_event_is_a_new_question_and_waits_again() {
        let (store, _) = observed_base().await;
        let first = store
            .record_search(event_with(&["art-1", "art-2"]), 0)
            .await
            .unwrap();
        store.skip_event(&first).await.unwrap();
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);

        let mut next = event_with(&["art-3", "art-4"]);
        next.query = "loop device on a mac".into();
        next.fold_onto = Some(first.clone());
        let folded = store.record_search(next, 300).await.unwrap();
        assert_eq!(folded, first, "the same row, rewritten");

        assert_eq!(
            store.pending_count(0.0).await.unwrap(),
            1,
            "a question nobody has answered, waiting"
        );
    }

    #[tokio::test]
    async fn a_fold_that_took_the_artifact_out_of_the_pool_refuses_the_open() {
        // An in-flight search can fold over the row before the click's stamp lands.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut later = after(&id, "fat32", Door::Ui);
        later.candidates[0].artifact_id = "a2".into();
        assert_eq!(store.record_search(later, 15).await.unwrap(), id, "folded");
        assert!(
            !store.open_event(&id, "a1").await.unwrap(),
            "the pool the rail was showing is gone, so there is nothing to open against"
        );
        assert!(store.open_event(&id, "a2").await.unwrap());
    }

    #[tokio::test]
    async fn yes_cannot_overwrite_a_verdict_somebody_else_gave() {
        // A stale tab's Yes must not replace another surface's gap.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("ntfs", Door::Ui), 0).await.unwrap();
        store
            .judge(&id, Verdict::Gap, Labeller::Deck)
            .await
            .unwrap();
        assert!(matches!(
            store.judge_hit(&id, "a1", Labeller::Confirm).await,
            Err(crate::error::Error::NotFound)
        ));
        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.gaps, s.hits), (1, 0), "the gap stood: {s:?}");
    }

    #[tokio::test]
    async fn undo_takes_back_only_what_this_labeller_gave() {
        // An undo in a stale tab must not wipe a hit the deck recorded meanwhile.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("ntfs", Door::Ui), 0).await.unwrap();
        assert!(store.decline(&id).await.unwrap());
        store.judge_hit(&id, "a1", Labeller::Deck).await.unwrap();

        assert!(matches!(
            store.unjudge(&id, Labeller::Confirm).await,
            Err(crate::error::Error::NotFound)
        ));
        assert_eq!(
            store.feedback_stats(0.0).await.unwrap().hits,
            1,
            "the pair survived the stale tab"
        );
        store.unjudge(&id, Labeller::Deck).await.unwrap();
        assert_eq!(store.feedback_stats(0.0).await.unwrap().hits, 0);
    }

    #[tokio::test]
    async fn two_windows_of_one_searcher_do_not_fold_into_each_other() {
        // Each page names its own event, so two windows never fold into each other.
        let store = Store::memory().await.unwrap();
        let tab_a = store
            .record_search(scoped("fat32", Door::Ui, Some("me")), 15)
            .await
            .unwrap();
        let tab_b = store
            .record_search(scoped("ntfs", Door::Ui, Some("me")), 15)
            .await
            .unwrap();
        assert_ne!(tab_a, tab_b, "the second window started its own search");

        let mut b = scoped("ntfs mount", Door::Ui, Some("me"));
        b.fold_onto = Some(tab_b.clone());
        assert_eq!(store.record_search(b, 15).await.unwrap(), tab_b);
        let mut got = queries(&store).await;
        got.sort();
        assert_eq!(got, vec!["fat32", "ntfs mount"]);
        assert!(store.open_event(&tab_a, "a1").await.unwrap());
    }

    #[tokio::test]
    async fn the_waiting_count_reads_the_pool_through_an_index() {
        // Both lookups must be index reads: the unjudged set only grows.
        use sqlx::Row;
        let store = Store::memory().await.unwrap();
        // runtime: shares the `dealable!` predicate with the other reads of the pool.
        let plan: Vec<String> = sqlx::query(concat!(
            "EXPLAIN QUERY PLAN SELECT count(*) FROM search_events WHERE ",
            dealable!()
        ))
        .bind(0.3f32)
        .fetch_all(&store.pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("detail"))
        .collect();
        assert!(
            plan.iter().all(|l| !l.contains("SCAN search_candidates")),
            "the waiting count scans the pool table: {plan:#?}"
        );
        assert_eq!(
            plan.iter()
                .filter(|l| l.contains("idx_candidates_similarity"))
                .count(),
            2,
            "both subqueries should be covered by the index: {plan:#?}"
        );
    }

    #[tokio::test]
    async fn a_gap_is_refused_where_the_box_has_moved_on() {
        // The gap belongs to the words that were on the screen.
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        store
            .record_search(after(&id, "fat32 mount", Door::Ui), 15)
            .await
            .unwrap();

        assert!(!store.gap_event(&id, "fat32").await.unwrap());
        assert_eq!(store.feedback_stats(0.0).await.unwrap().gaps, 0);
        assert!(store.gap_event(&id, "fat32 mount").await.unwrap());
    }

    #[tokio::test]
    async fn no_cannot_erase_a_verdict_somebody_else_gave() {
        // "No" clears verdict columns, so unguarded it deletes a confirmed pair.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("ntfs", Door::Ui), 0).await.unwrap();
        store.judge_hit(&id, "a1", Labeller::Deck).await.unwrap();
        assert!(!store.decline(&id).await.unwrap());
        assert_eq!(
            store.feedback_stats(0.0).await.unwrap().hits,
            1,
            "the pair survived"
        );
    }

    #[tokio::test]
    async fn a_search_that_returned_nothing_is_not_counted_with_labelling_off() {
        // At `weak_below = 0.0` the outer `COALESCE` would let an empty pool through.
        let store = Store::memory().await.unwrap();
        let mut empty = ev("nothing here", Door::Ui);
        empty.candidates.clear();
        store.record_search(empty, 0).await.unwrap();
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_opened_event_is_never_folded_into() {
        // The opened pool is the list the hit is ranked against.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        assert!(store.open_event(&id, "a1").await.unwrap());
        store
            .record_search(after(&id, "fat32", Door::Ui), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn saying_no_leaves_the_search_pending() {
        // "No" is not "the base has nothing": the search stays open.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("ntfs", Door::Ui), 0).await.unwrap();
        assert!(store.decline(&id).await.unwrap());
        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.judged, s.hits), (0, 0));
        assert_eq!(judged_by(&store, &id).await.as_deref(), Some("confirm"));
        assert_eq!(
            store.pending_count(0.0).await.unwrap(),
            1,
            "and the search stays a question"
        );
    }

    #[tokio::test]
    async fn a_search_too_short_or_too_loose_to_judge_is_never_counted() {
        // Kept and pending for the gap sweep, but left out of the waiting count.
        let store = Store::memory().await.unwrap();
        seed(&store, "ab", &["a1"]).await;
        let mut loose = ev("fat32", Door::Ui);
        loose.candidates[0].similarity = Some(0.2);
        store.record_search(loose, 0).await.unwrap();
        assert_eq!(store.pending_count(0.3).await.unwrap(), 0);

        let id = seed(&store, "ntfs", &["a1"]).await;
        assert_eq!(store.pending_count(0.3).await.unwrap(), 1);

        assert_eq!(
            store.feedback_stats(0.3).await.unwrap().pending,
            store.pending_count(0.3).await.unwrap()
        );

        store.open_event(&id, "a1").await.unwrap();
        assert_eq!(store.pending_count(0.3).await.unwrap(), 1);
    }

    async fn answering_capture(store: &Store) -> (String, String) {
        let src = store
            .insert_corpus("The ID photo is on Wednesday at 09:00.", "web", None)
            .await
            .unwrap();
        let made = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "The ID photo is on Wednesday at 09:00.".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        (src.id, made[0].id.clone())
    }

    #[tokio::test]
    async fn a_search_a_capture_answered_stops_waiting() {
        // A capture's own half-typed sentence is not a question waiting for anyone.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "when is the ID photo", &["a1"]).await;
        assert_eq!(store.pending_count(0.3).await.unwrap(), 1);

        let (corpus, artifact) = answering_capture(&store).await;
        store
            .cover_gap(
                crate::store::gaps::GapKind::Unmatched,
                &id,
                &corpus,
                &artifact,
                0.0,
                crate::store::gaps::CoveredBy::Capture,
            )
            .await
            .unwrap();

        assert_eq!(store.pending_count(0.3).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_search_the_coverage_check_closed_is_still_waiting() {
        // Only the capture typed from the query answers it; distance-based coverage does not.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "when is the ID photo", &["a1"]).await;

        let (corpus, artifact) = answering_capture(&store).await;
        store
            .cover_gap(
                crate::store::gaps::GapKind::Unmatched,
                &id,
                &corpus,
                &artifact,
                0.8,
                crate::store::gaps::CoveredBy::Distance,
            )
            .await
            .unwrap();

        assert_eq!(store.pending_count(0.3).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_search_the_vector_half_never_scored_is_still_counted() {
        // A lexical-only hit has no similarity, and that is not a zero.
        let store = Store::memory().await.unwrap();
        let mut lexical = ev("fat32", Door::Ui);
        lexical.candidates[0].similarity = None;
        store.record_search(lexical, 0).await.unwrap();
        assert_eq!(store.pending_count(0.35).await.unwrap(), 1);

        let mut empty = ev("ntfs", Door::Ui);
        empty.candidates.clear();
        store.record_search(empty, 0).await.unwrap();
        assert_eq!(
            store.pending_count(0.35).await.unwrap(),
            1,
            "a search that returned nothing is a hole, not a question: there is
             no list to point at, so there is nothing to answer. It is not
             lost — `store::gaps` raises it as an unmatched gap, which is what
             `a_search_that_returned_nothing_is_an_unmatched_gap` holds it to."
        );
    }

    #[tokio::test]
    async fn a_verdict_records_who_gave_it_and_undo_forgets_it() {
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        store.judge_hit(&id, "a1", Labeller::Confirm).await.unwrap();
        assert_eq!(judged_by(&store, &id).await.as_deref(), Some("confirm"));
        store.unjudge(&id, Labeller::Confirm).await.unwrap();
        assert_eq!(judged_by(&store, &id).await, None);
        assert_eq!(store.pending_count(0.0).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn nothing_here_has_it_records_a_gap_against_the_search_just_made() {
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(scoped("xyz", Door::Ui, Some("me")), 15)
            .await
            .unwrap();
        assert!(store.gap_event(&id, "xyz").await.unwrap());
        assert_eq!(store.feedback_stats(0.0).await.unwrap().gaps, 1);
        assert_eq!(judged_by(&store, &id).await.as_deref(), Some("confirm"));
        assert!(
            !store.gap_event(&id, "xyz").await.unwrap(),
            "a second press finds nothing left to label"
        );
        assert!(
            !store.gap_event("no-such-event", "xyz").await.unwrap(),
            "and an event that is no longer there is not a gap either"
        );
    }

    #[tokio::test]
    async fn a_prefix_from_another_door_does_not_fold_into_this_one() {
        // Folding across doors would invent a search nobody made.
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        store
            .record_search(ev("fat32", Door::Mcp), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn a_prefix_outside_the_window_starts_its_own_event() {
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        store.record_search(ev("fat32", Door::Ui), 0).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn folding_replaces_the_candidate_list_rather_than_appending_to_it() {
        // The candidates belong to the query that produced them.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut second = after(&id, "fat32", Door::Ui);
        second.candidates[0].artifact_id = "a2".into();
        store.record_search(second, 15).await.unwrap();

        let rows = sqlx::query!("SELECT artifact_id FROM search_candidates")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].artifact_id, "a2");
    }

    #[tokio::test]
    async fn a_narrowing_that_found_nothing_does_not_empty_the_stored_pool() {
        // An empty narrowing must not overwrite the pool that answered the same words.
        let store = Store::memory().await.unwrap();
        let first = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();

        let mut filtered = after(&first, "fat32", Door::Ui);
        filtered.filters = r#"{"category":"recipes"}"#.into();
        filtered.candidates.clear();
        let second = store.record_search(filtered, 15).await.unwrap();

        assert_ne!(first, second, "the empty search started its own event");
        let kept = sqlx::query!(
            "SELECT artifact_id FROM search_candidates WHERE event_id = ?",
            &first
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(kept.len(), 1, "the pool that answered the query survived");
    }

    #[tokio::test]
    async fn a_narrowing_that_found_something_still_folds() {
        let store = Store::memory().await.unwrap();
        let id = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        let mut filtered = after(&id, "fat32", Door::Ui);
        filtered.filters = r#"{"category":"disks"}"#.into();
        filtered.candidates[0].artifact_id = "a2".into();
        store.record_search(filtered, 15).await.unwrap();

        assert_eq!(queries(&store).await, vec!["fat32"]);
        let rows = sqlx::query!("SELECT artifact_id FROM search_candidates")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].artifact_id, "a2");
    }

    #[tokio::test]
    async fn a_judged_event_is_never_folded_into() {
        // Folding under a verdict would attach the label to words never judged.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        sqlx::query!(
            "UPDATE search_events SET judged_at = ?, verdict = 'gap' WHERE id = ?",
            now(),
            &id
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store
            .record_search(after(&id, "fat32", Door::Ui), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn an_empty_result_list_is_still_captured() {
        // No candidates is the most direct gap evidence; it must still be stored.
        let store = Store::memory().await.unwrap();
        let mut e = ev("etwas das es nicht gibt", Door::Ui);
        e.candidates.clear();
        store.record_search(e, 15).await.unwrap();
        assert_eq!(queries(&store).await.len(), 1);
    }

    async fn seed(store: &Store, query: &str, ranked: &[&str]) -> String {
        let mut e = ev(query, Door::Ui);
        e.candidates = ranked
            .iter()
            .enumerate()
            .map(|(i, id)| NewCandidate {
                artifact_id: (*id).into(),
                score: 1.0 - i as f32 / 100.0,
                similarity: Some(0.5),
                shown: i < 10,
                ..Default::default()
            })
            .collect();
        store.record_search(e, 0).await.unwrap()
    }

    #[tokio::test]
    async fn skipping_judges_nothing_and_stops_waiting() {
        // Not a verdict, but no longer counted as waiting: nothing asks it again.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "not sure", &["a"]).await;
        assert_eq!(store.pending_count(0.0).await.unwrap(), 1);
        store.skip_event(&id).await.unwrap();
        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.pending, s.judged), (0, 0), "{s:?}");
    }

    #[tokio::test]
    async fn undoing_a_verdict_on_a_skipped_search_puts_it_back_in_the_waiting_figure() {
        // `skips` stayed after an undo, hiding an outstanding search forever.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "not sure yet", &["a"]).await;
        store.skip_event(&id).await.unwrap();
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);
        store.judge_hit(&id, "a", Labeller::Deck).await.unwrap();
        store.unjudge(&id, Labeller::Deck).await.unwrap();
        assert_eq!(
            store.pending_count(0.0).await.unwrap(),
            1,
            "an undo puts the row back where it was before anybody answered"
        );
    }

    #[tokio::test]
    async fn a_judged_event_stops_waiting() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "only one", &["a"]).await;
        store.judge_hit(&id, "a", Labeller::Deck).await.unwrap();
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn forgetting_takes_the_situations_and_the_profiles_built_from_them() {
        // Forget must reach `context_events` and `context_clusters`, or the offer cites them.
        let store = Store::memory().await.unwrap();
        seed(&store, "when is the recycling centre open", &["a"]).await;
        store
            .record_context(&crate::store::context::ContextEvent {
                id: 0,
                scope: Some("alice".into()),
                at: 1_000,
                bundle: r#"{"tz":"Europe/Berlin","battery_level":0.4}"#.into(),
                device_key: Some("phone".into()),
                local_hour: Some(15.0),
                weekday: Some(4),
                tz: Some("Europe/Berlin".into()),
            })
            .await
            .unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let a = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "opening hours".into(),
                    title: Some("hours".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let aid = a[0].id.clone();
        store
            .replace_context_clusters(
                &aid,
                &[crate::store::context::StoredCluster {
                    scope: Some("alice".into()),
                    artifact_id: aid.clone(),
                    slot: 0,
                    centroid: vec![0.5; 53],
                    weight: 3.0,
                    events: 3,
                    last_at: 1_000,
                    encoder_version: 1,
                    representative: r#"{"at":1000,"bundle":{}}"#.into(),
                }],
            )
            .await
            .unwrap();

        store.purge_feedback().await.unwrap();

        assert!(
            store.context_events_since(0).await.unwrap().is_empty(),
            "the situations survived the button that says forget"
        );
        assert!(
            store
                .artifacts_with_context_clusters()
                .await
                .unwrap()
                .is_empty(),
            "the profiles built from them survived"
        );
    }

    #[tokio::test]
    async fn a_later_verdict_does_not_write_over_what_the_searcher_confirmed() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "already answered", &["a"]).await;
        store.judge_hit(&id, "a", Labeller::Confirm).await.unwrap();

        assert!(matches!(
            store.judge(&id, Verdict::Gap, Labeller::Deck).await,
            Err(crate::error::Error::NotFound)
        ));
        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.hits, s.gaps), (1, 0), "{s:?}");
    }

    #[tokio::test]
    async fn a_verdict_on_an_event_that_is_gone_is_refused() {
        // Reporting success would show an Undo for a row that does not exist.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "will be purged", &["a"]).await;
        store.purge_feedback().await.unwrap();

        for res in [
            store.judge_hit(&id, "a", Labeller::Deck).await,
            store.judge(&id, Verdict::Gap, Labeller::Deck).await,
            store.unjudge(&id, Labeller::Deck).await,
            store.skip_event(&id).await,
        ] {
            assert!(
                matches!(res, Err(crate::error::Error::NotFound)),
                "a write against a deleted event reported success: {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_field_metrics_read_the_rank_the_search_actually_gave() {
        let store = Store::memory().await.unwrap();
        let first = seed(&store, "top hit", &["a", "b", "c"]).await;
        store.judge_hit(&first, "a", Labeller::Deck).await.unwrap();
        let third = seed(&store, "third hit", &["x", "y", "z"]).await;
        store.judge_hit(&third, "z", Labeller::Deck).await.unwrap();

        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!(s.judged, 2);
        assert_eq!(s.hits, 2);
        assert!((s.recall_at_10 - 1.0).abs() < 1e-9);
        assert!((s.mrr - (1.0 + 1.0 / 3.0) / 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn the_judged_pairs_are_the_answers_and_only_the_answers() {
        // A gap or discard names no artifact; replayed, the ranking could only fail it.
        let (store, generation) = observed_base().await;
        let hit = seed(&store, "the image will not mount", &["a", "b"]).await;
        store.judge_hit(&hit, "b", Labeller::Deck).await.unwrap();
        let gap = seed(&store, "nothing about this", &["c"]).await;
        store
            .judge(&gap, Verdict::Gap, Labeller::Deck)
            .await
            .unwrap();
        let junk = seed(&store, "asdf", &["d"]).await;
        store
            .judge(&junk, Verdict::Discard, Labeller::Deck)
            .await
            .unwrap();
        seed(&store, "still waiting", &["e"]).await;

        let pairs = store.judged_pairs(&generation, 100).await.unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].event_id, hit);
        assert_eq!(pairs[0].query, "the image will not mount");
        assert_eq!(pairs[0].expect, "b");
        assert!(
            !pairs[0].query_vec.is_empty(),
            "a judged pair carries the vector its search was made with"
        );
        assert_eq!(
            pairs[0].served_rank,
            Some(2),
            "the pool position, counted from one as an observation's is"
        );
        assert!(
            store
                .judged_pairs("some-other-generation", 100)
                .await
                .unwrap()
                .is_empty(),
            "a verdict belongs to the generation it was given under"
        );

        // The day's counter reads verdicts, not pairs: a gap is judging.
        assert_eq!(store.judged_since(0).await.unwrap(), 3);
        assert_eq!(
            store.judged_since(crate::store::now() + 60).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn an_answer_the_pool_never_held_is_a_pair_with_no_served_rank() {
        // Its served rank is `None`: no position in the list was its.
        let (store, generation) = observed_base().await;
        let hit = seed(&store, "the image will not mount", &["a", "b"]).await;
        store
            .judge_hit(&hit, "elsewhere", Labeller::Deck)
            .await
            .unwrap();
        let pairs = store.judged_pairs(&generation, 100).await.unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].expect, "elsewhere");
        assert_eq!(pairs[0].served_rank, None);
    }

    #[tokio::test]
    async fn an_answer_outside_the_pool_counts_as_a_find() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "found nothing useful", &["a", "b"]).await;
        store
            .judge_hit(&id, "something-else", Labeller::Deck)
            .await
            .unwrap();

        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!(s.finds, 1);
        assert_eq!(s.recall_at_10, 0.0);
        assert_eq!(s.mrr, 0.0);
    }

    #[tokio::test]
    async fn gaps_and_discards_are_counted_but_are_not_pairs() {
        let store = Store::memory().await.unwrap();
        let g = seed(&store, "nothing written about this", &[]).await;
        store.judge(&g, Verdict::Gap, Labeller::Deck).await.unwrap();
        let d = seed(&store, "asdf", &["a"]).await;
        store
            .judge(&d, Verdict::Discard, Labeller::Deck)
            .await
            .unwrap();

        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.gaps, s.discards, s.hits), (1, 1, 0));
        assert_eq!(s.mrr, 0.0);
    }

    #[tokio::test]
    async fn retention_of_zero_keeps_everything() {
        let store = Store::memory().await.unwrap();
        seed(&store, "old", &["a"]).await;
        assert_eq!(store.expire_feedback(0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_event_past_the_retention_window_is_dropped() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "old", &["a"]).await;
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            now() - 40 * 86_400,
            &id
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_judged_event_outlives_the_retention_window() {
        // A verdict is the operator's work; expiring it would move recall invisibly.
        let store = Store::memory().await.unwrap();
        let kept = seed(&store, "judged", &["a"]).await;
        seed(&store, "never looked at", &["a"]).await;
        store.judge_hit(&kept, "a", Labeller::Deck).await.unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ?",
            now() - 40 * 86_400
        )
        .execute(&store.pool)
        .await
        .unwrap();

        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
        let left: Vec<String> = sqlx::query_scalar!("SELECT query FROM search_events")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["judged".to_string()]);
    }

    #[tokio::test]
    async fn a_discarded_event_expires_like_any_other() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "asdf", &["a"]).await;
        store
            .judge(&id, Verdict::Discard, Labeller::Deck)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ?",
            now() - 40 * 86_400
        )
        .execute(&store.pool)
        .await
        .unwrap();

        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn purging_removes_events_and_their_candidates() {
        let store = Store::memory().await.unwrap();
        seed(&store, "a search", &["a", "b"]).await;
        store.purge_feedback().await.unwrap();

        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_query_vector_survives_a_round_trip_through_the_blob() {
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let row = sqlx::query!(
            "SELECT query_vec, vec_dim FROM search_events WHERE id = ?",
            &id
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(row.vec_dim, 2);
        assert_eq!(blob_to_vec(&row.query_vec), vec![0.5, -0.25]);
    }

    #[tokio::test]
    async fn events_between_reads_answered_confirmed_and_what_was_shown() {
        let store = Store::memory().await.unwrap();
        let mut e = ev("a question", Door::Ui);
        e.answered = true;
        let id = store.record_search(e, 0).await.unwrap();
        store.judge_hit(&id, "a1", Labeller::Deck).await.unwrap();
        let now = now();
        let got = store.events_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].answered);
        assert_eq!(got[0].confirmed.as_deref(), Some("a1"));
        assert_eq!(got[0].shown, vec![("a1".to_string(), Some(0.8))]);
        assert_eq!(got[0].query_vec, vec![0.5, -0.25]);
        store
            .record_search(ev("bench", Door::Judge), 0)
            .await
            .unwrap();
        assert_eq!(store.events_between(0, now + 1).await.unwrap().len(), 1);
        // A harness kept `idle` from elapsing, so the sweep never ran.
        assert_eq!(store.newest_event_at().await.unwrap(), Some(now));
        store
            .insert_pursuit(now, &["q".into()], &[], None)
            .await
            .unwrap();
        store.purge_feedback().await.unwrap();
        assert!(store.recent_pursuits(10).await.unwrap().is_empty());
    }
}
