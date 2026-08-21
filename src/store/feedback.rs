//! What a real search looked like, so it can be judged later.
//!
//! The query is the one thing no amount of care can reconstruct afterwards: it
//! has to be recorded in the moment, before any result was seen. The verdict is
//! the opposite — it needs a person, and it can wait. Everything here exists to
//! keep those two apart in time, because a label assigned while reading the
//! answer contaminates the question.

use super::{Store, new_id, now};
use crate::error::Result;
use sqlx::Row;

/// Which front door a search came through.
///
/// An explicit parameter rather than a field on `SearchQuery`: that struct is
/// deserialised from the query string, so a `Default` there would silently
/// record an API search as a UI search the first time a caller forgot to set
/// it. With no default, the compiler asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    Ui,
    Api,
    Mcp,
    /// The search inside the judging view's "none of these" path. Never
    /// captured: those queries are composed in full knowledge of the answer,
    /// which is exactly the contamination this whole feature exists to avoid.
    Judge,
    /// A search made from the browser extension, usually over a selection on
    /// the page being read. Recorded like `Ui` and `Api`, and distinguished
    /// from them because it is the strongest uncontaminated query there is:
    /// composed before anything came back, about text the operator is looking
    /// at rather than text engram showed them.
    Extension,
    /// The retrieval behind `ask`. Never captured either, for a different
    /// reason: its right answer is a synthesis across several artifacts, so
    /// "which one was it" has no well-defined meaning to judge.
    Ask,
}

impl Door {
    pub fn as_str(&self) -> &'static str {
        match self {
            Door::Ui => "ui",
            Door::Api => "api",
            Door::Mcp => "mcp",
            Door::Extension => "extension",
            Door::Judge => "judge",
            Door::Ask => "ask",
        }
    }

    pub fn captured(&self) -> bool {
        matches!(self, Door::Ui | Door::Api | Door::Mcp | Door::Extension)
    }

    /// The door a client is allowed to claim for itself.
    ///
    /// Only `extension`. Everything else falls back to `Api`, because a client
    /// that could name `Ask` or `Judge` could mark a contaminated query as a
    /// clean one — or have a real one silently dropped — which is the exact
    /// thing the judging loop exists to prevent.
    pub fn from_client(raw: &str) -> Door {
        match raw {
            "extension" => Door::Extension,
            _ => Door::Api,
        }
    }
}

/// A door, plus who came through it where that is known.
///
/// A separate type rather than a second argument on every search entry point:
/// only the UI has a scope to give, so a `scope` parameter would be `None` at
/// almost every call site, and `From<Door>` lets the doors that have nothing to
/// say keep saying nothing.
#[derive(Debug, Clone)]
pub struct Origin {
    pub door: Door,
    /// The authenticated subject, for coalescing. `None` means unscoped, which
    /// folds only with other unscoped events from the same door.
    pub scope: Option<String>,
    /// The live sitting this search belongs to, where there is one — a web
    /// session id. `None` for every other door, which is what keeps the live
    /// sitting out of the API and `/mcp`: an access token is not a
    /// conversation. Read only by priming, and only when `sitting.prime` is on.
    pub session: Option<String>,
}

impl From<Door> for Origin {
    fn from(door: Door) -> Self {
        Origin {
            door,
            scope: None,
            session: None,
        }
    }
}

impl Door {
    /// This door, on behalf of a named subject.
    pub fn by(self, scope: impl Into<String>) -> Origin {
        Origin {
            door: self,
            scope: Some(scope.into()),
            session: None,
        }
    }
}

impl Origin {
    /// This search belongs to a live sitting. Only a door with a real session
    /// identity may say so — see `Origin::session`.
    pub fn in_sitting(mut self, session: Option<String>) -> Origin {
        self.session = session;
        self
    }
}

#[derive(Debug, Clone)]
pub struct NewCandidate {
    pub artifact_id: String,
    pub score: f32,
    /// Cosine, where the store could report one. `None` for a hit the lexical
    /// half matched verbatim — see `SearchResult::weak`.
    pub similarity: Option<f32>,
    /// Whether it was inside the answer the searcher actually saw.
    pub shown: bool,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub query: String,
    pub door: Door,
    /// Who searched, where the door knows — the authenticated subject for the
    /// UI, `None` everywhere else. Only coalescing reads it.
    pub scope: Option<String>,
    /// JSON, so a replay can reproduce the same narrowing.
    pub filters: String,
    pub query_vec: Vec<f32>,
    /// Vectors are only comparable under the model that produced them.
    pub embed_model: String,
    pub candidates: Vec<NewCandidate>,
    /// A synthesized artifact led the list above `weak_below`: the base
    /// answered, and the pursuit this lands in closes satisfied.
    pub answered: bool,
}

