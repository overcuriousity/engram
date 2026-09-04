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
    /// A search made from the terminal client. Recorded like `Ui` and `Api`,
    /// and distinguished from them for the reason `Extension` is: a query
    /// typed at a shell is composed before anything came back, about
    /// something the operator is looking at rather than something engram
    /// showed them.
    Cli,
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
            Door::Cli => "cli",
            Door::Judge => "judge",
            Door::Ask => "ask",
        }
    }

    pub fn captured(&self) -> bool {
        matches!(
            self,
            Door::Ui | Door::Api | Door::Mcp | Door::Extension | Door::Cli
        )
    }

    /// The door a client is allowed to claim for itself.
    ///
    /// Only `extension` and `cli`. Everything else falls back to `Api`,
    /// because a client
    /// that could name `Ask` or `Judge` could mark a contaminated query as a
    /// clean one — or have a real one silently dropped — which is the exact
    /// thing the judging loop exists to prevent.
    pub fn from_client(raw: &str) -> Door {
        match raw {
            "extension" => Door::Extension,
            "cli" => Door::Cli,
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
    /// The event this search is a rewording of, named by the page that is
    /// typing. See `NewEvent::fold_onto`.
    pub fold_onto: Option<String>,
}

impl From<Door> for Origin {
    fn from(door: Door) -> Self {
        Origin {
            door,
            scope: None,
            session: None,
            fold_onto: None,
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
            fold_onto: None,
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

    /// The search event the page sending this one is holding, where the door
    /// can name one. See `NewEvent::fold_onto`.
    pub fn folding_onto(mut self, event_id: Option<String>) -> Origin {
        self.fold_onto = event_id;
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
    /// The event this one is a rewording of, as named by the page that is
    /// typing — the id it was handed by the last answer it drew. `None` from a
    /// door with nothing to name. See the fold rule in `record_search`.
    pub fold_onto: Option<String>,
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
    /// is captured, and any query from the same searcher within
    /// `coalesce_secs` — longer, shorter or the same words again — replaces the
    /// one before it. What survives is the final wording: the query that was
    /// actually meant, asked once, holding the pool that answered it.
    ///
    /// Only text boxes fold, and only into the event the box is holding — see
    /// `fold_onto` and the rule below it. That is the web UI's
    /// search field and the browser extension's panel, both of which search as
    /// you type — the panel debounces at 200ms, so one query still arrives as
    /// several. A call through the API or MCP is a deliberate query, and an
    /// agent narrowing one search into a longer one made two decisions worth
    /// judging separately.
    pub async fn record_search(&self, ev: NewEvent, coalesce_secs: i64) -> Result<String> {
        // A write transaction from the first statement. This reads the previous
        // event and then writes over it, and a deferred transaction would take
        // the read snapshot first and only upgrade at the `UPDATE` — which two
        // overlapping captures answer with `SQLITE_BUSY_SNAPSHOT`, a failure no
        // `busy_timeout` waits out, and a lost capture is a search nobody can
        // judge. Taking the write lock up front makes the read and the write
        // one atomic thing and leaves the queueing to SQLite's busy timeout,
        // where a second writer waits instead of failing. It also means the UI
        // door, which now awaits this on every keystroke, queues only behind
        // another write to the same file rather than behind a lock held across
        // every door at once.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let at = now();

        // Which stored event this one may fold into, and the two doors answer
        // that differently because only one of them can name a row.
        //
        // The web UI names it: every answer hands the page the id it was
        // recorded under, and the box sends that id back with the next
        // keystroke. So a fold is onto that one row and no other. Keyed on
        // scope alone it was onto whatever the searcher wrote last through this
        // door, which is the same row for every tab they have open — a second
        // window searching inside the window overwrote the first one's query
        // and its whole pool, while the first one's rail went on naming the id
        // on every row. The open that followed was then either refused (the
        // artifact is no longer in the pool) or scored against a query nobody
        // in that tab had typed.
        //
        // The extension panel names nothing — the API answer it reads carries
        // no event id — so it still folds by the searcher, and two panels of
        // one browser can still collide inside the window. `scope IS ?` rather
        // than `=` so an event recorded without a subject finds its own
        // predecessor instead of matching nothing.
        //
        // Both carry the same two guards, and for the same reason the query
        // below is guarded at all: a judged or opened event is finished, and
        // folding into one would rewrite the search a verdict already answered.
        let prev = match ev.door {
            Door::Ui => match ev.fold_onto.as_deref() {
                Some(prev_id) => {
                    sqlx::query(
                        "SELECT id, created_at,
                                (SELECT COUNT(*) FROM search_candidates
                                  WHERE event_id = search_events.id) AS pool
                           FROM search_events
                          WHERE id = ? AND door = ? AND scope IS ?
                            AND judged_at IS NULL AND opened_at IS NULL",
                    )
                    .bind(prev_id)
                    .bind(ev.door.as_str())
                    .bind(&ev.scope)
                    .fetch_optional(&mut *tx)
                    .await?
                }
                // The first search of a page, which has nothing to fold into.
                None => None,
            },
            Door::Extension => {
                sqlx::query(
                    "SELECT id, created_at,
                            (SELECT COUNT(*) FROM search_candidates
                              WHERE event_id = search_events.id) AS pool
                       FROM search_events
                      WHERE door = ? AND scope IS ? AND judged_at IS NULL
                        AND opened_at IS NULL
                      ORDER BY created_at DESC, id DESC LIMIT 1",
                )
                .bind(ev.door.as_str())
                .bind(&ev.scope)
                .fetch_optional(&mut *tx)
                .await?
            }
            _ => None,
        };

        // Same burst, whatever the wording. Everything inside the window folds
        // forward: a rewording is the same need said again, and a burst of
        // three wordings ending in one open used to be three cards, two of them
        // about words nobody meant.
        //
        // A shorter query folds like any other, and used to not. The rule that
        // dropped it read a shorter prefix as a keystroke arriving late — "fat"
        // landing after "fat32" — and kept the longer stored wording as the one
        // that was meant. Backspacing is indistinguishable from that at the
        // point of the write and is not rare: the box searches as you type, so
        // deleting a word is an ordinary edit. What the stale rule left behind
        // was an event holding a *different* query and a *different* pool than
        // the rail beside it was showing, named on every row of it — so an open
        // was stamped, and a verdict later scored, against a search nobody was
        // looking at. Folding forward keeps the stored event and what was
        // rendered the same thing; the race it gave up on was already the rule
        // everywhere else in this match, where the last write wins.
        //
        // The one thing that stops a fold is an open — `opened_at` above —
        // because the pool that was read is the pool the hit is scored on.
        // The same query twice (the form fires on load, on submit and on every
        // filter change) folds forward like any other, taking the newer
        // filters and pool with it.
        enum Fold {
            /// The stored event is an earlier wording of this one.
            Extends(String),
            New,
        }
        let fold = match prev.as_ref() {
            Some(r) => {
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

/// Who gave a verdict. See `search_events.judged_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Labeller {
    /// The judge deck — retired; its rows remain. Stored as NULL, which is
    /// what every verdict before the column existed was.
    Deck,
    /// The bar under an opened result, or the gap button on the rail: at the
    /// moment, by the person who searched.
    ///
    /// The only other labeller there is. A read long enough used to count as a
    /// hit on its own, and it is gone: what it measured was a page staying
    /// open, which is a tab left behind as often as it is an answer, and it
    /// arrived *after* the click it was overwriting — the beacon flushes when
    /// the pane is left, so it landed on top of "not sure" and "undo" and put
    /// a hit back that a person had just taken away. A verdict now comes from
    /// somebody saying so.
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

/// The answerable set: what `pending_count` counts. One bind, `weak_below`.
///
/// An `EXISTS` and two `COALESCE`s, for three different absences. The `EXISTS`:
/// an empty pool is not a card, because a search that returned nothing is a
/// hole — there is no list to point at, so the only answers left are gap and
/// discard, and neither is worth spending a person's turn on. It is not thereby
/// lost: `unmatched_gaps_from!` in `store::gaps` names the empty pool
/// explicitly and the sweep raises it as a hole in the base, which is what it
/// is. (It did not, once, and the search fell through the crack between the
/// two.) Said as an `EXISTS` and not left to the outer `COALESCE` below, which
/// reads an empty pool as the weakest search there is: that holds the empty
/// pool out at every threshold but the lowest one, and `weak_below = 0.0` is a
/// supported setting — it turns the labelling off — where `0 >= 0` let exactly
/// the unanswerable card through and raised the same search as a gap beside
/// it. The outer one still stands, because it is what the threshold means. The
/// inner one: a candidate the vector half
/// never scored reads as the *strongest*, because a similarity nobody measured
/// is not evidence of a weak match. A hit found by the lexical half alone
/// carries none — `search_inner` stores what the vector search returned, and
/// with the embedder down or the query answered on keywords it returns nothing
/// — and reading those as zero withheld every such search from the deck
/// silently, when a keyword search that found something is exactly the card a
/// person can answer.
/// `skips = 0` is the same rule applied to the person rather than to the data:
/// somebody looked at this search, said "not sure", and is not going to be
/// asked it again. While the judge deck existed, `ORDER BY skips ASC` was that
/// second asking and the count was honest. The deck is gone, nothing reads
/// `skips` any more, and every skipped search stayed `judged_at IS NULL` for
/// ever — permanently inflating the "N waiting" figure on Settings and
/// Insights with questions that had already been answered as far as anyone was
/// ever going to answer them. The column is still written, and is still what
/// tells a skipped search from one nobody has seen.
macro_rules! dealable {
    () => {
        "judged_at IS NULL AND skips = 0 AND length(query) >= 3
         AND EXISTS (SELECT 1 FROM search_candidates WHERE event_id = search_events.id)
         AND COALESCE((SELECT max(COALESCE(similarity, 1.0)) FROM search_candidates
                        WHERE event_id = search_events.id), 0) >= ?"
    };
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub captured: i64,
    /// Searches still waiting for a verdict *and* answerable — the
    /// `dealable!` set `pending_count` counts. Every screen showing this says
    /// "waiting", and a number counting questions nobody can answer is a
    /// queue that never empties.
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

/// A query and the artifact a person said answered it.
#[derive(Debug, Clone)]
pub struct JudgedPair {
    pub query: String,
    pub expect: String,
}

impl Store {
    /// Every write here is a verdict on a row that may not be there any more:
    /// retention expires events on a timer and Ops can purge them outright,
    /// both of them under a screen that is already open. An UPDATE matching
    /// nothing is not a recorded judgement, and reporting it as one puts a
    /// number in front of the operator that no stored row supports.
    fn judged_one(res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// `AND judged_at IS NULL`, as `decline` and `gap_event` both carry: the
    /// bar under an opened result is drawn against an unjudged search, and
    /// another tab can answer that same search in the time this one is left
    /// open. Without the guard, Yes in the stale tab overwrote whatever was
    /// recorded — a gap became a hit, and `pairs.json` gained a pair nobody
    /// meant.
    pub async fn judge_hit(&self, event_id: &str, artifact_id: &str, by: Labeller) -> Result<()> {
        Self::judged_one(
            sqlx::query(
                "UPDATE search_events
                 SET judged_at = ?, verdict = 'hit', expect_id = ?, judged_by = ?
                 WHERE id = ? AND judged_at IS NULL",
            )
            .bind(now())
            .bind(artifact_id)
            .bind(by.as_sql())
            .bind(event_id)
            .execute(&self.pool)
            .await?,
        )
    }

    /// The deck's gap and discard, and the same `AND judged_at IS NULL` the
    /// other three writes carry — for the same race, read from the other side.
    /// A card sits on screen while the searcher who made that search opens a
    /// result from it and presses Yes in the workspace; unguarded, G on the
    /// stale card turned their confirmed hit into a gap, dropped the pair from
    /// `pairs.json`, and left `expect_id` naming an artifact on a `gap` row —
    /// a state no other path can produce. `expect_id = NULL` beside it, as
    /// `gap_event` writes it: with the guard an unjudged row never holds one,
    /// and the two writes that mean the same thing should not differ.
    ///
    /// `NotFound` is what a refusal reads as here, which is what the deck
    /// already does with `judge_hit`: the deck deals only unjudged events, so
    /// for it the answer means what it always meant — that row is gone.
    pub async fn judge(&self, event_id: &str, verdict: Verdict, by: Labeller) -> Result<()> {
        Self::judged_one(
            sqlx::query(
                "UPDATE search_events
                 SET judged_at = ?, verdict = ?, expect_id = NULL, judged_by = ?
                 WHERE id = ? AND judged_at IS NULL",
            )
            .bind(now())
            .bind(verdict.as_str())
            .bind(by.as_sql())
            .bind(event_id)
            .execute(&self.pool)
            .await?,
        )
    }

    /// Whether this event was recorded by this searcher.
    ///
    /// Every route that labels a search takes the event id from the page — the
    /// `?event=` on a rail link, the id in the verdict and gap posts — because
    /// the page is what knows which search a row came from. An id is not a
    /// capability, though: on an install with more than one person, anything
    /// that acted on the id alone let one person stamp opens and verdicts onto
    /// another's searches, and quietly break their coalescing besides, by
    /// guessing at ids that are not secret. So the caller has to own the row.
    ///
    /// `scope = ?` and not `IS`: every search that can reach these routes came
    /// through the UI door, and the UI door always names its subject (see
    /// `Origin`). A row with no scope is not one of them.
    pub async fn event_is_mine(&self, event_id: &str, scope: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM search_events WHERE id = ? AND scope = ?")
                .bind(event_id)
                .bind(scope)
                .fetch_optional(&self.pool)
                .await?
                .is_some(),
        )
    }

    /// The search a result was opened from, named by the page that listed it.
    /// Stamped `opened_at`, which is what stops the next rewording folding into
    /// it — see `record_search`.
    ///
    /// Named rather than guessed: the UI door waits for its own capture (see
    /// `Core::search_inner`) so every rail row carries the id of the search
    /// that produced it. What this replaced looked for the newest recent event
    /// whose pool happened to hold the artifact, which had two ways to be
    /// wrong — a click arriving before the background write found nothing, and
    /// one arriving an hour later could be answered by a different search
    /// entirely, then labelled by a read that had nothing to do with it.
    ///
    /// The artifact is named as well as the event, and the pool is checked for
    /// it: a search still in flight when the click lands takes this very row as
    /// its predecessor — `opened_at` is still NULL at the moment it reads —
    /// and folds forward over the query, the filters and the whole candidate
    /// set before this statement runs. Stamping that row regardless drew the
    /// bar against a search the artifact was never in, so a Yes recorded a hit
    /// on a pool that never held it. The window is one type-ahead debounce
    /// wide, and it is the same misattribution the named event exists to stop.
    ///
    /// `false` where the event is gone — retention expires them, Ops purges
    /// them — or where a person has already spoken for it, or where the fold
    /// above happened. That is what decides whether the bar under the artifact
    /// is drawn at all.
    pub async fn open_event(&self, event_id: &str, artifact_id: &str) -> Result<bool> {
        let generation = self.live_generation().await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // Read before the stamp, because the stamp destroys the thing worth
        // knowing: whether this event had been opened already. The UPDATE below
        // is deliberately left exactly as it was — it has never carried an
        // `opened_at IS NULL` guard, and adding one would change when the
        // verdict bar is drawn. So the guard goes on the observation instead,
        // which is new and answers to nothing.
        //
        // The join is also the membership check: no row here means this
        // artifact was not in this event's pool, which is the same thing the
        // EXISTS below refuses on.
        let before = sqlx::query(
            "SELECT e.opened_at AS opened_at, e.query AS query,
                    e.query_vec AS query_vec, e.embed_model AS embed_model,
                    c.rank AS rank
               FROM search_events e
               JOIN search_candidates c
                 ON c.event_id = e.id AND c.artifact_id = ?
              WHERE e.id = ?",
        )
        .bind(artifact_id)
        .bind(event_id)
        .fetch_optional(&mut *tx)
        .await?;

        let stamped = sqlx::query(
            "UPDATE search_events SET opened_at = ?
              WHERE id = ? AND judged_at IS NULL
                AND EXISTS (SELECT 1 FROM search_candidates
                             WHERE event_id = ? AND artifact_id = ?)",
        )
        .bind(now())
        .bind(event_id)
        .bind(event_id)
        .bind(artifact_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        // An open is a deliberate act on a list somebody read: the strongest
        // thing a plain search can say, and the only one it says out loud.
        if let (true, Some(g), Some(row)) = (stamped, &generation, &before)
            && row.get::<Option<i64>, _>("opened_at").is_none()
        {
            crate::store::observations::insert(
                &mut *tx,
                &crate::store::observations::NewObservation {
                    generation_id: g.id.clone(),
                    query: row.get("query"),
                    query_vec: blob_to_vec(&row.get::<Vec<u8>, _>("query_vec")),
                    embed_model: row.get("embed_model"),
                    artifact_id: Some(artifact_id.to_string()),
                    // `search_candidates.rank` counts from zero; `observations.rank`
                    // and `ask_citations.n` both count from one. Converted here
                    // rather than left for whoever compares an opened result with
                    // a cited excerpt and finds them a place apart.
                    rank: Some(row.get::<i64, _>("rank") + 1),
                    source: crate::store::observations::Source::Opened,
                },
            )
            .await?;
        }

        tx.commit().await?;
        Ok(stamped)
    }

    /// "Not this one": the search stays a question for the deck, and the column
    /// records that the answer came from a person rather than the deck.
    ///
    /// `AND judged_at IS NULL`, as `open_event` and `gap_event` both carry, and
    /// for a sharper reason than either: this clears the verdict columns, so
    /// without the guard it is the one answer on the bar that *destroys* a
    /// label. The bar is drawn against an unjudged event, but the deck can
    /// answer the same search in the time a tab is left open, and pressing No
    /// in that tab used to silently delete a confirmed pair out of
    /// `pairs.json`. `false` where that has happened — the same two ordinary
    /// outcomes `gap_event` reports.
    pub async fn decline(&self, event_id: &str) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE search_events
             SET judged_at = NULL, verdict = NULL, expect_id = NULL, judged_by = 'confirm'
             WHERE id = ? AND judged_at IS NULL",
        )
        .bind(event_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// The rail's "nothing here has it": a gap against the search the rail was
    /// filled by, named by the page as an open is. `false` where there was
    /// nothing left to label — the event was purged, or already judged.
    ///
    /// Not `judge`, which would report a second click as an error and a purged
    /// event as the same error: the button has two outcomes and both of them
    /// are ordinary.
    ///
    /// `AND query = ?` is this button's version of the check `open_event` makes
    /// against the pool: the row may have been folded into by a trailing
    /// keystroke between the rail being rendered and the button being pressed,
    /// and the gap has to be recorded against the query that was on the screen.
    /// Without it, "nothing here has it" could land on a later wording that did
    /// return something — a hole reported in the base over a search that
    /// answered. `false` where the query has moved on, which is the same
    /// ordinary outcome as a search already judged.
    pub async fn gap_event(&self, event_id: &str, query: &str) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE search_events
             SET judged_at = ?, verdict = ?, expect_id = NULL, judged_by = ?
             WHERE id = ? AND query = ? AND judged_at IS NULL",
        )
        .bind(now())
        .bind(Verdict::Gap.as_str())
        .bind(Labeller::Confirm.as_sql())
        .bind(event_id)
        .bind(query)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Take a verdict back, returning the event to the pending queue.
    ///
    /// A misfired digit is a mislabelled pair, and a mislabelled pair is worse
    /// than no pair at all: it is scored against the ranker as truth. Clearing
    /// `expect_id` with the verdict matters — a stale answer left behind would
    /// keep counting towards recall for a judgement nobody stands behind.
    ///
    /// Takes back only what this labeller gave: `judged_by IS ?`, which is the
    /// deck's NULL or the bar's `confirm`. An undo is a second thought about
    /// one's own answer, and unguarded it was a way to erase somebody else's —
    /// press No on the bar (which leaves the search pending), let the deck deal
    /// it and record a hit, then press undo in the tab still holding the bar,
    /// and the confirmed pair was gone. The same failure `decline` is guarded
    /// against, on the button beside it.
    pub async fn unjudge(&self, event_id: &str, by: Labeller) -> Result<()> {
        Self::judged_one(
            sqlx::query(
                // `skips` goes back with the verdict. `dealable!` excludes a
                // skipped event, so a search skipped once, judged later and
                // then undone was `judged_at IS NULL` — outstanding — and
                // still excluded from `pending_count` and `Stats::pending`
                // for ever: genuinely waiting and invisible on Settings and
                // Insights. An undo puts the row back where it was before
                // anybody answered it.
                "UPDATE search_events
                 SET judged_at = NULL, verdict = NULL, expect_id = NULL, judged_by = NULL,
                     skips = 0
                 WHERE id = ? AND judged_by IS ?",
            )
            .bind(event_id)
            .bind(by.as_sql())
            .execute(&self.pool)
            .await?,
        )
    }

    /// Not a verdict: the event keeps `judged_at IS NULL`, so it is never
    /// counted as answered, never enters the eval pairs, and never becomes a
    /// discard — an honest "I don't remember" must not cost the search
    /// anything, or it stops being honest.
    ///
    /// What it does cost is being asked again: `dealable!` excludes a skipped
    /// event, so the row leaves the "waiting" figure. The alternative, now that
    /// the deck that re-dealt skipped cards is gone, is a question that is
    /// counted as outstanding for ever and put to nobody.
    pub async fn skip_event(&self, event_id: &str) -> Result<()> {
        Self::judged_one(
            sqlx::query("UPDATE search_events SET skips = skips + 1 WHERE id = ?")
                .bind(event_id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// How many searches are waiting for a verdict — the `pending` figure in
    /// `Stats`, split out so the tests can pin the answerable set on its own.
    ///
    /// Not a single indexed count — the figure counts what can actually be
    /// answered, or it reads "12 waiting" over questions nobody can take — so
    /// `dealable!` is two correlated subqueries per row it looks at. Both
    /// seek `search_candidates` by `event_id`, which leads the primary key and
    /// `idx_candidates_similarity`, so each is an index seek rather than a
    /// scan; what it costs is that pair of seeks per unjudged event. With
    /// `retain_days = 0` — the default, where nothing is ever trimmed — that
    /// set only grows, and the events this predicate holds back (a query under
    /// three characters, a pool of nothing) stay in it forever. If this count
    /// ever becomes the slow part of a page, that is the thing to measure.
    pub(crate) async fn pending_count(&self, weak_below: f32) -> Result<i64> {
        Ok(sqlx::query_scalar(concat!(
            "SELECT count(*) FROM search_events WHERE ",
            dealable!()
        ))
        .bind(weak_below)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The field value: recall@10 and MRR read from the ranks the searches
    /// actually gave. No vector store and no embedding are involved, so the
    /// number can move on every single judgement — which is what makes it worth
    /// showing while judging rather than afterwards.
    pub async fn feedback_stats(&self, weak_below: f32) -> Result<Stats> {
        let mut s = Stats {
            captured: sqlx::query_scalar("SELECT count(*) FROM search_events")
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

    /// Every judgement that names an answer: the dataset a tuning sweep
    /// replays, and the same rows `--export-eval` freezes into `pairs.json`.
    ///
    /// Gaps and discards are verdicts but not pairs — neither names an
    /// artifact, so replaying one would be a query the ranking can only fail.
    pub async fn judged_pairs(&self) -> Result<Vec<JudgedPair>> {
        Ok(sqlx::query(
            "SELECT query, expect_id FROM search_events
             WHERE verdict = 'hit' AND expect_id IS NOT NULL
             ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| JudgedPair {
            query: r.get("query"),
            expect: r.get("expect_id"),
        })
        .collect())
    }

    /// Verdicts given since `since`. What the day's counter on the judge page
    /// reads, so it counts the work done rather than the pairs produced.
    pub async fn judged_since(&self, since: i64) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM search_events WHERE judged_at >= ?")
                .bind(since)
                .fetch_one(&self.pool)
                .await?,
        )
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
        // The situations those page views happened in, and the profiles built
        // from them. Both, and not one: the clusters are derived, but a
        // centroid *is* the situations that formed it — a device, an hour, a
        // weekday, averaged — so deleting the events and keeping the profiles
        // would leave the offer still saying "Pattern · weekday, hour, device"
        // out of data the person just asked to be rid of. The centroids also
        // live on the points, which a DELETE here cannot reach —
        // `Core::forget_situations` clears those first, and the caller that
        // has a vector store runs the two together.
        let situations = sqlx::query("DELETE FROM context_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        let profiles = sqlx::query("DELETE FROM context_clusters")
            .execute(&self.pool)
            .await?
            .rows_affected();
        // Pursuits and what was opened are the same kind of record — what a
        // person did — and go with one press.
        Ok(searches
            + situations
            + profiles
            + self.purge_asks().await?
            + self.purge_pursuits().await?)
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
        // `recommended_shown` is excluded for the same reason the judge door is:
        // it is not a session. That row is written on every page view — it is
        // the base talking to itself, not a person doing something — so
        // counting it keeps the base looking busy for as long as a tab is open,
        // and the sweep that waits for quiet then never runs at all.
        //
        // `recommended_open` is not excluded. Taking the offer is a real act.
        let i: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(at) FROM interaction_events WHERE kind <> 'recommended_shown'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok([s, a, i].into_iter().flatten().max())
    }
}

#[cfg(test)]
mod tests {

    /// A store with one generation live, and a helper that lists artifacts in
    /// the order given so a rank is a known quantity.
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
                })
                .collect(),
            answered: false,
            fold_onto: None,
        }
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
        // `open_event` already refuses this. The observation must not outlive
        // the refusal, or a positive would be recorded against a list that
        // never held the artifact it names.
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
        // The second open affects no row because `opened_at` is already set.
        // Asserted rather than assumed: it is what stops a double click being
        // double evidence.
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
        // The gate that matters: a contaminated query still cannot label
        // itself clean, and a real one cannot be made to disappear.
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
        // The pursuit sweep waits for quiet, and `recommended_shown` is written
        // on every page view — the base talking to itself, not a person doing
        // something. Counted, it would keep the base looking active for as long
        // as a tab is open and the sweep would never run. This is the judge
        // door's bug, one table over.
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "x".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
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

        // Taking it does. That is a person doing something.
        store
            .record_recommendation(&aid, "recommended_open", "{}", Some("me"), 9_500)
            .await
            .unwrap();
        assert_eq!(store.newest_event_at().await.unwrap(), Some(9_500));
    }
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
            }],
            answered: false,
        }
    }

    /// The next keystroke from a page already holding `prev` — what the box
    /// sends once an answer has named the event it was recorded under.
    fn after(prev: &str, query: &str, door: Door) -> NewEvent {
        NewEvent {
            fold_onto: Some(prev.to_string()),
            ..ev(query, door)
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
        let alice = store
            .record_search(scoped("backup restore", Door::Ui, Some("alice")), 15)
            .await
            .unwrap();
        // Bob's page naming Alice's event, which an id off a page is always
        // free to do — it is not a capability. The scope is what refuses.
        let mut bob = scoped("backup", Door::Ui, Some("bob"));
        bob.fold_onto = Some(alice.clone());
        store.record_search(bob, 15).await.unwrap();

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
        // The box carries the id forward from each answer, the way the page
        // does: one chain of keystrokes, folding into its own event.
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
    async fn a_shorter_query_folds_forward_like_any_other() {
        // Deleting a word is an ordinary edit — the box searches as you type —
        // and it used to be read as a half-typed keystroke arriving late, so
        // the longer wording stayed and the shorter one was thrown away. What
        // that left was a stored event holding a different query and a
        // different pool than the rail beside it was showing, named on every
        // row of it, so an open was stamped and a verdict scored against a
        // search nobody was looking at. The record follows the box.
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
        let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(candidates, 1, "and the pool that answered it");
    }

    #[tokio::test]
    async fn a_typing_burst_recorded_concurrently_still_collapses() {
        // Every keystroke fires its own background write, so the order they
        // commit in is not the order they were typed in. What this pins is that
        // the burst still folds under any order — the in-memory store runs on
        // one connection, so it cannot reproduce the busy-snapshot failure the
        // capture mutex is there for; only the file-backed store can.
        let store = Store::memory().await.unwrap();
        // Every one of them names the id the page was holding when it fired,
        // which is the same id for a burst still in flight.
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
        // The form fires on load, on submit and on every filter change, so one
        // search reaches capture several times over — and a bookmarked
        // `/ui/search?q=fat32` fires again on every reload. Requiring the
        // query to have *grown* left each repeat standing as its own event:
        // one thing to judge became five, each needing its own verdict, and
        // `--export-eval` emitted five identical pairs that weighted that one
        // query five times over in recall@10 and MRR.
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
            .record_search(after(&first, "fat32", Door::Ui), 15)
            .await
            .unwrap();

        assert_eq!(queries(&store).await, vec!["fat32", "fat32"]);
    }

    #[tokio::test]
    async fn a_rewording_within_the_window_folds_into_one_event() {
        // A burst of searches that ends in one open is one need, worded three
        // ways. Folded only on prefix, each wording was its own card — three
        // questions about one thing, two of them about words nobody meant.
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
        sqlx::query_scalar("SELECT judged_by FROM search_events WHERE id = ?")
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn opening_a_result_stamps_the_search_the_page_named() {
        // The UI door waits for its own capture, so the page carries the id and
        // this is a stamp rather than a guess.
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

        // A person having answered closes it: the bar is not drawn again over
        // a search that has been spoken for.
        store.judge_hit(&id, "a1", Labeller::Confirm).await.unwrap();
        assert!(!store.open_event(&id, "a1").await.unwrap());
    }

    #[tokio::test]
    async fn a_fold_that_took_the_artifact_out_of_the_pool_refuses_the_open() {
        // The click and the next keystroke race: a search still in flight when
        // the link is followed reads this very row as its predecessor —
        // nothing has stamped `opened_at` yet — and folds forward over the
        // query and the whole pool. Stamping it anyway drew the verdict bar
        // against a search the artifact was never in.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut later = after(&id, "fat32", Door::Ui);
        later.candidates[0].artifact_id = "a2".into();
        assert_eq!(store.record_search(later, 15).await.unwrap(), id, "folded");
        assert!(
            !store.open_event(&id, "a1").await.unwrap(),
            "the pool the rail was showing is gone, so there is nothing to open against"
        );
        // The row the fold left behind is still openable on its own terms.
        assert!(store.open_event(&id, "a2").await.unwrap());
    }

    #[tokio::test]
    async fn yes_cannot_overwrite_a_verdict_somebody_else_gave() {
        // The other half of the same race, on the button beside "no": the deck
        // calls the search a gap while the tab holding the bar is open, and Yes
        // in that stale tab replaced the gap with a hit — a pair in
        // `pairs.json` nobody meant to put there.
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
        // "No" leaves the search pending, so the deck can deal it and record a
        // hit while the bar — now showing undo — is still on screen. Pressing
        // it wiped the confirmed pair, which is what `decline` two tests up is
        // guarded against; the undo beside it was not.
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
        // The deck's own undo still reaches the deck's own verdict.
        store.unjudge(&id, Labeller::Deck).await.unwrap();
        assert_eq!(store.feedback_stats(0.0).await.unwrap().hits, 0);
    }

    #[tokio::test]
    async fn two_windows_of_one_searcher_do_not_fold_into_each_other() {
        // Keyed on the searcher alone, a second window searching inside the
        // window folded into the first one's event: it overwrote that query
        // and its whole pool while the first window's rail went on naming the
        // id on every row, so the open that followed was refused — or scored
        // against words nobody in that window had typed. Each page names the
        // event it is holding, and holds a different one.
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

        // Both go on typing, and each stays its own.
        let mut b = scoped("ntfs mount", Door::Ui, Some("me"));
        b.fold_onto = Some(tab_b.clone());
        assert_eq!(store.record_search(b, 15).await.unwrap(), tab_b);
        let mut got = queries(&store).await;
        got.sort();
        assert_eq!(got, vec!["fat32", "ntfs mount"]);
        // And the first window's rail can still be answered.
        assert!(store.open_event(&tab_a, "a1").await.unwrap());
    }

    #[tokio::test]
    async fn the_waiting_count_reads_the_pool_through_an_index() {
        // `pending_count` backs the waiting figure, and `dealable!`
        // asks two things of `search_candidates` for every unjudged event it
        // looks at. Both have to be index reads: with `retain_days = 0` — the
        // default, where nothing is trimmed — the set they run over only
        // grows, and the events this predicate holds back stay in it forever.
        let store = Store::memory().await.unwrap();
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
        // The rail is drawn, a trailing keystroke folds a later wording into
        // the same row, and only then is "nothing here has it" pressed. The
        // gap belongs to the words that were on the screen: recorded against
        // the newer ones it reports a hole in the base over a search that
        // answered. Same check `open_event` makes against the pool.
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
        // The wording the row now holds is still answerable.
        assert!(store.gap_event(&id, "fat32 mount").await.unwrap());
    }

    #[tokio::test]
    async fn no_cannot_erase_a_verdict_somebody_else_gave() {
        // The bar is drawn against an unjudged search, and another tab can
        // answer the same search while the one holding it is open. "No" clears the
        // verdict columns, so without a guard it is the one answer on the bar
        // that deletes a confirmed pair out of the eval set.
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
        // `weak_below = 0.0` turns the labelling off, and at that setting the
        // outer `COALESCE` in `dealable!` reads an empty pool as `0 >= 0` and
        // lets it through — a card with no options, which is unanswerable
        // except by skip, gap or discard, and which the sweep is raising as a
        // hole in the base at the same time.
        let store = Store::memory().await.unwrap();
        let mut empty = ev("nothing here", Door::Ui);
        empty.candidates.clear();
        store.record_search(empty, 0).await.unwrap();
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_opened_event_is_never_folded_into() {
        // Opening freezes the pool: it is the list the person actually read,
        // and the hit about to be recorded against it needs the rank it held.
        // A search after the open starts its own event.
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
        // "No" clears nothing into a verdict — it is a person saying the thing
        // in front of them was not it, which is not the same as saying the
        // base has nothing. So the search stays an open question, marked as
        // having been spoken for by a person.
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
        // A two-letter query and a search whose best match was under the
        // weak line are the questions nobody can answer: a typo, or a hole the
        // distance already says is one (`GapKind::Unmatched`). They stay
        // recorded and stay pending — the gap sweep reads them — but the
        // waiting count leaves them out.
        let store = Store::memory().await.unwrap();
        seed(&store, "ab", &["a1"]).await;
        let mut loose = ev("fat32", Door::Ui);
        loose.candidates[0].similarity = Some(0.2);
        store.record_search(loose, 0).await.unwrap();
        assert_eq!(store.pending_count(0.3).await.unwrap(), 0);

        let id = seed(&store, "ntfs", &["a1"]).await;
        assert_eq!(store.pending_count(0.3).await.unwrap(), 1);

        // Every screen that says "waiting" counts the same set — the number
        // used to be read off a plain `judged_at IS NULL` and said "12
        // waiting" over questions nobody could answer.
        assert_eq!(
            store.feedback_stats(0.3).await.unwrap().pending,
            store.pending_count(0.3).await.unwrap()
        );

        // Opening a result from it is not a verdict; it stays a question.
        store.open_event(&id, "a1").await.unwrap();
        assert_eq!(store.pending_count(0.3).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_search_the_vector_half_never_scored_is_still_counted() {
        // A hit found by the lexical half alone carries no similarity — the
        // embedder is down, or the query was answered on keywords. Reading
        // that absence as a zero withheld every such search from the count,
        // silently, when a keyword search that found something is exactly the
        // question a person can answer. Only a measured similarity under the
        // line, or a pool with nothing in it, is unanswerable.
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
        // Taking it back leaves the search an open question with nothing on it —
        // no verdict, no expectation, and no record of who once gave one.
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
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut second = after(&id, "fat32", Door::Ui);
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

        let mut filtered = after(&first, "fat32", Door::Ui);
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
        let id = store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        let mut filtered = after(&id, "fat32", Door::Ui);
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
            .record_search(after(&id, "fat32", Door::Ui), 15)
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
    async fn skipping_judges_nothing_and_stops_waiting() {
        // Not a verdict: an honest "I don't remember" must never cost the
        // search anything, or it stops being honest — so `judged` stays 0, the
        // event never enters the pairs and never becomes a discard.
        //
        // But it does stop being counted as waiting. The deck that re-dealt a
        // skipped card is gone; nothing asks this again, and a figure every
        // screen labels "waiting" must not include questions that have been put
        // to somebody and are never going to be put to anybody else.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "not sure", &["a"]).await;
        assert_eq!(store.pending_count(0.0).await.unwrap(), 1);
        store.skip_event(&id).await.unwrap();
        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.pending, s.judged), (0, 0), "{s:?}");
    }

    #[tokio::test]
    async fn undoing_a_verdict_on_a_skipped_search_puts_it_back_in_the_waiting_figure() {
        // Skipped once, judged later, then undone: `judged_at` went back to
        // NULL while `skips` stayed, and `dealable!` excludes a skipped event
        // — so the search was outstanding and invisible on Settings and
        // Insights at the same time, for ever.
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
        // The button says forget. It used to leave `context_events` — a device
        // fingerprint, an hour, a weekday and a battery reading per page view,
        // kept for 400 days — and `context_clusters`, which is those same
        // situations averaged. So the offer went on saying "Pattern · weekday,
        // hour, device · like 08.08., 15:04" out of data the person had just
        // asked to be rid of, which is the one outcome the button exists to
        // prevent.
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
                    ordinal: 0,
                    text: "opening hours".into(),
                    corpus_span: None,
                    title: Some("hours".into()),
                    category: None,
                    tags: Vec::new(),
                    segment_idx: None,
                    caveats: Vec::new(),
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
        // The race the other three writes are guarded for, from the other
        // side: before a second surface answers, the person who made that
        // search opens a result and presses Yes. A stale write used to turn
        // their hit into a gap — the pair gone from `pairs.json`, and
        // `expect_id` left naming an artifact on a `gap` row.
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
        // Retention expires events on a timer and Ops can purge them, both
        // under a judging screen that is already open. Reporting success would
        // put an MRR delta and an Undo button in front of the operator for a
        // row that does not exist.
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
        // No Qdrant and no embedding: the rank of every candidate was stored
        // when the search happened, so confirming one settles its rank too.
        let store = Store::memory().await.unwrap();
        let first = seed(&store, "top hit", &["a", "b", "c"]).await;
        store.judge_hit(&first, "a", Labeller::Deck).await.unwrap();
        let third = seed(&store, "third hit", &["x", "y", "z"]).await;
        store.judge_hit(&third, "z", Labeller::Deck).await.unwrap();

        let s = store.feedback_stats(0.0).await.unwrap();
        assert_eq!(s.judged, 2);
        assert_eq!(s.hits, 2);
        assert!((s.recall_at_10 - 1.0).abs() < 1e-9);
        // 1/1 and 1/3, averaged.
        assert!((s.mrr - (1.0 + 1.0 / 3.0) / 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn the_judged_pairs_are_the_answers_and_only_the_answers() {
        // What a sweep replays. A gap and a discard are verdicts, and they
        // count towards the judgement floor, but neither names an artifact:
        // replayed as a pair, one would be a query the ranking can only fail.
        let store = Store::memory().await.unwrap();
        let hit = seed(&store, "the image will not mount", &["a", "b"]).await;
        store.judge_hit(&hit, "a", Labeller::Deck).await.unwrap();
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

        let pairs = store.judged_pairs().await.unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].query, "the image will not mount");
        assert_eq!(pairs[0].expect, "a");

        // The day's counter reads verdicts, not pairs: judging is the work
        // being paced, and a gap is judging.
        assert_eq!(store.judged_since(0).await.unwrap(), 3);
        assert_eq!(
            store.judged_since(crate::store::now() + 60).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn an_answer_outside_the_pool_counts_as_a_find() {
        // The whole point of the "none of these" path: an artifact the ranker
        // never returned. It has no rank, so it contributes nothing to MRR and
        // it drags recall down — which is the truth about that search.
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
        // Neither can score: one has no answer, the other was not a question.
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
        seed(&store, "never looked at", &["a"]).await;
        store.judge_hit(&kept, "a", Labeller::Deck).await.unwrap();
        sqlx::query("UPDATE search_events SET created_at = ?")
            .bind(now() - 40 * 86_400)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
        let left: Vec<String> = sqlx::query_scalar("SELECT query FROM search_events")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["judged".to_string()]);
    }

    #[tokio::test]
    async fn a_discarded_event_expires_like_any_other() {
        // `discard` is the operator saying this was never a search. Holding
        // typos forever is keeping exactly what the window exists to shed.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "asdf", &["a"]).await;
        store
            .judge(&id, Verdict::Discard, Labeller::Deck)
            .await
            .unwrap();
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
        assert_eq!(store.pending_count(0.0).await.unwrap(), 0);
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
        store.judge_hit(&id, "a1", Labeller::Deck).await.unwrap();
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