/// One recorded search as the pursuit sweep reads it.
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub id: String,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub created_at: i64,
    pub answered: bool,
    /// `expect_id` when the verdict is `hit`: a person said this artifact was
    /// the answer.
    pub confirmed: Option<String>,
    pub scope: Option<String>,
    /// The shown candidates: `(artifact_id, similarity)`, rank order.
    pub shown: Vec<(String, Option<f32>)>,
}

pub(crate) fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    // `as_chunks` rather than `chunks_exact`: the width is a constant, so the
    // chunk arrives as `[u8; 4]` and `from_le_bytes` takes it whole instead of
    // being handed a slice this has to re-assert the length of. `.0` drops the
    // trailing bytes of a blob that is not a whole number of floats, which is
    // what `chunks_exact` did with its remainder.
    let (chunks, _) = b.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

impl Store {
    /// Record one search, folding a typing burst into a single event.
    ///
    /// Capturing only deliberate searches would lose the most valuable case:
    /// `mark` is set on open, expand and submit, so a search where the operator
    /// found nothing useful and gave up would never be recorded. So everything
    /// is captured, and an event whose query extends the previous one from the
    /// same searcher — or repeats it verbatim — within `coalesce_secs` replaces
    /// it. What survives is the final wording: the query that was actually
    /// meant, asked once.
    ///
    /// Only text boxes fold, and only within one `scope`. That is the web UI's
    /// search field and the browser extension's panel, both of which search as
    /// you type — the panel debounces at 200ms, so one query still arrives as
    /// several. A call through the API or MCP is a deliberate query, and an
    /// agent narrowing one search into a longer one made two decisions worth
    /// judging separately.
    pub async fn record_search(&self, ev: NewEvent, coalesce_secs: i64) -> Result<String> {
        // One capture at a time. Two of these overlapping would read the same
        // previous event and both try to upgrade to a write, which fails
        // outright rather than waiting — and a lost capture is a search nobody
        // can judge.
        let _serialised = self.capture.lock().await;
        let mut tx = self.pool.begin().await?;
        let at = now();

        // `scope IS ?` rather than `=`, so a UI event recorded without a
        // subject still finds its own predecessor instead of matching nothing.
        let prev = match ev.door {
            Door::Ui | Door::Extension => {
                sqlx::query(
                    "SELECT id, query, created_at,
                            (SELECT COUNT(*) FROM search_candidates
                              WHERE event_id = search_events.id) AS pool
                       FROM search_events
                      WHERE door = ? AND scope IS ? AND judged_at IS NULL
                      ORDER BY created_at DESC, id DESC LIMIT 1",
                )
                .bind(ev.door.as_str())
                .bind(&ev.scope)
                .fetch_optional(&mut *tx)
                .await?
            }
            _ => None,
        };

        // Same typing burst, in either direction. A keystroke is one HTTP
        // request among several in flight, so "fat" can land after "fat32"; the
        // test asks whether one query is a prefix of the other rather than
        // whether this one grew, so the burst folds the same way regardless of
        // the order the requests happened to arrive in. A prefix of equal
        // length is the same query twice — the form fires on load, on submit
        // and on every filter change, so one search reaches here several times
        // over — and it folds forward like any other, taking the newer filters
        // and pool with it. Left unfolded it would be a second thing to judge
        // that says nothing new, and a second identical pair in the eval set.
        enum Fold {
            /// The stored event is an earlier keystroke of this one.
            Extends(String),
            /// This is an earlier keystroke of the stored event, arriving late.
            Superseded(String),
            New,
        }
        let fold = match prev.as_ref() {
            Some(r) => {
                let prior: String = r.get("query");
                let created: i64 = r.get("created_at");
                // A window of zero means folding is off, not "fold within the
                // same second" — which is what a plain `<=` gives, since both
                // events usually land on one timestamp.
                let fresh = coalesce_secs > 0 && at - created <= coalesce_secs;
                let id: String = r.get("id");
                // Folding forward replaces the stored pool with this one, and
                // the filters travel with it — that is the point, since the
                // form re-fires the same `q` on every chip. What it must not do
                // is fold a pool away to nothing: a narrowing that matched
                // zero artifacts would leave the event that is actually going
                // to be judged holding no candidates at all, and a card with no
                // options is unanswerable except by skip, gap or discard. The
                // earlier pool answered the same query and is the better record
                // of it, so the empty search starts its own event instead.
                let pool: i64 = r.get("pool");
                let empties = ev.candidates.is_empty() && pool > 0;
                if !fresh {
                    Fold::New
                } else if ev.query.len() >= prior.len() && ev.query.starts_with(&prior) && !empties
                {
                    Fold::Extends(id)
                } else if prior.len() > ev.query.len() && prior.starts_with(&ev.query) {
                    Fold::Superseded(id)
                } else {
                    Fold::New
                }
            }
            None => Fold::New,
        };

        // Nothing to write: the final wording is already stored, and it was
        // answered by a pool drawn for the query that was actually meant.
        if let Fold::Superseded(id) = fold {
            return Ok(id);
        }
        let extends = match fold {
            Fold::Extends(id) => Some(id),
            _ => None,
        };

        let id = match extends {
            Some(id) => {
                sqlx::query(
                    "UPDATE search_events
                     SET query = ?, filters = ?, query_vec = ?, vec_dim = ?,
                         embed_model = ?, created_at = ?, answered = ?
                     WHERE id = ?",
                )
                .bind(&ev.query)
                .bind(&ev.filters)
                .bind(vec_to_blob(&ev.query_vec))
                .bind(ev.query_vec.len() as i64)
                .bind(&ev.embed_model)
                .bind(at)
                .bind(ev.answered as i64)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM search_candidates WHERE event_id = ?")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                id
            }
            None => {
                let id = new_id();
                sqlx::query(
                    "INSERT INTO search_events
                       (id, query, door, scope, filters, query_vec, vec_dim, embed_model,
                        created_at, answered)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&ev.query)
                .bind(ev.door.as_str())
                .bind(&ev.scope)
                .bind(&ev.filters)
                .bind(vec_to_blob(&ev.query_vec))
                .bind(ev.query_vec.len() as i64)
                .bind(&ev.embed_model)
                .bind(at)
                .bind(ev.answered as i64)
                .execute(&mut *tx)
                .await?;
                id
            }
        };

        for (rank, c) in ev.candidates.iter().enumerate() {
            sqlx::query(
                "INSERT INTO search_candidates
                   (event_id, rank, artifact_id, score, similarity, shown)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(rank as i64)
            .bind(&c.artifact_id)
            .bind(c.score)
            .bind(c.similarity)
            .bind(c.shown as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// One artifact is the answer. `expect_id` names it — and it may be an
    /// artifact the search never returned, which is the most valuable case
    /// there is.
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

#[derive(Debug, Clone)]
pub struct Candidate {
    pub artifact_id: String,
    pub rank: i64,
    pub shown: bool,
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub id: String,
    pub query: String,
    pub door: String,
    pub created_at: i64,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub captured: i64,
    pub pending: i64,
    pub judged: i64,
    pub hits: i64,
    /// Hits whose artifact the search never returned. Rare, expensive, and the
    /// only evidence that ranking — rather than the corpus — was at fault.
    pub finds: i64,
    pub gaps: i64,
    pub discards: i64,
    pub recall_at_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone)]
pub struct Miss {
    pub query: String,
    /// `None` means the confirmed artifact was not in the stored pool at all.
    pub rank: Option<i64>,
}

impl Store {
    /// The next event to judge: never-skipped first, newest first within that.
    ///
    /// Newest first because a judgement is worth something only while the
    /// situation is still in mind, and that memory is the most perishable part
    /// of the whole dataset.
    pub async fn next_pending(&self) -> Result<Option<PendingEvent>> {
        let row = sqlx::query(
            // `id DESC` breaks the tie: two searches within one second are
            // ordinary, and `created_at` alone would leave SQLite to pick.
            // Ids are uuid v7, so they sort by time down to the millisecond.
            "SELECT id, query, door, created_at FROM search_events
             WHERE judged_at IS NULL
             ORDER BY skips ASC, created_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        self.hydrate(row).await.map(Some)
    }

    /// One event by id, judged or not.
    ///
    /// What undo needs: the event it just put back is not necessarily the one
    /// the judging order now puts first, and the operator expects to land back
    /// on the card they were looking at rather than somewhere else.
    pub async fn pending_by_id(&self, event_id: &str) -> Result<Option<PendingEvent>> {
        let row = sqlx::query("SELECT id, query, door, created_at FROM search_events WHERE id = ?")
            .bind(event_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        self.hydrate(row).await.map(Some)
    }

    /// An event row plus the pool it recorded.
    async fn hydrate(&self, row: sqlx::sqlite::SqliteRow) -> Result<PendingEvent> {
        let id: String = row.get("id");
        let candidates = sqlx::query(
            "SELECT artifact_id, rank, shown FROM search_candidates
             WHERE event_id = ? ORDER BY rank",
        )
        .bind(&id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Candidate {
            artifact_id: r.get("artifact_id"),
            rank: r.get("rank"),
            shown: r.get::<i64, _>("shown") == 1,
        })
        .collect();

        Ok(PendingEvent {
            id,
            query: row.get("query"),
            door: row.get("door"),
            created_at: row.get("created_at"),
            candidates,
        })
    }

    /// The query one event recorded, by id.
    ///
    /// Deliberately not `next_pending` with a filter: the judging order moves
    /// under a screen that is already open, and the operator's own event is not
    /// necessarily the one at the front of it. Judged events answer too — the
    /// caller wants the text, not a verdict.
    pub async fn event_query(&self, event_id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT query FROM search_events WHERE id = ?")
                .bind(event_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Where this artifact stood in what the search returned, if it was in the
    /// pool at all. `None` is the interesting answer: it means the search never
    /// offered what turned out to be the right thing.
    pub async fn rank_in_event(&self, event_id: &str, artifact_id: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT rank FROM search_candidates WHERE event_id = ? AND artifact_id = ?",
        )
        .bind(event_id)
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every write here is a verdict on a row that may not be there any more:
    /// retention expires events on a timer and Ops can purge them outright,
    /// both of them under a judging screen that is already open. An UPDATE
    /// matching nothing is not a recorded judgement, and reporting it as one
    /// puts a number in front of the operator that no stored row supports.
    fn judged_one(res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    pub async fn judge_hit(&self, event_id: &str, artifact_id: &str) -> Result<()> {
        Self::judged_one(
            sqlx::query(
                "UPDATE search_events SET judged_at = ?, verdict = 'hit', expect_id = ?
                 WHERE id = ?",
            )
            .bind(now())
            .bind(artifact_id)
            .bind(event_id)
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn judge(&self, event_id: &str, verdict: Verdict) -> Result<()> {
        Self::judged_one(
            sqlx::query("UPDATE search_events SET judged_at = ?, verdict = ? WHERE id = ?")
                .bind(now())
                .bind(verdict.as_str())
                .bind(event_id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Take a verdict back, returning the event to the pending queue.
    ///
    /// A misfired digit is a mislabelled pair, and a mislabelled pair is worse
    /// than no pair at all: it is scored against the ranker as truth. Clearing
    /// `expect_id` with the verdict matters — a stale answer left behind would
    /// keep counting towards recall for a judgement nobody stands behind.
    pub async fn unjudge(&self, event_id: &str) -> Result<()> {
        Self::judged_one(
            sqlx::query(
                "UPDATE search_events
                 SET judged_at = NULL, verdict = NULL, expect_id = NULL
                 WHERE id = ?",
            )
            .bind(event_id)
            .execute(&self.pool)
            .await?,
        )
    }

    /// Not a verdict: the event stays pending and only sinks in the order. An
    /// honest "I don't remember" must never cost anything, or it stops being
    /// honest.
    pub async fn skip_event(&self, event_id: &str) -> Result<()> {
        Self::judged_one(
            sqlx::query("UPDATE search_events SET skips = skips + 1 WHERE id = ?")
                .bind(event_id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// How many searches are waiting for a verdict.
    ///
    /// Split out of `feedback_stats`, which runs half a dozen queries and two
    /// joins: this one is read on every page render to draw the nav, and the
    /// nav must not cost what the ops page costs.
    pub async fn pending_count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM search_events WHERE judged_at IS NULL")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// The field value: recall@10 and MRR read from the ranks the searches
    /// actually gave. No vector store and no embedding are involved, so the
    /// number can move on every single judgement — which is what makes it worth
    /// showing while judging rather than afterwards.
    pub async fn feedback_stats(&self) -> Result<Stats> {
        let mut s = Stats {
            captured: sqlx::query_scalar("SELECT count(*) FROM search_events")
                .fetch_one(&self.pool)
                .await?,
            pending: sqlx::query_scalar(
                "SELECT count(*) FROM search_events WHERE judged_at IS NULL",
            )
            .fetch_one(&self.pool)
            .await?,
            ..Default::default()
        };

        for (field, verdict) in [
            (&mut s.hits, "hit"),
            (&mut s.gaps, "gap"),
            (&mut s.discards, "discard"),
        ] {
            *field = sqlx::query_scalar("SELECT count(*) FROM search_events WHERE verdict = ?")
                .bind(verdict)
                .fetch_one(&self.pool)
                .await?;
        }
        s.judged = s.hits + s.gaps + s.discards;

        // A left join, because an expected artifact that was never returned has
        // no candidate row to join to — and that absence is precisely what a
        // miss is.
        let ranks: Vec<Option<i64>> = sqlx::query(
            "SELECT c.rank AS rank FROM search_events e
             LEFT JOIN search_candidates c
               ON c.event_id = e.id AND c.artifact_id = e.expect_id
             WHERE e.verdict = 'hit'",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| r.get::<Option<i64>, _>("rank"))
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

    /// The queries whose confirmed answer fell outside the first ten. The list
    /// that is actually read: an aggregate says something is wrong, this says
    /// what.
    pub async fn misses(&self, limit: i64) -> Result<Vec<Miss>> {
        Ok(sqlx::query(
            "SELECT e.query AS query, c.rank AS rank FROM search_events e
             LEFT JOIN search_candidates c
               ON c.event_id = e.id AND c.artifact_id = e.expect_id
             WHERE e.verdict = 'hit' AND (c.rank IS NULL OR c.rank >= 10)
             ORDER BY e.judged_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Miss {
            query: r.get("query"),
            rank: r.get("rank"),
        })
        .collect())
    }

    /// Drop captured *unjudged* searches older than the window. `0` keeps them
    /// forever.
    ///
    /// Driven by its own ticker rather than by the consolidation sweep: a
    /// retention window is a promise about personal data, and a promise that
    /// quietly lapses when an unrelated feature is switched off is not one.
    ///
    /// A judged event is exempt. The window exists to stop unexamined searches
    /// accumulating; a verdict is the operator's own considered work, and it is
    /// the only thing this whole feature produces — expiring it would delete
    /// the eval pair silently, move recall and MRR for no visible reason, and
    /// leave `--export-eval` poorer every month. `purge_feedback` still takes
    /// everything, so the way out remains a deliberate one.
    ///
    /// `discard` is not exempt. It is the operator saying this was never a
    /// search — a typo, or poking at the box — and holding those forever would
    /// be keeping exactly the material the window exists to shed.
    pub async fn expire_feedback(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        let searches = sqlx::query(
            "DELETE FROM search_events
                 WHERE created_at < ? AND (verdict IS NULL OR verdict = 'discard')",
        )
        .bind(now() - retain_days * 86_400)
        .execute(&self.pool)
        .await?
        .rows_affected();
        // One promise, both tables. A question is the same class of personal
        // data as a query and ages under the same window.
        Ok(searches + self.expire_asks(retain_days).await?)
    }

    /// Everything captured, gone. Judgements included: they are statements
    /// about queries, and a judgement whose query no longer exists is not a
    /// record of anything.
    pub async fn purge_feedback(&self) -> Result<u64> {
        // `search_candidates` goes with it through ON DELETE CASCADE.
        let searches = sqlx::query("DELETE FROM search_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        // Pursuits and what was opened are the same kind of record — what a
        // person did — and go with one press.
        Ok(searches + self.purge_asks().await? + self.purge_pursuits().await?)
    }

    /// Recorded searches with `from < created_at <= to`, oldest first, with
    /// what the sweep needs and nothing else. The judge door is excluded: a
    /// benchmark query is not a need.
    pub async fn events_between(&self, from: i64, to: i64) -> Result<Vec<RecordedEvent>> {
        let rows = sqlx::query(
            "SELECT id, query, query_vec, created_at, answered, verdict, expect_id, scope
               FROM search_events
              WHERE created_at > ? AND created_at <= ? AND door <> 'judge'
              ORDER BY created_at, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.get("id");
            let shown: Vec<(String, Option<f32>)> = sqlx::query(
                "SELECT artifact_id, similarity FROM search_candidates
                  WHERE event_id = ? AND shown = 1 ORDER BY rank",
            )
            .bind(&id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|c| {
                (
                    c.get::<String, _>("artifact_id"),
                    c.get::<Option<f32>, _>("similarity"),
                )
            })
            .collect();
            let verdict: Option<String> = r.get("verdict");
            out.push(RecordedEvent {
                id,
                query: r.get("query"),
                query_vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                created_at: r.get("created_at"),
                answered: r.get::<i64, _>("answered") != 0,
                confirmed: if verdict.as_deref() == Some("hit") {
                    r.get("expect_id")
                } else {
                    None
                },
                scope: r.get("scope"),
                shown,
            });
        }
        Ok(out)
    }

    /// When the session was last active, if it ever was.
    ///
    /// Every kind of event, not just the ones that open a pursuit. A search is
    /// what a pursuit is built around, but reading is what the operator spends
    /// the session doing: one search at the top of the hour, then twenty
    /// minutes of opening and pivoting through what it returned. Counting only
    /// searches calls that session idle while it is at its most active, and the
    /// sweep that then fires is not merely early — it advances the cursor past
    /// the search, so every interaction still to come arrives with no search of
    /// its own left in range, finds no owner, and is dropped. The long read is
    /// scored as an abandonment, which is the one reading it least resembles.
    ///
    /// The judge door is excluded for the same reason `events_between` excludes
    /// it: a benchmark run is not a session. Counting it kept the base looking
    /// busy for as long as a harness was pointed at it, and the sweep that
    /// waits for quiet then never ran at all.
    pub async fn newest_event_at(&self) -> Result<Option<i64>> {
        let s: Option<i64> =
            sqlx::query_scalar("SELECT MAX(created_at) FROM search_events WHERE door <> 'judge'")
                .fetch_one(&self.pool)
                .await?;
        let a: Option<i64> = sqlx::query_scalar("SELECT MAX(created_at) FROM ask_events")
            .fetch_one(&self.pool)
            .await?;
        let i: Option<i64> = sqlx::query_scalar("SELECT MAX(at) FROM interaction_events")
            .fetch_one(&self.pool)
            .await?;
        Ok([s, a, i].into_iter().flatten().max())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(query: &str, door: Door) -> NewEvent {
        scoped(query, door, None)
    }

    #[test]
    fn only_the_extension_may_name_its_own_door() {
        // The door is how a search is weighted later, so a client that could
        // name any of them could label an `ask` retrieval as a deliberate
        // query and quietly poison the eval set.
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
            }],
            answered: false,
        }
    }

    async fn queries(store: &Store) -> Vec<String> {
        sqlx::query("SELECT query FROM search_events ORDER BY created_at")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("query"))
            .collect()
    }

    #[tokio::test]
    async fn two_operators_typing_at_once_keep_their_own_events() {
        // Folding is a statement about one person's keystrokes. Keyed on the
        // door alone it also folded across people: B's `backup` arriving while
        // A's `backup restore` was the newest pending event was read as an
        // early keystroke of it, and B's search — and its whole pool — was
        // never recorded at all.
        let store = Store::memory().await.unwrap();
        store
            .record_search(scoped("backup restore", Door::Ui, Some("alice")), 15)
            .await
            .unwrap();
        store
            .record_search(scoped("backup", Door::Ui, Some("bob")), 15)
            .await
            .unwrap();

        let mut got = queries(&store).await;
        got.sort();
        assert_eq!(got, vec!["backup", "backup restore"]);
    }

    #[tokio::test]
    async fn deliberate_calls_through_the_other_doors_never_fold() {
        // An agent narrowing `list files` to `list files in dir` made two
        // decisions, and both are worth judging. Only a text box produces the
        // keystroke bursts folding exists for.
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
        for q in ["daten", "datentr", "datenträger nicht erkannt"] {
            store.record_search(ev(q, Door::Ui), 15).await.unwrap();
        }
        assert_eq!(queries(&store).await, vec!["datenträger nicht erkannt"]);
    }

    #[tokio::test]
    async fn a_typing_burst_in_the_extension_panel_collapses_the_same_way() {
        // The panel searches as you type and debounces at 200ms, so one query
        // reaches here as several. Left unfolded, every prefix fragment would
        // become its own row waiting to be judged — the eval set filling with
        // half-words is exactly what folding exists to prevent, and the door
        // it arrived through does not change that.
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
    async fn a_keystroke_that_arrives_late_folds_into_the_wording_it_preceded() {
        // Each keystroke is its own request, so "fat" can be committed after
        // "fat32". Testing only whether the query grew left the earlier
        // keystroke standing as a second event, and the judging queue filled
        // with half-typed prefixes — the thing coalescing exists to prevent.
        let store = Store::memory().await.unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();

        assert_eq!(queries(&store).await, vec!["fat32"]);
        let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(candidates, 1, "the surviving event lost its pool");
    }

    #[tokio::test]
    async fn a_typing_burst_recorded_concurrently_still_collapses() {
        // Every keystroke fires its own background write, so the order they
        // commit in is not the order they were typed in. What this pins is that
        // the burst still folds under any order — the in-memory store runs on
        // one connection, so it cannot reproduce the busy-snapshot failure the
        // capture mutex is there for; only the file-backed store can.
        let store = Store::memory().await.unwrap();
        let mut tasks = Vec::new();
        for n in 1..="datenträger".chars().count() {
            let store = store.clone();
            let q: String = "datenträger".chars().take(n).collect();
            tasks.push(tokio::spawn(async move {
                store.record_search(ev(&q, Door::Ui), 15).await
            }));
        }
        for t in tasks {
            t.await.unwrap().expect("a capture was dropped");
        }

        assert_eq!(queries(&store).await, vec!["datenträger"]);
    }

    #[tokio::test]
    async fn the_same_search_arriving_twice_stays_one_event() {
        // The form fires on load, on submit and on every filter change, so one
        // search reaches capture several times over — and a bookmarked
        // `/ui/search?q=fat32` fires again on every reload. Requiring the
        // query to have *grown* left each repeat standing as its own event:
        // one thing to judge became five, each needing its own verdict, and
        // `--export-eval` emitted five identical pairs that weighted that one
        // query five times over in recall@10 and MRR.
        let store = Store::memory().await.unwrap();
        for _ in 0..3 {
            store
                .record_search(ev("fat32", Door::Ui), 15)
                .await
                .unwrap();
        }

        assert_eq!(queries(&store).await, vec!["fat32"]);
        let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(candidates, 1, "the pool was written once per repeat");
    }

    #[tokio::test]
    async fn the_same_search_outside_the_window_is_asked_again() {
        // Folding is about one burst at the keyboard, not about a query being
        // unique forever. Coming back to the same question an hour later is a
        // second occasion, and it is judged as one.
        let store = Store::memory().await.unwrap();
        let first = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(now() - 3600)
            .bind(&first)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();

        assert_eq!(queries(&store).await, vec!["fat32", "fat32"]);
    }

    #[tokio::test]
    async fn a_query_that_is_not_a_prefix_starts_its_own_event() {
        let store = Store::memory().await.unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        store.record_search(ev("ntfs", Door::Ui), 15).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat32", "ntfs"]);
    }

    #[tokio::test]
    async fn a_prefix_from_another_door_does_not_fold_into_this_one() {
        // Two front doors are two people as far as this is concerned. Folding
        // an MCP call into a half-typed UI query would invent a search nobody
        // made.
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
        // Zero window: the previous event is already too old to extend.
        store.record_search(ev("fat32", Door::Ui), 0).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn folding_replaces_the_candidate_list_rather_than_appending_to_it() {
        // The candidates belong to the query that produced them. Keeping the
        // earlier ones would describe a result list that was never shown.
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut second = ev("fat32", Door::Ui);
        second.candidates[0].artifact_id = "a2".into();
        store.record_search(second, 15).await.unwrap();

        let rows = sqlx::query("SELECT artifact_id FROM search_candidates")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("artifact_id"), "a2");
    }

    #[tokio::test]
    async fn a_narrowing_that_found_nothing_does_not_empty_the_stored_pool() {
        // The search form fires the same `q` again on every filter change, and
        // a repeat folds forward taking the newer pool with it. When the newer
        // pool is empty — a category chip that nothing in the base matches —
        // that left the event holding no candidates, and its card offered
        // nothing to choose: unjudgeable except by skip, gap or discard, on a
        // query that had twenty perfectly good answers a moment earlier. The
        // empty search is still recorded; it just does not overwrite the pool
        // that answered the same words.
        let store = Store::memory().await.unwrap();
        let first = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();

        let mut filtered = ev("fat32", Door::Ui);
        filtered.filters = r#"{"category":"recipes"}"#.into();
        filtered.candidates.clear();
        let second = store.record_search(filtered, 15).await.unwrap();

        assert_ne!(first, second, "the empty search started its own event");
        let kept = sqlx::query("SELECT artifact_id FROM search_candidates WHERE event_id = ?")
            .bind(&first)
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(kept.len(), 1, "the pool that answered the query survived");
    }

    #[tokio::test]
    async fn a_narrowing_that_found_something_still_folds() {
        // The guard is only about emptying a pool. A filter change that returns
        // results is the documented case: one search, judged once, against the
        // narrowing that was actually meant.
        let store = Store::memory().await.unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        let mut filtered = ev("fat32", Door::Ui);
        filtered.filters = r#"{"category":"disks"}"#.into();
        filtered.candidates[0].artifact_id = "a2".into();
        store.record_search(filtered, 15).await.unwrap();

        assert_eq!(queries(&store).await, vec!["fat32"]);
        let rows = sqlx::query("SELECT artifact_id FROM search_candidates")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("artifact_id"), "a2");
    }

    #[tokio::test]
    async fn a_judged_event_is_never_folded_into() {
        // Folding rewrites the query. Doing that under a verdict would leave a
        // label attached to words the operator never judged.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        sqlx::query("UPDATE search_events SET judged_at = ?, verdict = 'gap' WHERE id = ?")
            .bind(now())
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn an_empty_result_list_is_still_captured() {
        // A search that found nothing is the most direct evidence of a gap the
        // system will ever get. It has no candidates and must still be stored.
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
            })
            .collect();
        // No folding: these are separate searches, not one being typed.
        store.record_search(e, 0).await.unwrap()
    }

    #[tokio::test]
    async fn the_newest_unjudged_event_comes_up_first() {
        // Judging is worth something because the situation is still in mind,
        // and that memory is the most perishable part of the dataset.
        let store = Store::memory().await.unwrap();
        seed(&store, "older", &["a"]).await;
        seed(&store, "newer", &["b"]).await;
        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "newer");
    }

    #[tokio::test]
    async fn a_skipped_event_sinks_below_the_ones_never_looked_at() {
        let store = Store::memory().await.unwrap();
        seed(&store, "older", &["a"]).await;
        let newer = seed(&store, "newer", &["b"]).await;
        store.skip_event(&newer).await.unwrap();
        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "older");
    }

    #[tokio::test]
    async fn a_judged_event_does_not_come_back() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "only one", &["a"]).await;
        store.judge_hit(&id, "a").await.unwrap();
        assert!(store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_verdict_on_an_event_that_is_gone_is_refused() {
        // Retention expires events on a timer and Ops can purge them, both
        // under a judging screen that is already open. Reporting success would
        // put an MRR delta and an Undo button in front of the operator for a
        // row that does not exist.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "will be purged", &["a"]).await;
        store.purge_feedback().await.unwrap();

        for res in [
            store.judge_hit(&id, "a").await,
            store.judge(&id, Verdict::Gap).await,
            store.unjudge(&id).await,
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
        // No Qdrant and no embedding: the rank of every candidate was stored
        // when the search happened, so confirming one settles its rank too.
        let store = Store::memory().await.unwrap();
        let first = seed(&store, "top hit", &["a", "b", "c"]).await;
        store.judge_hit(&first, "a").await.unwrap();
        let third = seed(&store, "third hit", &["x", "y", "z"]).await;
        store.judge_hit(&third, "z").await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!(s.judged, 2);
        assert_eq!(s.hits, 2);
        assert!((s.recall_at_10 - 1.0).abs() < 1e-9);
        // 1/1 and 1/3, averaged.
        assert!((s.mrr - (1.0 + 1.0 / 3.0) / 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_answer_outside_the_pool_counts_as_a_find_and_a_miss() {
        // The whole point of the "none of these" path: an artifact the ranker
        // never returned. It has no rank, so it contributes nothing to MRR and
        // it drags recall down — which is the truth about that search.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "found nothing useful", &["a", "b"]).await;
        store.judge_hit(&id, "something-else").await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!(s.finds, 1);
        assert_eq!(s.recall_at_10, 0.0);
        assert_eq!(s.mrr, 0.0);
        assert_eq!(store.misses(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gaps_and_discards_are_counted_but_are_not_pairs() {
        let store = Store::memory().await.unwrap();
        let g = seed(&store, "nothing written about this", &[]).await;
        store.judge(&g, Verdict::Gap).await.unwrap();
        let d = seed(&store, "asdf", &["a"]).await;
        store.judge(&d, Verdict::Discard).await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!((s.gaps, s.discards, s.hits), (1, 1, 0));
        // Neither can score: one has no answer, the other was not a question.
        assert_eq!(s.mrr, 0.0);
    }

    #[tokio::test]
    async fn an_event_is_read_by_id_not_by_judging_order() {
        // The assign screen asks for the event it was opened on. A capture
        // landing while it is open takes the front of the judging order, and
        // must not take the query off the screen with it.
        let store = Store::memory().await.unwrap();
        let older = seed(&store, "older", &["a"]).await;
        seed(&store, "newer", &["b"]).await;

        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "newer");
        assert_eq!(
            store.event_query(&older).await.unwrap().as_deref(),
            Some("older")
        );
        assert_eq!(store.event_query("no such event").await.unwrap(), None);
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
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(now() - 40 * 86_400)
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_judged_event_outlives_the_retention_window() {
        // The window is for unexamined searches. A verdict is the operator's
        // own work and the only thing the feature produces: expiring it would
        // delete an eval pair silently and move recall for no visible reason.
        let store = Store::memory().await.unwrap();
        let kept = seed(&store, "judged", &["a"]).await;
        let gone = seed(&store, "never looked at", &["a"]).await;
        store.judge_hit(&kept, "a").await.unwrap();
        sqlx::query("UPDATE search_events SET created_at = ?")
            .bind(now() - 40 * 86_400)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
        assert_eq!(
            store.event_query(&kept).await.unwrap().as_deref(),
            Some("judged")
        );
        assert_eq!(store.event_query(&gone).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_discarded_event_expires_like_any_other() {
        // `discard` is the operator saying this was never a search. Holding
        // typos forever is keeping exactly what the window exists to shed.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "asdf", &["a"]).await;
        store.judge(&id, Verdict::Discard).await.unwrap();
        sqlx::query("UPDATE search_events SET created_at = ?")
            .bind(now() - 40 * 86_400)
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

        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_query_vector_survives_a_round_trip_through_the_blob() {
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let row = sqlx::query("SELECT query_vec, vec_dim FROM search_events WHERE id = ?")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("vec_dim"), 2);
        assert_eq!(
            blob_to_vec(&row.get::<Vec<u8>, _>("query_vec")),
            vec![0.5, -0.25]
        );
    }

    #[tokio::test]
    async fn events_between_reads_answered_confirmed_and_what_was_shown() {
        let store = Store::memory().await.unwrap();
        let mut e = ev("a question", Door::Ui);
        e.answered = true;
        let id = store.record_search(e, 0).await.unwrap();
        store.judge_hit(&id, "a1").await.unwrap();
        let now = now();
        let got = store.events_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].answered);
        assert_eq!(got[0].confirmed.as_deref(), Some("a1"));
        assert_eq!(got[0].shown, vec![("a1".to_string(), Some(0.8))]);
        assert_eq!(got[0].query_vec, vec![0.5, -0.25]);
        // The judge door is not a need.
        store
            .record_search(ev("bench", Door::Judge), 0)
            .await
            .unwrap();
        assert_eq!(store.events_between(0, now + 1).await.unwrap().len(), 1);
        // Nor is it a session: a harness pointed at the base kept `idle` from
        // ever elapsing, and the sweep that waits for quiet never ran at all.
        assert_eq!(store.newest_event_at().await.unwrap(), Some(now));
        // The forget button takes the pursuits along.
        store
            .insert_pursuit(now, &["q".into()], &[], None)
            .await
            .unwrap();
        store.purge_feedback().await.unwrap();
        assert!(store.recent_pursuits(10).await.unwrap().is_empty());
    }
}
