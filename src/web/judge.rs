//! Turning captured searches into labelled pairs.
//!
//! Most pairs are made at the moment of search now — a result read, a bar
//! answered, a gap pressed on the rail — and the deck deals only what none of
//! that labelled. See `web::ui::artifact_detail` and `Store::open_event`.
//!
//! The card shows the query as it was typed and the top five of the stored pool
//! in the order the search gave them, titled, with the rest behind a fold. It
//! used to shuffle the whole pool, so that the ranker's opinion could not be
//! seen while its work was judged; that made a card of twenty unordered
//! paragraphs, and the honest answer to it was "I don't know, I was looking".
//! Five, in order, is a question a person can answer. The position bias that
//! costs is small, and `docs/evaluation.md` says so beside the number. Scores
//! are still withheld.
//!
//! The pool offered is wider than the answer the searcher saw, so an artifact
//! the ranking buried can still be confirmed. That is the only way a ranking
//! failure leaves a record instead of passing as a shrug.

use crate::error::Result;
use crate::store::feedback::{Labeller, PendingEvent, Stats, Verdict};
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::tenant::CanJudge;
use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};

/// Judgements before the miss list is worth opening.
const MISS_LIST_AT: i64 = 10;

pub struct Choice {
    pub artifact_id: String,
    pub title: String,
    /// Whether `title` is the note's name rather than the artifact's own — see
    /// `SearchResult::titled_by_corpus`, which the search rail marks for the
    /// same reason. Without the marker several passages of one pasted note
    /// render as N options under one identical name, separable only by their
    /// snippets, on the one surface whose whole purpose is an unambiguous
    /// verdict about which of them was the one.
    pub titled_by_corpus: bool,
    pub snippet: String,
    /// Whether confirming this one would produce a pair the benchmark can hold.
    /// A deprecated or superseded artifact is offered but not choosable — see
    /// `card_for` for why it is shown at all.
    pub usable: bool,
    /// The digit that presses this option, or `None` where no key reaches it:
    /// behind the fold, or on something unusable. Over the choosable options
    /// among the five dealt, so the digits an operator can see are the digits
    /// that work and they run without a gap.
    pub key: Option<usize>,
}

/// How many of the pool the card deals openly. The rest are one click away.
pub const DEALT: usize = 5;

pub struct Card {
    pub id: String,
    pub query: String,
    pub door: String,
    pub when: String,
    /// Whether anything was opened from this search. See `PendingEvent::opened`.
    pub opened: bool,
    /// In the order the search gave them.
    pub choices: Vec<Choice>,
}

impl Card {
    /// Where the fold starts, for the template.
    pub fn dealt(&self) -> usize {
        DEALT
    }

    /// How many are behind it.
    pub fn folded(&self) -> usize {
        self.choices.len().saturating_sub(DEALT)
    }
}

/// The header's live half: what is true right now, and what just moved.
///
/// One struct rather than a dozen template fields because it is rendered
/// twice — once in the page, once out of band beside the next card — and two
/// copies of the same dozen fields would drift.
pub struct Pulse {
    pub judged: i64,
    /// The judgement count the last sweep ran at. Zero before the first one,
    /// which is when the page still explains what a sweep is.
    pub floor: i64,
    /// The judgement count the next sweep runs at.
    pub target: i64,
    /// What that target buys, in the words for a first sweep or a later one.
    pub label: &'static str,
    pub recall: String,
    pub mrr: String,
    /// `▲ +0.01`, or empty where nothing was judged. What the tick renders on.
    pub delta: String,
    /// Verdicts in the last 24 hours. Not "today": the zone that would define
    /// a midnight belongs to the client, never to the server — see
    /// `core::context::local_time` — and a window needs no zone at all.
    pub recent: i64,
    pub pending: i64,
    pub hits: i64,
    pub finds: i64,
    pub gaps: i64,
    pub discards: i64,
}

#[derive(Template)]
#[template(path = "judge.html")]
struct JudgeTemplate {
    /// The layout stamps this on `<html>`; every full page carries it.
    /// Waiting judgements for the nav. See `state::judge_pending`. Counted on
    /// this page too, so the badge falls as the queue is worked down rather
    /// than standing at whatever it read on arrival.
    judge_pending: Option<i64>,
    /// Always `Some` here. `Option` because the partial is shared with the card
    /// fragment, which draws nothing where there is no card to hang it under.
    pulse: Option<Pulse>,
    /// What the sweeps have to say. Always `Some` here; `Option` because the
    /// partial is shared with the card fragment and the apply answer.
    tune: Option<TuneView>,
    tune_oob: bool,
    misses: Vec<crate::store::feedback::Miss>,
    /// Questions asked and judged, beside the searches. Read from the same
    /// database and moved by every verdict on the ask page.
    asks: crate::store::asks::AskStats,
    card: Option<Card>,
    /// Always `None` here — the page is a fresh arrival, not the moment after a
    /// verdict. It exists because the card partial is shared with the fragment
    /// route, which does show one.
    flash: Option<Flash>,
}

#[derive(Template)]
#[template(path = "_judge_card.html")]
struct CardTemplate {
    card: Option<Card>,
    /// What the judgement just before this one revealed. `None` on a plain
    /// fetch of the next card.
    flash: Option<Flash>,
    /// The figures, drawn inside the card so they move with the work. Always
    /// `Some` from every route: they are part of what a verdict replaces now,
    /// not a second region kept in step with it. What tells a verdict from a
    /// plain fetch is `Pulse::delta`, which every animation on them keys on.
    pulse: Option<Pulse>,
    /// A sweep runs off the request path, so the page that paid for it was
    /// already sent. The next verdict is the first chance to report one.
    tune: Option<TuneView>,
    tune_oob: bool,
}

pub struct Flash {
    pub line: String,
    /// How loudly to say it, as a class suffix. See `tier`.
    pub tier: &'static str,
    /// `MRR 0.54 → 0.57`, so the figure the work is measured by visibly moves
    /// as the work is done.
    pub delta: String,
    /// The event this verdict was recorded against, so it can be taken back.
    /// `None` after a skip, which recorded nothing to undo.
    pub undo: Option<String>,
}

/// What the judgement just revealed, said plainly.
///
/// The emphasis runs opposite to intuition: the better the ranking did, the
/// quieter the line. A rank-one confirmation teaches almost nothing, and an
/// interface that cheers for it is training its operator to agree with
/// whatever was already on top.
pub fn diagnosis(rank: Option<i64>, verdict: Verdict) -> &'static str {
    match (verdict, rank) {
        (Verdict::Gap, _) => "a hole: your base doesn't know this yet.",
        (Verdict::Discard, _) => "discarded.",
        (Verdict::Hit, None) => "a find: search would never have shown you this.",
        (Verdict::Hit, Some(r)) if r >= 10 => {
            "the ranking got this wrong — this is what we're here for."
        }
        (Verdict::Hit, Some(r)) if r > 0 => "there, but far down. These are what move the MRR.",
        (Verdict::Hit, _) => "found as expected.",
    }
}

/// How much weight the flash line is given, as a class suffix.
///
/// The same split `diagnosis` makes, spent on emphasis rather than wording, and
/// running the same way: quietest where the ranking did best. A rank-one
/// confirmation is greyed towards invisible and a hit the ranking buried gets
/// the only accent on the page. It has to run this way round — an interface
/// that lit up for confirmations would be teaching its operator that agreeing
/// with the top result is the good outcome, and the top result is the thing
/// under examination.
pub fn tier(rank: Option<i64>, verdict: Verdict) -> &'static str {
    match (verdict, rank) {
        (Verdict::Gap, _) => "gap",
        (Verdict::Discard, _) => "quiet",
        // A find, and a hit below what the search actually showed: the two
        // cases the wider pool exists to make recordable at all.
        (Verdict::Hit, None) => "rare",
        (Verdict::Hit, Some(r)) if r >= 10 => "rare",
        (Verdict::Hit, Some(r)) if r > 0 => "plain",
        (Verdict::Hit, _) => "common",
    }
}

/// Roughly how long ago, in the words someone would use out loud. Precision
/// past "days" would suggest the timestamp matters; it is here to jog a memory.
pub(crate) fn ago(then: i64) -> String {
    let days = (crate::store::now() - then).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        n if n < 30 => format!("{n} days ago"),
        n => format!("{} months ago", n / 30),
    }
}

/// The card's preview: plain text, markup gone.
///
/// Flattening whitespace was the whole of this, so a card showed
/// `# Configure Linux…` and `custom\_passphrase` — the escapes an artifact
/// carries so that markdown renders it correctly, shown to a person as if they
/// were the text. `markdown::snippet` already strips them, and already stops
/// at a word rather than mid-one.
fn snippet_of(text: &str) -> String {
    crate::web::markdown::snippet(text, 140)
}

/// Hydrate a pending event into something renderable, dropping candidates whose
/// artifact has since been deleted and marking those the benchmark cannot hold.
///
/// One read per candidate rather than one query for all of them: the pool is at
/// most `feedback.candidates` long, this is not a hot path, and a hand-built
/// `IN (?, ?, …)` would be the more fragile of the two.
async fn card_for(tenant: &Tenant, event: PendingEvent) -> Result<Card> {
    let mut choices = Vec::with_capacity(event.candidates.len());
    // Each choice's corpus, by position, for the titling below.
    let mut corpora: Vec<Option<String>> = Vec::with_capacity(event.candidates.len());
    for c in &event.candidates {
        // A deleted artifact keeps its candidate row — the pool is history —
        // but it cannot be offered as something to choose. Only that: any other
        // failure is raised, because a pool quietly one short is one the
        // operator judges anyway, and the verdict is recorded as though the
        // missing candidate had been seen and rejected.
        match tenant.core.store.get_artifact(&c.artifact_id).await {
            Ok(a) => {
                corpora.push(a.corpus_id.clone());
                choices.push(Choice {
                    // Deprecated and superseded artifacts are shown greyed rather
                    // than dropped, for the reason just given: shortening the pool
                    // silently is what makes a verdict mean something it doesn't.
                    // `hit` refuses these anyway — `eval::export` would drop the
                    // pair — so showing them unchoosable says the same thing on the
                    // card, before the keystroke, instead of after it.
                    usable: a.in_results(),
                    artifact_id: a.id,
                    title: a.title.unwrap_or_default(),
                    titled_by_corpus: false,
                    snippet: snippet_of(&a.text),
                    key: None,
                })
            }
            Err(crate::error::Error::NotFound) => continue,
            Err(e) => return Err(e),
        }
    }
    // A candidate with no heading of its own is named by its note, the way a
    // ranked hit is (`Core::fill_titles`): a card of twenty paragraphs with
    // no names is a card nobody can read.
    let untitled: Vec<String> = choices
        .iter()
        .zip(&corpora)
        .filter(|(c, _)| c.title.is_empty())
        .filter_map(|(_, corpus)| corpus.clone())
        .collect();
    if !untitled.is_empty() {
        // Best-effort, exactly as in `Core::fill_titles`: a failed read costs
        // the borrowed names, never the card. Propagating it turned a
        // renderable card into a 500 over optional decoration.
        match tenant.core.store.corpus_titles(&untitled).await {
            Ok(titles) => {
                for (c, corpus) in choices
                    .iter_mut()
                    .zip(&corpora)
                    .filter(|(c, _)| c.title.is_empty())
                {
                    if let Some(t) = corpus.as_ref().and_then(|id| titles.get(id)) {
                        c.title = t.clone();
                        c.titled_by_corpus = true;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not read corpus titles for the judge card")
            }
        }
    }
    // Over the five dealt only: an option behind the fold answering to a key
    // would be a key that presses something the operator cannot see.
    for (key, c) in (1..=DEALT).zip(choices.iter_mut().take(DEALT).filter(|c| c.usable)) {
        c.key = Some(key);
    }
    Ok(Card {
        choices,
        id: event.id,
        query: event.query,
        door: event.door,
        when: ago(event.created_at),
        opened: event.opened,
    })
}

async fn next_pending_card(tenant: &Tenant) -> Result<Option<Card>> {
    match tenant
        .core
        .store
        .next_pending(tenant.core.weak_below)
        .await?
    {
        Some(event) => Ok(Some(card_for(tenant, event).await?)),
        None => Ok(None),
    }
}

/// What the header shows, and what — if anything — just moved.
///
/// The target is the next sweep rather than a fixed milestone: what a
/// judgement buys is a measurement, and after the first one the distance is to
/// the next re-sweep. Read from the last run rather than counted, so the two
/// always agree about when it is due.
async fn pulse_of(tenant: &Tenant, stats: &Stats, delta: String) -> Result<Pulse> {
    let tune = &tenant.core.feedback.tune;
    let (floor, label) = match tenant.core.store.latest_eval_run().await? {
        None => (0, "until the first sweep"),
        Some(last) => (last.judged_count, "until the next sweep"),
    };
    let target = match floor {
        0 => tune.min_judgements,
        n => n + tune.resweep_after,
    };
    // A target already passed would read as a count going backwards; it
    // happens whenever a sweep is queued but has not run yet.
    let target = target.max(stats.judged).max(1);
    Ok(Pulse {
        judged: stats.judged,
        floor,
        target,
        label,
        recall: format!("{:.2}", stats.recall_at_10),
        mrr: format!("{:.2}", stats.mrr),
        delta,
        recent: tenant
            .core
            .store
            .judged_since(crate::store::now() - 86_400)
            .await?,
        pending: stats.pending,
        hits: stats.hits,
        finds: stats.finds,
        gaps: stats.gaps,
        discards: stats.discards,
    })
}

async fn page(CanJudge(tenant): CanJudge) -> Result<Response> {
    use axum::response::IntoResponse;
    let stats = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    let misses = if stats.judged >= MISS_LIST_AT {
        tenant.core.store.misses(20).await?
    } else {
        vec![]
    };
    Ok(HtmlTemplate(JudgeTemplate {
        // Read off the stats already in hand rather than counted again.
        judge_pending: tenant.core.learn.enabled.then_some(stats.pending),
        pulse: Some(pulse_of(&tenant, &stats, String::new()).await?),
        tune: Some(tune_view(&tenant, "").await?),
        tune_oob: false,
        misses,
        asks: tenant.core.store.ask_stats().await?,
        card: next_pending_card(&tenant).await?,
        flash: None,
    })
    .into_response())
}

/// The next card, with nothing to say about the last one.
///
/// The figures ride inside the card, so they are always rendered — leaving them
/// out would blank the bar rather than leave it alone. What is left out is the
/// movement: `delta` is empty, and every animation on this fragment keys on it.
async fn next_card(CanJudge(tenant): CanJudge) -> Result<Response> {
    use axum::response::IntoResponse;
    let stats = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    Ok(HtmlTemplate(CardTemplate {
        card: next_pending_card(&tenant).await?,
        flash: None,
        pulse: Some(pulse_of(&tenant, &stats, String::new()).await?),
        tune: None,
        tune_oob: true,
    })
    .into_response())
}

/// Render the next card with a note about the verdict that was just given.
///
/// The MRR is read on both sides of the write, so the delta shown is the one
/// this judgement actually caused rather than a figure recomputed later.
async fn card_after(
    tenant: &Tenant,
    before: Stats,
    rank: Option<i64>,
    verdict: Verdict,
    judged: &str,
) -> Result<Response> {
    use axum::response::IntoResponse;
    let stats = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    let after = stats.mrr;
    // A verdict is what buys the next measurement. Off the request path: the
    // operator must not wait on a grid of searches, and a sweep that fails
    // must not fail the verdict that paid for it.
    crate::eval::sweep::maybe_spawn(&tenant.core);
    let moved = after - before.mrr;
    // Three places rather than two, and no floor under it. Half a point was the
    // threshold, on the reasoning that a smaller move leaves the figure on
    // screen unchanged. But one verdict can shift the MRR by at most 1/n, so
    // the tick fell silent in exact proportion to how much work had been done:
    // the one number this page is worked towards stopped acknowledging the work
    // precisely where the work started to add up. Shown at the precision the
    // movement actually has instead, and shown whenever there was one.
    let delta = if moved == 0.0 {
        String::new()
    } else if moved > 0.0 {
        format!("▲ +{moved:.2}")
    } else {
        format!("▼ −{:.2}", moved.abs())
    };
    Ok(HtmlTemplate(CardTemplate {
        card: next_pending_card(tenant).await?,
        flash: Some(Flash {
            line: diagnosis(rank, verdict).to_string(),
            tier: tier(rank, verdict),
            delta: format!("MRR {:.2} → {after:.2}", before.mrr),
            undo: Some(judged.to_string()),
        }),
        pulse: Some(pulse_of(tenant, &stats, delta).await?),
        tune: Some(tune_view(tenant, "").await?),
        tune_oob: true,
    })
    .into_response())
}

/// Put the same card back with a note, having recorded nothing.
///
/// Not `card_after` with an empty delta: nothing was judged, so there is no MRR
/// movement to report and nothing to undo. The event is still pending, so it is
/// fetched by id rather than taken from the queue — a capture landing in the
/// meantime would otherwise swap the card out from under the correction.
async fn card_again(tenant: &Tenant, event_id: &str, line: &str) -> Result<Response> {
    use axum::response::IntoResponse;
    let card = match tenant.core.store.pending_by_id(event_id).await? {
        Some(event) => Some(card_for(tenant, event).await?),
        None => next_pending_card(tenant).await?,
    };
    let stats = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    Ok(HtmlTemplate(CardTemplate {
        card,
        flash: Some(Flash {
            line: line.to_string(),
            // A correction, not a verdict: said plainly, with none of the
            // weight a judgement carries.
            tier: "quiet",
            delta: String::new(),
            undo: None,
        }),
        // Nothing was recorded, so the figures are rendered where they stand and
        // `delta` is empty, which is what every animation on them keys on.
        pulse: Some(pulse_of(tenant, &stats, String::new()).await?),
        tune: None,
        tune_oob: true,
    })
    .into_response())
}

/// One candidate's full text, for reading before confirming it.
///
/// The snippet on the card is 140 characters, which is enough to recognise an
/// artifact and not enough to be sure of one — and a verdict is a line in the
/// dataset the ranker is scored against. Deliberately says nothing about rank,
/// score or whether the search showed this at all: the card withholds that on
/// purpose, and a detail view that leaked it would undo the whole arrangement.
async fn read_artifact(
    CanJudge(tenant): CanJudge,
    Path(artifact_id): Path<String>,
) -> Result<Response> {
    use axum::response::IntoResponse;
    let a = tenant.core.store.get_artifact(&artifact_id).await?;
    Ok(HtmlTemplate(FullTemplate {
        html: crate::web::markdown::render(&a.text),
    })
    .into_response())
}

/// Take back the verdict just recorded and return to that card.
///
/// The keyboard shortcuts make judging fast enough to be done at all, and fast
/// enough to misfire; without this, a slipped digit is a wrong pair scored as
/// truth forever. The event comes back to the card it was on rather than to
/// whatever now heads the queue.
async fn undo(CanJudge(tenant): CanJudge, Path(event_id): Path<String>) -> Result<Response> {
    use axum::response::IntoResponse;
    // The event may have expired between the verdict and the second thoughts.
    // The store says so now rather than reporting a write it did not make; here
    // that is not an error, for the reason below.
    match tenant.core.store.unjudge(&event_id).await {
        Ok(()) | Err(crate::error::Error::NotFound) => {}
        Err(e) => return Err(e),
    }
    let card = match tenant.core.store.pending_by_id(&event_id).await? {
        Some(event) => Some(card_for(&tenant, event).await?),
        // Expired out from under the operator, or never existed. The next card
        // is a better answer than an error page.
        None => next_pending_card(&tenant).await?,
    };
    let stats = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    Ok(HtmlTemplate(CardTemplate {
        card,
        flash: None,
        // An undo moves the figures back, and the operator is looking straight
        // at them while it happens. No delta, though: the tick and the bar's
        // travel are how the page acknowledges work, and taking a judgement
        // back is not some of it.
        pulse: Some(pulse_of(&tenant, &stats, String::new()).await?),
        tune: None,
        tune_oob: true,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct HitForm {
    pub artifact_id: String,
}

async fn hit(
    CanJudge(tenant): CanJudge,
    Path(event_id): Path<String>,
    axum::extract::Form(f): axum::extract::Form<HitForm>,
) -> Result<Response> {
    // The id has to name something. Both paths that post here — the card and
    // the assign search — offer only artifacts that existed when they were
    // rendered, so a miss means the artifact was deleted since, or the form was
    // replayed. Either way the expectation would name nothing: `feedback_stats`
    // reads a missing rank as "search never showed you this", counts the event
    // as a find, and drags recall@10 down for a ranking failure that never
    // happened. Pool membership is deliberately not required — an artifact the
    // search never offered is exactly what the assign path is for.
    let artifact = tenant.core.store.get_artifact(&f.artifact_id).await?;

    // Being active is required, though. `eval::export` freezes only active,
    // un-superseded artifacts and drops any pair naming something else, so a
    // confirmation here would raise the recall and MRR on this very page while
    // contributing nothing to `pairs.json` — the two numbers the operator is
    // asked to trust, disagreeing about the same judgement. Refused rather than
    // recorded: the card comes back so the answer can be given again against
    // something the benchmark will still be able to hold.
    if !artifact.in_results() {
        return card_again(
            &tenant,
            &event_id,
            "that one is deprecated or superseded, so the benchmark can't hold it. \
             Pick what answers this now, or call it a gap.",
        )
        .await;
    }

    // Read before the write: afterwards the event is no longer pending, and the
    // rank is what decides which diagnosis the operator gets.
    let rank = tenant
        .core
        .store
        .rank_in_event(&event_id, &f.artifact_id)
        .await?;
    let before = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    tenant
        .core
        .store
        .judge_hit(&event_id, &f.artifact_id, Labeller::Deck)
        .await?;
    card_after(&tenant, before, rank, Verdict::Hit, &event_id).await
}

async fn gap(CanJudge(tenant): CanJudge, Path(event_id): Path<String>) -> Result<Response> {
    let before = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    tenant
        .core
        .store
        .judge(&event_id, Verdict::Gap, Labeller::Deck)
        .await?;
    card_after(&tenant, before, None, Verdict::Gap, &event_id).await
}

async fn discard(CanJudge(tenant): CanJudge, Path(event_id): Path<String>) -> Result<Response> {
    let before = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below)
        .await?;
    tenant
        .core
        .store
        .judge(&event_id, Verdict::Discard, Labeller::Deck)
        .await?;
    card_after(&tenant, before, None, Verdict::Discard, &event_id).await
}

async fn skip(CanJudge(tenant): CanJudge, Path(event_id): Path<String>) -> Result<Response> {
    tenant.core.store.skip_event(&event_id).await?;
    next_card(CanJudge(tenant)).await
}

// ── The "none of these" path ────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_judge_full.html")]
struct FullTemplate {
    /// Rendered and sanitized markdown — chunk text is model output shown
    /// inside an authenticated session, so it is untrusted by definition.
    html: String,
}

#[derive(Template)]
#[template(path = "_judge_assign.html")]
struct AssignTemplate {
    event_id: String,
    /// The query as it was captured — the thing being judged. Fixed for the
    /// life of the screen: it is the operator's reference for what they are
    /// looking for, so typing must not overwrite it.
    event_query: String,
    /// What is in the search box. Separate from `event_query` because the swap
    /// rebuilds the input, and a box that renders empty loses whatever was
    /// being typed.
    typed: String,
    results: Vec<Choice>,
    /// Whether a search has been run yet, so an empty list can say "nothing
    /// matched" instead of appearing before anything was asked.
    searched: bool,
}

async fn assign(CanJudge(tenant): CanJudge, Path(event_id): Path<String>) -> Result<Response> {
    use axum::response::IntoResponse;
    // By id, not by "whichever is next to judge": a capture landing between the
    // card being drawn and this click would otherwise win the ordering and
    // leave the screen with no query on it at all.
    let event_query = tenant
        .core
        .store
        .event_query(&event_id)
        .await?
        .unwrap_or_default();
    Ok(HtmlTemplate(AssignTemplate {
        event_id,
        event_query,
        typed: String::new(),
        results: vec![],
        searched: false,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct AssignQuery {
    #[serde(default)]
    pub q: String,
}

async fn assign_results(
    CanJudge(tenant): CanJudge,
    Path(event_id): Path<String>,
    axum::extract::Query(p): axum::extract::Query<AssignQuery>,
) -> Result<Response> {
    use axum::response::IntoResponse;
    let event_query = tenant
        .core
        .store
        .event_query(&event_id)
        .await?
        .unwrap_or_default();
    let mut results = vec![];
    if !p.q.trim().is_empty() {
        let query = crate::core::search::SearchQuery {
            q: p.q.clone(),
            limit: 10,
            tags: vec![],
            category: None,
            // Looking something up in order to label it is not the operator
            // reading their notes.
            mark: false,
            include_deprecated: false,
            include_superseded: false,
            // One deliberate lookup; the scope decides whether it reranks.
            rerank: true,
            explain: false,
        };
        // The one search in the application that must never be captured: it is
        // composed in full knowledge of the answer, which is the contamination
        // the whole feature exists to keep out of the dataset.
        let hits = tenant
            .core
            .search(&query, crate::store::feedback::Door::Judge)
            .await?;
        results = hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| Choice {
                artifact_id: h.artifact_id,
                title: h.title.unwrap_or_default(),
                // The search already borrowed the note's name where the hit had
                // none of its own (`Core::fill_titles`), and says so.
                titled_by_corpus: h.titled_by_corpus,
                snippet: snippet_of(&h.text),
                // The search that produced these excluded deprecated and
                // superseded artifacts, so everything offered here is something
                // the benchmark can hold.
                usable: true,
                // `limit` above is ten and the shortcut is one digit, so the
                // tenth result gets no badge rather than one that cannot be
                // pressed. Numbered here rather than in the template for the
                // same reason as the card: the digits are the ones that work.
                key: (i < 9).then_some(i + 1),
            })
            .collect();
    }
    Ok(HtmlTemplate(AssignTemplate {
        event_id,
        event_query,
        typed: p.q,
        results,
        searched: true,
    })
    .into_response())
}

// ── What the sweeps have to say ─────────────────────────────────────────────

/// A recommendation, ready to read and to take.
pub struct Rec {
    pub id: String,
    /// What would change and what it buys, in one line.
    pub line: String,
    /// The pairs that move under it. Mandatory, never folded away: an
    /// aggregate says something moved, and only this says what.
    pub diff: Vec<String>,
}

pub struct TuneView {
    pub rec: Option<Rec>,
    /// Why there is nothing to offer, when a sweep has run and found nothing.
    /// Empty before the first sweep, where the honest answer is silence.
    pub quiet: String,
    pub applied: Vec<String>,
    /// What the press just before this one did.
    pub flash: String,
}

#[derive(Template)]
#[template(path = "_judge_tune.html")]
struct TuneTemplate {
    tune: Option<TuneView>,
    tune_oob: bool,
}

fn cap_str(c: Option<usize>) -> String {
    c.map_or("none".to_string(), |n| n.to_string())
}

/// One line naming what changes and what it is worth.
///
/// Every figure is read off the run rather than recomputed: a number and the
/// settings that produced it travel together, which is the whole of what the
/// `eval_runs` row is for.
///
/// "Replayed over N pairs" leads the figures rather than trailing them. They
/// used to end the line, which put `MRR 0.50 → 0.60` immediately under the
/// header's own MRR with nothing between them — two numbers of one name, one
/// read from the ranks the searches actually gave and one from a replay of
/// those searches through a door that skips priming. Neither is wrong; they
/// are not the same quantity, and side by side they invited being read as one.
fn describe(run: &crate::store::eval_runs::EvalRun) -> String {
    format!(
        "recency {:.2} → {:.2}, cap {} → {} · replayed over {} pairs: \
         MRR {:.2} → {:.2}, recall@10 {:.2} → {:.2}",
        run.base_params.recency_weight,
        run.best_params.recency_weight,
        cap_str(run.base_params.per_source_cap),
        cap_str(run.best_params.per_source_cap),
        run.pairs_used,
        run.base_mrr,
        run.best_mrr,
        run.base_recall,
        run.best_recall,
    )
}

fn rank_str(r: Option<usize>) -> String {
    r.map_or("not in the first ten".to_string(), |i| {
        format!("position {}", i + 1)
    })
}

async fn tune_view(tenant: &Tenant, flash: &str) -> Result<TuneView> {
    let rec = tenant
        .core
        .store
        .open_recommendation()
        .await?
        .map(|run| Rec {
            line: describe(&run),
            diff: run
                .diff
                .iter()
                .map(|d| format!("{} — {} → {}", d.query, rank_str(d.base), rank_str(d.new)))
                .collect(),
            id: run.id,
        });
    // Only where a sweep has actually run and come back empty. Before the
    // first one there is nothing to explain, and a line explaining nothing is
    // one more thing on a page that has enough.
    let quiet = match (&rec, tenant.core.store.latest_eval_run().await?) {
        (None, Some(last)) if !last.recommended => format!(
            "last sweep {}: no improvement found over {} pairs.",
            ago(last.created_at),
            last.pairs_used
        ),
        _ => String::new(),
    };
    let applied = tenant
        .core
        .store
        .applied_eval_runs(10)
        .await?
        .iter()
        .map(|r| {
            format!(
                "{} — {}",
                ago(r.applied_at.unwrap_or(r.created_at)),
                describe(r)
            )
        })
        .collect();
    Ok(TuneView {
        rec,
        quiet,
        applied,
        flash: flash.to_string(),
    })
}

// ── Taking a recommendation live ────────────────────────────────────────────

/// The tuning block, redrawn, with a line about what just happened.
async fn tune_fragment(tenant: &Tenant, line: &str) -> Result<Response> {
    use axum::response::IntoResponse;
    Ok(HtmlTemplate(TuneTemplate {
        tune: Some(tune_view(tenant, line).await?),
        // Answering the button inside the block itself, which htmx swaps by
        // target rather than by id.
        tune_oob: false,
    })
    .into_response())
}

/// Apply the open recommendation: the file first, then the running parameters,
/// then the stamp.
///
/// The order is the guarantee. A hot swap the file does not carry would vanish
/// on the next restart, leaving the tuning history claiming a change that is no
/// longer in force — and the file is the one place an operator can read what
/// their server is doing.
async fn tune_apply(
    State(st): State<AppState>,
    CanJudge(tenant): CanJudge,
    Path(run_id): Path<String>,
) -> Result<Response> {
    let Some(run) = tenant.core.store.eval_run(&run_id).await? else {
        return Err(crate::error::Error::NotFound);
    };
    // A recommendation that was already taken, a run that never was one, or
    // one a later sweep has since spoken over: all three arrive from a page
    // left open, and none is a reason to write anything. Asked of the store
    // rather than of this row, so what the button may take is exactly what the
    // page may offer.
    let open = tenant.core.store.open_recommendation().await?;
    if open.as_ref().is_none_or(|o| o.id != run.id) {
        return tune_fragment(
            &tenant,
            "that sweep is not an open recommendation — nothing was changed.",
        )
        .await;
    }

    let params: crate::core::ranking::RankingParams = run.best_params.into();
    if let Err(e) = crate::config::write_ranking(&st.config_path, &params) {
        // Said here rather than raised: a read-only config file is an ordinary
        // thing to find out about, and the operator is looking at the button
        // they just pressed. Nothing was swapped and nothing was stamped, so
        // the recommendation stays open and can be applied once the file can
        // be written.
        tracing::warn!(error = %e, path = %st.config_path.display(), "config.toml not written");
        return tune_fragment(
            &tenant,
            "config.toml could not be written, so nothing was applied. \
             The recommendation is still here.",
        )
        .await;
    }
    *tenant.core.ranking.write().expect("ranking lock") = params;
    // The stamp is what closes the recommendation, so its answer is the one
    // thing here that must not be dropped. `false` is the second press of the
    // same button arriving while the first was still in flight: same run, same
    // parameters, so the file and the running settings say what this press
    // would have written anyway — but only one press gets to report a change.
    // An error is worse than either, and raising it would have answered a 500
    // to a request that did change the file and the parameters: the operator
    // would have read "nothing happened" about a server that is now running
    // settings its history does not mention.
    match tenant.core.store.mark_eval_run_applied(&run_id).await {
        // The environment is layered over the file, so where one of these keys
        // is set the write is real and the restart undoes it. Said now, beside
        // the button, rather than discovered months later as a history claiming
        // settings the server stopped running at its last boot.
        Ok(true) => {
            let line = match crate::config::ranking_keys_in_env().as_slice() {
                [] => "applied — the next search runs with these settings.".to_string(),
                keys => format!(
                    "applied — the next search runs with these settings, but {} is set in the \
                     environment and will overrule the file at the next restart.",
                    keys.join(" and ")
                ),
            };
            tune_fragment(&tenant, &line).await
        }
        Ok(false) => {
            tune_fragment(
                &tenant,
                "that sweep had already been applied — nothing changed.",
            )
            .await
        }
        Err(e) => {
            tracing::error!(error = %e, run = %run_id, "applied run not stamped");
            tune_fragment(
                &tenant,
                "these settings are live and written to config.toml, but the run could not be \
                 recorded as applied — it may be offered again.",
            )
            .await
        }
    }
}

pub fn judge_router() -> Router<AppState> {
    Router::new()
        .route("/ui/judge", get(page))
        .route("/ui/judge/tune/{run_id}/apply", post(tune_apply))
        .route("/ui/judge/next", get(next_card))
        .route("/ui/judge/{id}/hit", post(hit))
        .route("/ui/judge/{id}/gap", post(gap))
        .route("/ui/judge/{id}/discard", post(discard))
        .route("/ui/judge/{id}/skip", post(skip))
        .route("/ui/judge/{id}/assign", get(assign))
        .route("/ui/judge/{id}/assign/results", get(assign_results))
        .route("/ui/judge/{id}/undo", post(undo))
        .route("/ui/judge/read/{artifact_id}", get(read_artifact))
}

#[cfg(test)]
mod tests {
    use crate::store::artifacts::ArtifactStatus;
    use crate::store::feedback::{Door, NewCandidate, NewEvent};
    use crate::web::test_support::{app_with_cookie, body_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The nav named a role. The page asks a real cognitive task and never
    /// said why it was worth doing — and the number on Insights is exactly
    /// what it is worth: recall@10 and MRR are read off these verdicts.
    #[tokio::test]
    async fn the_nav_names_the_task_and_the_page_says_why_it_matters() {
        // The entry exists only where searches are actually being recorded —
        // an installation that records none has nothing to review and is
        // offered no destination for it.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/judge")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("Review searches"),
            "the nav names the task, not a role: {html}"
        );
        assert!(
            html.contains("your own searches, coming back unlabelled"),
            "and the page says what it is asking for: {html}"
        );
    }

    /// The gate, from the outside: a signed-in user without the grant.
    ///
    /// Eleven routes, because the grant covers the router and not a page —
    /// `/ui/judge/tune/{run_id}/apply` is the only door in the tree that
    /// writes `config.toml`, and it is inside this one.
    async fn ungranted_app() -> (axum::Router, String) {
        crate::web::test_support::app_with_cookie_ungranted(
            crate::core::test_support::test_core().await,
        )
        .await
    }

    async fn status_of(app: &axum::Router, cookie: &str, method: &str, path: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn an_ungranted_user_is_refused_at_every_judge_route() {
        let (app, cookie) = ungranted_app().await;
        for path in ["/ui/judge", "/ui/judge/next", "/ui/judge/read/a1"] {
            assert_eq!(
                status_of(&app, &cookie, "GET", path).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        for path in ["/ui/judge/j1/assign", "/ui/judge/j1/assign/results"] {
            assert_eq!(
                status_of(&app, &cookie, "GET", path).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        for path in [
            "/ui/judge/tune/r1/apply",
            "/ui/judge/j1/hit",
            "/ui/judge/j1/gap",
            "/ui/judge/j1/discard",
            "/ui/judge/j1/skip",
            "/ui/judge/j1/undo",
        ] {
            assert_eq!(
                status_of(&app, &cookie, "POST", path).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
    }

    /// The grant is a column, not a copy of one.
    ///
    /// `engram --revoke-judge` is a second process writing the control
    /// database. The registry is holding a `User` it read when the tenant was
    /// opened, and a tenant on an instance under its cap is never evicted — so
    /// a gate reading that snapshot would keep letting a revoked user through
    /// for the life of the process, `/ui/judge/tune/{run}/apply` included.
    #[tokio::test]
    async fn revoking_the_grant_shuts_the_door_on_the_next_request() {
        let core = crate::core::test_support::test_core().await;
        let control = core.store.control.clone();
        let (app, cookie) = app_with_cookie(core).await;
        assert_eq!(
            status_of(&app, &cookie, "GET", "/ui/judge").await,
            StatusCode::OK
        );

        control
            .set_can_judge(crate::store::TEST_SUBJECT, false)
            .await
            .unwrap();

        assert_eq!(
            status_of(&app, &cookie, "GET", "/ui/judge").await,
            StatusCode::FORBIDDEN,
            "the revoke waited for an eviction that never comes"
        );
        assert_eq!(
            status_of(&app, &cookie, "POST", "/ui/judge/tune/r1/apply").await,
            StatusCode::FORBIDDEN,
            "a revoked user could still write config.toml"
        );
    }

    /// And the other way round, for the same reason.
    #[tokio::test]
    async fn granting_the_judge_opens_the_door_without_a_restart() {
        let mut core = crate::core::test_support::test_core().await;
        // The nav entry is gated on a capture log existing at all, so the half
        // of this that is about the nav needs one.
        core.learn.enabled = true;
        let control = core.store.control.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie_ungranted(core).await;
        assert_eq!(
            status_of(&app, &cookie, "GET", "/ui/judge").await,
            StatusCode::FORBIDDEN
        );

        control
            .set_can_judge(crate::store::TEST_SUBJECT, true)
            .await
            .unwrap();

        assert_eq!(
            status_of(&app, &cookie, "GET", "/ui/judge").await,
            StatusCode::OK
        );

        // And the nav says so. The gate read the live column while the nav read
        // `Tenant.user.can_judge` — the row as it was at open time — so
        // `engram --grant-judge`, which prints that the user may now judge, put
        // no link anywhere until their core fell out of the LRU. On an instance
        // under its cap that is never, and a route nothing leads to is a route
        // only the operator knows about.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/search")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            body_of(res).await.contains("/ui/judge"),
            "the grant opened the door and left no sign of it"
        );
    }

    #[tokio::test]
    async fn an_ungranted_user_gets_no_judge_entry_in_the_nav() {
        let (app, cookie) = ungranted_app().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(
            !body.contains("/ui/judge"),
            "an ungranted user was shown the door"
        );
    }

    #[tokio::test]
    async fn the_config_writing_route_is_behind_the_same_gate() {
        let (app, cookie) = ungranted_app().await;
        assert_eq!(
            status_of(&app, &cookie, "POST", "/ui/judge/tune/r1/apply").await,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn a_judge_card_names_nothing_untitled_and_leaks_no_markdown() {
        // Two thirds of the deployment's judge list read "Untitled", and the
        // snippets under them carried the backslash escapes and the leading
        // "#" that an artifact carries so a renderer reads it correctly —
        // shown to a person as if they were the text.
        assert_eq!(
            super::snippet_of("## Configure **auditd**"),
            "Configure auditd"
        );
        let s = super::snippet_of(r"2 - A custom passphrase (custom\_passphrase)");
        assert!(!s.contains('\\'), "escape shown as text: {s:?}");
    }

    /// A session, `real` genuine artifacts, and one captured search whose pool
    /// is those artifacts followed by `phantom` ids that name nothing.
    async fn judge_app(
        real: usize,
        phantom: &[&str],
    ) -> (axum::Router, String, crate::core::Core, Vec<String>) {
        // Learning off and the shipped floor: the ordinary judging tests are
        // never within reach of a sweep and never wait on one.
        judge_app_tuned(real, phantom, None).await
    }

    /// `judge_app`, with tuning live and the judgement floor low enough that a
    /// test can cross it.
    async fn judge_app_tuned(
        real: usize,
        phantom: &[&str],
        floor: Option<i64>,
    ) -> (axum::Router, String, crate::core::Core, Vec<String>) {
        let mut core = crate::core::test_support::test_core().await;
        if let Some(n) = floor {
            core.feedback.tune.min_judgements = n;
            core.learn.enabled = true;
        }
        let core = core;
        let src = core
            .store
            .insert_corpus("raw for judging", "web", None)
            .await
            .unwrap();
        let new: Vec<crate::store::artifacts::NewArtifact> = (0..real)
            .map(|i| crate::store::artifacts::NewArtifact {
                ordinal: i as i64,
                text: format!("artifact number {i}, about mounting an image"),
                corpus_span: None,
                title: Some(format!("artifact {i}")),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        let ids: Vec<String> = made.iter().map(|c| c.id.clone()).collect();

        let mut pool: Vec<String> = ids.clone();
        pool.extend(phantom.iter().map(|s| s.to_string()));
        if !pool.is_empty() {
            core.store
                .record_search(
                    NewEvent {
                        query: "the image will not mount".into(),
                        door: Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        candidates: pool
                            .iter()
                            .enumerate()
                            .map(|(i, id)| NewCandidate {
                                artifact_id: id.clone(),
                                score: 1.0 - i as f32 / 100.0,
                                similarity: Some(0.5),
                                shown: i < 10,
                            })
                            .collect(),
                        answered: false,
                    },
                    0,
                )
                .await
                .unwrap();
        }

        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        (app, cookie, handle, ids)
    }

    async fn get(app: &axum::Router, uri: &str, cookie: &str) -> String {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        body_of(res).await
    }

    async fn post(app: &axum::Router, uri: &str, cookie: &str, body: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .method("POST")
                    .header("cookie", cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_candidate_with_no_heading_is_named_by_its_note_on_the_card() {
        // Twenty unordered paragraphs is the card nobody can read; a pasted
        // note's passages have no heading, and the note's title is what a
        // person calls them — the same titling a ranked hit gets.
        let (app, cookie, core, _) = judge_app(0, &[]).await;
        let src = core
            .store
            .insert_corpus("feeding schedule", "web", Some("Sourdough"))
            .await
            .unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "feeding schedule that finally worked".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        core.store
            .record_search(
                NewEvent {
                    query: "sourdough".into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![NewCandidate {
                        artifact_id: made[0].id.clone(),
                        score: 1.0,
                        similarity: Some(0.5),
                        shown: true,
                    }],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains("Sourdough"), "{body}");
        // And the card says whose name it is. Unmarked, several passages of one
        // pasted note are N options under one identical title, separable only
        // by their snippets — on the surface whose whole purpose is recording
        // which of them was the one.
        assert!(
            body.contains("judge-title-corpus"),
            "a borrowed name went unmarked: {body}"
        );
        assert!(body.contains("The note this passage is from"), "{body}");
        let css = include_str!("../../assets/app.css");
        assert!(
            css.contains(".judge-title-corpus"),
            "the marker has no rule"
        );
    }

    #[tokio::test]
    async fn the_page_counts_the_questions_judged_beside_the_searches() {
        let (app, cookie, core, _) = judge_app(2, &[]).await;
        let ask = |q: &str| crate::store::asks::NewAsk {
            question: q.into(),
            scope: None,
            filters: "{}".into(),
            query_vec: vec![0.0; 4],
            embed_model: "fake".into(),
            answer: "a".into(),
            abstained: false,
            dropped: 0,
            truncated: false,
            citations: vec![],
        };
        let a = core.store.record_ask(ask("one")).await.unwrap();
        core.store.record_ask(ask("two")).await.unwrap();
        core.store
            .judge_ask(&a, crate::store::asks::AskVerdict::Right)
            .await
            .unwrap();
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains("1 of 2 questions judged"), "{body}");
        assert!(body.contains("1 right"), "{body}");
        // And the count the card is built around is sized. It was renamed out
        // from under its rule when the XP bar came out, and rendered at body
        // size with no margin while every assertion about it still passed on
        // the text alone.
        assert!(body.contains(r#"class="judge-count"#), "{body}");
        let css = include_str!("../../assets/app.css");
        assert!(css.contains(".judge-count"), "the count has no rule");
    }

    #[tokio::test]
    async fn the_card_offers_the_whole_pool_not_only_what_was_shown() {
        // Offering only the ten that were displayed would make a buried hit
        // unconfirmable, and the ranking failure invisible.
        let (app, cookie, _core, ids) = judge_app(13, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        for id in &ids {
            assert!(body.contains(id.as_str()), "candidate {id} missing");
        }
    }

    #[tokio::test]
    async fn the_card_deals_the_top_five_in_order_and_folds_the_rest() {
        // Twenty unordered paragraphs forced a linear read of all twenty. The
        // top five in the order the search gave them is a question a person
        // can answer; the rest of the pool is still there — a buried hit can
        // still be confirmed — behind one click, and carries no key.
        let (app, cookie, _core, ids) = judge_app(13, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        let fold = body.find("judge-more").expect("no fold for the rest");
        let mut last = 0;
        for id in &ids[..5] {
            let at = body.find(id.as_str()).unwrap();
            assert!(at < fold, "{id} is not among the five dealt: {body}");
            assert!(at > last, "the five are not in the search's order: {body}");
            last = at;
        }
        for id in &ids[5..] {
            assert!(body.find(id.as_str()).unwrap() > fold, "{id} dealt openly");
        }
        assert!(body.contains("8 more"), "{body}");
        assert!(body.contains(r#"<span class="judge-key">5</span>"#));
        assert!(
            !body.contains(r#"<span class="judge-key">6</span>"#),
            "a key points at an option behind the fold"
        );
    }

    #[tokio::test]
    async fn the_card_says_whether_anything_was_opened() {
        // A search nobody opened anything from is a different question — was
        // there something you wanted at all? — from one where something was
        // read and not confirmed.
        let (app, cookie, core, _) = judge_app(2, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(body.contains("opened nothing"), "{body}");
        assert!(body.contains("No, I was just looking"), "{body}");
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let opened = event.candidates[0].artifact_id.clone();
        assert!(core.store.open_event(&event.id, &opened).await.unwrap());
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(!body.contains("opened nothing"), "{body}");
        assert!(body.contains("which of these was it"), "{body}");
    }

    #[tokio::test]
    async fn the_way_out_of_the_question_is_never_inside_the_scrolling_part() {
        // The pool is the only thing on the card with a scrollbar, and the three
        // answers are outside it. That is the whole of why they stay put — it
        // used to be `position: sticky` fighting a page twenty-three cards tall.
        let (app, cookie, _core, _) = judge_app(20, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        let pool_ends = body.find("</ol>").expect("no pool");
        let outs = body.find("judge-outs").expect("no way out of the question");
        assert!(
            outs > pool_ends,
            "the answers are inside the box that scrolls: {body}"
        );
        assert_eq!(
            body.matches("judge-pool").count(),
            1,
            "more than one thing on the card scrolls: {body}"
        );
    }

    #[tokio::test]
    async fn reading_a_candidate_costs_no_line_of_its_own() {
        // Twenty candidates meant twenty lines that said "Read it in full" and
        // nothing else — a whole row each, to repeat the same six words. The
        // handle is in the row now, and what it opens lands in the sibling
        // beneath so it can span the row without moving the handle out of its
        // column.
        let (app, cookie, _core, _) = judge_app(20, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(
            !body.contains(">Read it in full<"),
            "the handle is still a line of prose per candidate: {body}"
        );
        assert!(
            body.contains(r#"hx-target="next .judge-full""#),
            "the full text is not fetched into the row it belongs to: {body}"
        );
    }

    #[tokio::test]
    async fn a_verdict_moves_the_count_and_there_is_no_bar() {
        // The XP bar, the trail and the sheen made the page prettier and the
        // question no easier. The count is what a verdict buys: the distance
        // to the next sweep, said as a number.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        assert_eq!(
            post(
                &app,
                &format!("/ui/judge/{}/hit", event.id),
                &cookie,
                &format!("artifact_id={}", ids[0])
            )
            .await,
            StatusCode::OK
        );
        let card = get(&app, "/ui/judge/next", &cookie).await;
        assert!(!card.contains("progressbar"), "the bar is back: {card}");
        assert!(card.contains("<b>1</b> /"), "{card}");
        assert!(card.contains("until the first sweep"), "{card}");
    }

    #[tokio::test]
    async fn the_card_shows_no_ranks_and_no_scores() {
        // Both are the ranker's opinion, which is exactly what must not be
        // heard while judging.
        //
        // Read off the pool rather than the whole fragment. The figures ride in
        // the card now, and one of them is the mean reciprocal *rank* — an
        // aggregate over judgements already given, which says nothing about the
        // candidates on this card and is the number the work is aimed at. What
        // must stay clean is the list itself.
        let (app, cookie, _core, _) = judge_app(3, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        let pool = body
            .split_once(r#"<div class="judge-pool">"#)
            .and_then(|(_, rest)| rest.split_once("</ol>"))
            .map(|(pool, _)| pool)
            .unwrap_or_else(|| panic!("no pool on the card: {body}"));
        assert!(
            !pool.contains("rank"),
            "a rank leaked into the pool: {pool}"
        );
        assert!(
            !pool.contains("score"),
            "a score leaked into the pool: {pool}"
        );
    }

    #[tokio::test]
    async fn confirming_a_candidate_records_the_hit_and_moves_on() {
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let status = post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[1]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let s = core.store.feedback_stats(0.0).await.unwrap();
        assert_eq!(s.hits, 1);
        assert!(core.store.next_pending(0.0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_deprecated_candidate_cannot_be_confirmed() {
        // `eval::export` freezes only active artifacts and drops any pair
        // naming something else. Recording this would raise the recall and MRR
        // on this very page while `pairs.json` gained nothing — the two numbers
        // the operator is asked to trust, disagreeing about one judgement.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();

        let status = post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[1]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the operator gets the card back");

        assert_eq!(core.store.feedback_stats(0.0).await.unwrap().hits, 0);
        assert_eq!(
            core.store.next_pending(0.0).await.unwrap().map(|e| e.id),
            Some(event.id),
            "the event must still be waiting for a verdict it can keep"
        );
    }

    #[tokio::test]
    async fn a_deprecated_candidate_is_shown_unchoosable_rather_than_offered() {
        // The refusal above is correct but arrives too late to be read: the
        // shuffle is seeded by event id, so the rejected option came back at
        // the same place with the same digit and nothing marking it, and an
        // operator judging by keystroke got the same flash every time. The
        // pool still shows at full length — a card quietly one option short is
        // one where "none of these" means something it doesn't — but the
        // option carries no key and cannot be posted.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(
            body.contains(ids[1].as_str()) || body.contains("judge-option-unusable"),
            "the deprecated candidate must still appear in the pool"
        );
        assert!(
            body.contains("judge-option-unusable") && body.contains("disabled"),
            "it must be marked and disabled rather than silently refused later"
        );
        assert_eq!(
            body.matches(r#"<span class="judge-key">1</span>"#).count(),
            1,
            "the one choosable option keeps the first digit"
        );
        assert_eq!(
            body.matches(r#"<span class="judge-key">2</span>"#).count(),
            0,
            "no digit may point at an option that would be refused"
        );
    }

    #[tokio::test]
    async fn a_candidate_can_be_read_in_full_before_it_is_confirmed() {
        // The snippet stops at 140 characters, and the click after it writes a
        // line into the dataset the ranker is scored against.
        let (app, cookie, core, ids) = judge_app(1, &[]).await;
        let card = get(&app, "/ui/judge/next", &cookie).await;
        assert!(
            card.contains(&format!("/ui/judge/read/{}", ids[0])),
            "the card offers no way to read a candidate: {card}"
        );

        let full = get(&app, &format!("/ui/judge/read/{}", ids[0]), &cookie).await;
        let stored = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(
            full.contains(&stored.text),
            "the reading view is not the artifact: {full}"
        );
        // Reading must stay a read: the event is still waiting for a verdict.
        assert!(core.store.next_pending(0.0).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_reading_view_says_nothing_about_rank_or_score() {
        let (app, cookie, _core, ids) = judge_app(1, &[]).await;
        let full = get(&app, &format!("/ui/judge/read/{}", ids[0]), &cookie).await;
        assert!(
            !full.contains("rank"),
            "a rank leaked into the reading view"
        );
        assert!(
            !full.contains("score"),
            "a score leaked into the reading view"
        );
    }

    #[tokio::test]
    async fn a_verdict_can_be_taken_back() {
        // Judging is driven by digit keys because it has to cost seconds, and
        // that is exactly what makes it misfire. A pair labelled by a slipped
        // key is scored as truth.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let flash = {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/ui/judge/{}/hit", event.id))
                        .method("POST")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(format!("artifact_id={}", ids[0])))
                        .unwrap(),
                )
                .await
                .unwrap();
            body_of(res).await
        };
        assert!(
            flash.contains(&format!("/ui/judge/{}/undo", event.id)),
            "the verdict was recorded with no way back: {flash}"
        );
        assert_eq!(core.store.feedback_stats(0.0).await.unwrap().hits, 1);

        let back = {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/ui/judge/{}/undo", event.id))
                        .method("POST")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            body_of(res).await
        };

        let s = core.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.hits, s.judged), (0, 0), "the verdict outlived its undo");
        let pending = core.store.next_pending(0.0).await.unwrap().unwrap();
        assert_eq!(pending.id, event.id, "a different event came back");
        assert!(
            back.contains(&event.id),
            "undo did not return to the card it undid: {back}"
        );
        // The answer goes with the verdict: a stale `expect_id` would keep
        // counting towards recall for a judgement nobody stands behind.
        assert_eq!(
            core.store.rank_in_event(&event.id, &ids[0]).await.unwrap(),
            Some(0),
            "the pool is history and must survive the undo"
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT expect_id FROM search_events WHERE id = ?"
            )
            .bind(&event.id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn skipping_leaves_it_pending() {
        let (app, cookie, core, _) = judge_app(1, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        post(&app, &format!("/ui/judge/{}/skip", event.id), &cookie, "").await;

        assert!(core.store.next_pending(0.0).await.unwrap().is_some());
        assert_eq!(core.store.feedback_stats(0.0).await.unwrap().judged, 0);
    }

    #[tokio::test]
    async fn a_vanished_artifact_is_left_out_of_the_card() {
        // The pool is history and keeps its rows; the card is a list of things
        // that can still be chosen.
        let (app, cookie, _core, _) = judge_app(1, &["gone-for-good"]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(!body.contains("gone-for-good"));
    }

    #[tokio::test]
    async fn confirming_an_artifact_that_no_longer_exists_records_nothing() {
        // A card drawn before the artifact was deleted, or a replayed POST.
        // The expectation would name nothing, and a missing rank reads as "the
        // search would never have shown you this" — a find, and a permanent
        // dent in recall@10 for a ranking failure that never happened.
        let (app, cookie, core, _) = judge_app(1, &["gone-for-good"]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let status = post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            "artifact_id=gone-for-good",
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let s = core.store.feedback_stats(0.0).await.unwrap();
        assert_eq!(s.finds, 0, "a phantom was counted as a find");
        assert_eq!(s.judged, 0);
        assert!(
            core.store.next_pending(0.0).await.unwrap().is_some(),
            "the event was consumed by a verdict that was refused"
        );
    }

    #[tokio::test]
    async fn a_verdict_on_a_purged_event_says_so_instead_of_claiming_success() {
        // Retention or an Ops purge under an open judging screen. The flash
        // would otherwise show an MRR delta and an Undo for a row that is gone.
        let (app, cookie, core, ids) = judge_app(1, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        core.store.purge_feedback().await.unwrap();

        for (uri, body) in [
            (
                format!("/ui/judge/{}/hit", event.id),
                format!("artifact_id={}", ids[0]),
            ),
            (format!("/ui/judge/{}/gap", event.id), String::new()),
            (format!("/ui/judge/{}/discard", event.id), String::new()),
            (format!("/ui/judge/{}/skip", event.id), String::new()),
        ] {
            assert_eq!(
                post(&app, &uri, &cookie, &body).await,
                StatusCode::NOT_FOUND,
                "{uri} claimed to record a verdict on an event that is gone"
            );
        }
    }

    #[test]
    fn the_diagnosis_is_loudest_where_the_ranking_did_worst() {
        // Inverted on purpose. A first-position hit is the least informative
        // card of the day; making it the most celebrated would breed agreement
        // with whatever the ranker already thought.
        use super::{diagnosis, tier};
        use crate::store::feedback::Verdict;
        assert_eq!(diagnosis(Some(0), Verdict::Hit), "found as expected.");
        assert!(diagnosis(Some(13), Verdict::Hit).contains("wrong"));
        assert!(diagnosis(None, Verdict::Hit).contains("find"));
        assert!(diagnosis(None, Verdict::Gap).contains("hole"));

        // The same split, said again in weight rather than in words: a
        // confirmation of what was already on top is greyed towards invisible,
        // and the two cases the wider pool exists for get the only accent on
        // the page. Held here rather than left to the stylesheet — which
        // verdict is the quiet one is a claim about what judging is for.
        assert_eq!(tier(Some(0), Verdict::Hit), "common");
        assert_eq!(tier(Some(13), Verdict::Hit), "rare");
        assert_eq!(tier(None, Verdict::Hit), "rare");
        assert_eq!(tier(None, Verdict::Gap), "gap");
        assert_eq!(tier(None, Verdict::Discard), "quiet");
    }

    #[tokio::test]
    async fn the_assignment_search_is_never_captured() {
        // It is composed in full knowledge of the answer. Recording it would
        // feed the dataset exactly the contamination this feature avoids.
        let (app, cookie, core, _) = judge_app(2, &[]).await;
        core.store.purge_feedback().await.unwrap();
        let event = core
            .store
            .record_search(
                NewEvent {
                    query: "the one being judged".into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        let before = core.store.feedback_stats(0.0).await.unwrap().captured;

        get(
            &app,
            &format!("/ui/judge/{event}/assign/results?q=mounting+an+image"),
            &cookie,
        )
        .await;
        core.background.wait_idle().await;

        assert_eq!(
            core.store.feedback_stats(0.0).await.unwrap().captured,
            before,
            "looking something up in order to label it must not become data"
        );
    }

    #[tokio::test]
    async fn the_assign_row_reads_like_the_card_row() {
        // The assign screen is where reading the candidate matters most — the
        // operator is choosing from results they have never seen ranked, on a
        // query written from memory. Its rows are the card's rows: a chevron
        // handle in the row, and the full text in the sibling beneath it, where
        // the stylesheet's `:has(> .judge-peek[open]) > .judge-full` rule can
        // reach it. Inside the disclosure it stayed `display: none` for ever,
        // and the handle was six words of prose wrapping down a two-rem column.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        // Embedded by hand, as `search::tests::reembed_all` does: the assign
        // screen searches, and a store with no vectors answers nothing.
        for id in &ids {
            crate::jobs::embed::run(&core, id).await.unwrap();
        }
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let body = get(
            &app,
            &format!("/ui/judge/{}/assign/results?q=mounting", event.id),
            &cookie,
        )
        .await;
        assert!(
            body.contains("judge-option"),
            "the search found nothing to read: {body}"
        );
        assert!(
            !body.contains(">Read it in full<"),
            "the handle is a line of prose per row: {body}"
        );
        assert!(
            body.contains(r#"hx-target="next .judge-full""#),
            "the full text is not fetched into the row it belongs to: {body}"
        );
    }

    #[tokio::test]
    async fn confirming_from_outside_the_pool_is_reported_as_a_find() {
        let (app, cookie, core, ids) = judge_app(1, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        // An artifact that exists but was never in this event's pool.
        let src = core
            .store
            .insert_corpus("another raw", "web", None)
            .await
            .unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "the artifact search never offered".into(),
                    corpus_span: None,
                    title: Some("unoffered".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        assert_ne!(made[0].id, ids[0]);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/judge/{}/hit", event.id))
                    .method("POST")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("artifact_id={}", made[0].id)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(body.contains("a find"), "the flash did not name it a find");
        assert_eq!(core.store.feedback_stats(0.0).await.unwrap().finds, 1);
    }

    #[tokio::test]
    async fn a_verdict_past_the_floor_pays_for_a_sweep() {
        // The loop the whole feature is: a verdict is what buys the next
        // measurement, so the check rides on the verdict rather than a timer.
        let (app, cookie, core, ids) = judge_app_tuned(2, &[], Some(1)).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[0]),
        )
        .await;
        core.background.wait_idle().await;

        let run = core.store.latest_eval_run().await.unwrap();
        assert!(run.is_some(), "the floor was crossed and no sweep ran");
        assert_eq!(run.unwrap().pairs_used, 1);
    }

    #[tokio::test]
    async fn under_the_floor_a_verdict_buys_nothing() {
        // Below it a sweep would recommend the quirks of a handful of queries
        // as confidently as a real improvement.
        let (app, cookie, core, ids) = judge_app_tuned(2, &[], Some(50)).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[0]),
        )
        .await;
        core.background.wait_idle().await;

        assert!(core.store.latest_eval_run().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_second_sweep_waits_for_new_judgements_rather_than_the_clock() {
        // What makes a re-sweep worth running is new evidence: the same pairs
        // under the same grid give the same answer, at the cost of a grid of
        // searches per verdict.
        let (app, cookie, core, ids) = judge_app_tuned(3, &[], Some(1)).await;
        for id in ids.iter().take(2) {
            let event = core.store.next_pending(0.0).await.unwrap();
            let Some(event) = event else { break };
            post(
                &app,
                &format!("/ui/judge/{}/hit", event.id),
                &cookie,
                &format!("artifact_id={id}"),
            )
            .await;
            core.background.wait_idle().await;
        }

        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM eval_runs")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(runs, 1, "one judgement is not ten");
    }

    /// An app whose store already holds one recommendation, plus the path to
    /// the configuration file that app would rewrite.
    async fn tune_app(
        recommended: bool,
    ) -> (
        axum::Router,
        String,
        crate::core::Core,
        String,
        std::path::PathBuf,
    ) {
        let core = crate::core::test_support::test_core().await;
        let base = crate::store::eval_runs::RunParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
        };
        let best = if recommended {
            crate::store::eval_runs::RunParams {
                recency_weight: 0.1,
                per_source_cap: None,
            }
        } else {
            base
        };
        let run = core
            .store
            .record_eval_run(&crate::store::eval_runs::NewEvalRun {
                judged_count: 50,
                pairs_used: 12,
                pairs_skipped: 0,
                base,
                base_recall: 0.70,
                base_mrr: 0.50,
                best,
                best_recall: 0.80,
                best_mrr: 0.60,
                diff: vec![crate::store::eval_runs::DiffRow {
                    query: "the image will not mount".into(),
                    base: Some(5),
                    new: Some(1),
                }],
                recommended,
            })
            .await
            .unwrap();
        let handle = core.clone();
        let (app, cookie, state) = crate::web::test_support::app_with_state(core).await;
        let path = state.config_path.as_ref().clone();
        (app, cookie, handle, run, path)
    }

    #[tokio::test]
    async fn applying_writes_the_file_swaps_the_parameters_and_stamps_the_run() {
        // All three or none: a swap the file does not carry vanishes on
        // restart, and a stamp without either is a history of things that did
        // not happen.
        let (app, cookie, core, run, path) = tune_app(true).await;
        let status = post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await;
        assert_eq!(status, StatusCode::OK);

        let live = *core.ranking.read().unwrap();
        assert_eq!(live.recency_weight, 0.1);
        assert_eq!(live.per_source_cap, None);

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("recency_weight = 0.1"), "{written}");
        assert!(written.contains("per_source_cap = 0"), "{written}");
        assert!(
            written.contains("# a comment the apply path must not eat"),
            "the operator's file came back as a machine's: {written}"
        );

        assert!(
            core.store
                .eval_run(&run)
                .await
                .unwrap()
                .unwrap()
                .applied_at
                .is_some()
        );
        assert!(core.store.open_recommendation().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_run_that_is_not_an_open_recommendation_changes_nothing() {
        // Both arrive from a page left open: one was never a recommendation,
        // the other has already been taken.
        for second_press in [false, true] {
            let (app, cookie, core, run, path) = tune_app(second_press).await;
            let before = std::fs::read_to_string(&path).unwrap();
            if second_press {
                assert_eq!(
                    post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await,
                    StatusCode::OK
                );
            }
            let live_before = *core.ranking.read().unwrap();

            let status = post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await;
            assert_eq!(
                status,
                StatusCode::OK,
                "a stale press is an answer, not a 500"
            );
            assert_eq!(*core.ranking.read().unwrap(), live_before);
            if !second_press {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            }
        }
    }

    #[tokio::test]
    async fn a_run_that_does_not_exist_is_a_404() {
        let (app, cookie, _core, _, _) = tune_app(true).await;
        assert_eq!(
            post(&app, "/ui/judge/tune/no-such-run/apply", &cookie, "").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn an_unwritable_config_leaves_the_running_parameters_alone() {
        // The whole apply or none of it. The recommendation stays open, so it
        // can be taken once the file can be written.
        let (app, cookie, core, run, path) = tune_app(true).await;
        std::fs::remove_file(&path).unwrap();
        let before = *core.ranking.read().unwrap();

        let status = post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await;
        assert_eq!(status, StatusCode::OK, "the operator is told, not 500'd");

        assert_eq!(*core.ranking.read().unwrap(), before, "swapped anyway");
        assert!(
            core.store
                .eval_run(&run)
                .await
                .unwrap()
                .unwrap()
                .applied_at
                .is_none(),
            "stamped a change that was never made"
        );
        assert!(core.store.open_recommendation().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_header_explains_the_numbers_it_shows() {
        // They were shown bare: an operator who had not read docs/evaluation.md
        // was asked to work towards two figures nobody had told them the
        // meaning of, and towards a target that named no reward.
        let (app, cookie, _core, _) = judge_app(2, &[]).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains("Mean reciprocal rank"), "MRR unexplained");
        assert!(body.contains("top ten"), "recall@10 unexplained");
        assert!(
            body.contains("tries other ranking settings"),
            "the progress bar must say what it is progress towards"
        );
        assert!(body.contains("until the first sweep"));
        assert!(body.contains("last 24h"), "the day's work is not counted");
    }

    #[tokio::test]
    async fn a_verdict_ships_the_header_beside_the_next_card() {
        // The header lives outside the swapped region, so without this it
        // stood at whatever it read on arrival while the queue was worked
        // down — the one figure the work is measured by, frozen.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/judge/{}/hit", event.id))
                    .method("POST")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("artifact_id={}", ids[0])))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(res).await;

        assert!(body.contains(r#"id="judge-live""#), "no header shipped");
        assert!(body.contains("hx-swap-oob"), "the header would not land");
        assert!(
            body.contains("judge-tick"),
            "the figure that moved must show that it moved"
        );
    }

    #[tokio::test]
    async fn a_card_fetched_without_a_verdict_animates_nothing() {
        // Nothing was judged, so nothing moved. An animation here would be the
        // page congratulating itself for a page load.
        let (app, cookie, _core, _) = judge_app(2, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(!body.contains("hx-swap-oob"), "{body}");
        assert!(!body.contains("judge-tick"), "{body}");
    }

    #[tokio::test]
    async fn an_open_recommendation_is_offered_with_the_pairs_that_moved() {
        let (app, cookie, _core, run, _) = tune_app(true).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains(&format!("/ui/judge/tune/{run}/apply")));
        assert!(body.contains("recency"), "the line must name what changes");
        assert!(body.contains("cap"), "both knobs are named");
        assert!(body.contains("MRR 0.50 → 0.60"), "{body}");
        assert!(
            body.contains("what changes"),
            "the diff is the part that decides it, not an extra"
        );
        assert!(
            body.contains("the image will not mount"),
            "the moved pair is named by its own query"
        );
    }

    #[tokio::test]
    async fn the_page_draws_one_header_and_one_recommendation() {
        // Both partials are shipped beside the card a verdict returns, and the
        // page draws both itself. Included unconditionally they rendered a
        // second time inside `#card`: the whole recommendation twice, and the
        // lower Apply button swapping the upper copy — so the one the operator
        // pressed stayed on screen, still offering what had just been taken.
        let (app, cookie, _core, _run, _) = tune_app(true).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert_eq!(body.matches(r#"id="judge-live""#).count(), 1, "{body}");
        assert_eq!(body.matches(r#"id="judge-tune""#).count(), 1, "{body}");
        assert_eq!(body.matches("/apply").count(), 1, "two Apply buttons");
    }

    #[tokio::test]
    async fn the_sweeps_figures_are_named_as_a_replay_rather_than_the_headers() {
        // The line carries an MRR and a recall@10, and so does the header a few
        // lines above it. Same names, different measurements: one is the ranks
        // the searches gave, the other is those searches run again under each
        // setting. Printed bare they read as one quantity moving.
        let (app, cookie, _core, _run, _) = tune_app(true).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains("replayed over 12 pairs"), "{body}");
        assert!(body.contains("from the replay"), "{body}");
    }

    #[tokio::test]
    async fn the_count_targets_the_next_sweep_rather_than_a_fixed_milestone() {
        // What a judgement buys is a measurement, and after the first sweep
        // the distance is to the next re-sweep, read from the last run rather
        // than counted so the two always agree about when it is due.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[0]),
        )
        .await;
        let swept_at = core.store.feedback_stats(0.0).await.unwrap().judged;
        record_run_at(&core, swept_at).await;

        let body = get(&app, "/ui/judge", &cookie).await;
        let target = swept_at + core.feedback.tune.resweep_after;
        assert!(
            body.contains(&format!(
                "<b>{swept_at}</b> / {target} until the next sweep"
            )),
            "the count does not run to the next sweep: {body}"
        );
        assert!(!body.contains("progressbar"), "the bar is back: {body}");
    }

    /// A sweep in the store, recorded as having run at `judged`.
    async fn record_run_at(core: &crate::core::Core, judged: i64) {
        let params = crate::store::eval_runs::RunParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
        };
        core.store
            .record_eval_run(&crate::store::eval_runs::NewEvalRun {
                judged_count: judged,
                pairs_used: 1,
                pairs_skipped: 0,
                base: params,
                base_recall: 0.70,
                base_mrr: 0.50,
                best: params,
                best_recall: 0.70,
                best_mrr: 0.50,
                diff: vec![],
                recommended: false,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_sweep_that_found_nothing_says_so_rather_than_going_quiet() {
        // Silence reads as "no sweep has ever run", which is a different fact
        // and the wrong one.
        let (app, cookie, _core, _, _) = tune_app(false).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains("no improvement found"), "{body}");
        assert!(!body.contains("/apply"), "nothing to apply was offered");
    }

    #[tokio::test]
    async fn before_any_sweep_the_block_says_nothing_at_all() {
        let (app, cookie, _core, _) = judge_app(2, &[]).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(!body.contains("no improvement found"));
        assert!(!body.contains("/apply"));
        assert!(!body.contains("tuning history"));
    }

    #[tokio::test]
    async fn an_applied_change_stands_in_the_history_with_its_numbers() {
        // The provenance rule, made structural: a number without the settings
        // that produced it cannot be compared against anything.
        let (app, cookie, _core, run, _) = tune_app(true).await;
        post(&app, &format!("/ui/judge/tune/{run}/apply"), &cookie, "").await;

        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(body.contains("tuning history"), "{body}");
        assert!(body.contains("MRR 0.50 → 0.60"), "{body}");
        assert!(body.contains("cap 3 → none"), "{body}");
    }

    #[tokio::test]
    async fn applying_answers_with_the_block_it_replaces() {
        // htmx swaps `#judge-tune` by id: a reply that is not that block would
        // leave the recommendation on screen after it was taken.
        let (app, cookie, _core, run, _) = tune_app(true).await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/judge/tune/{run}/apply"))
                    .method("POST")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(body.contains(r#"id="judge-tune""#), "{body}");
        assert!(body.contains("applied"), "{body}");
        assert!(!body.contains("/apply"), "it is still offering itself");
    }

    #[tokio::test]
    async fn a_sweep_that_finished_in_the_background_surfaces_on_the_next_verdict() {
        // It runs off the request path, so the page that paid for it has
        // already been sent. The next verdict is the first chance to say so.
        let (app, cookie, core, ids) = judge_app_tuned(2, &[], Some(1)).await;
        let event = core.store.next_pending(0.0).await.unwrap().unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/judge/{}/hit", event.id))
                    .method("POST")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("artifact_id={}", ids[0])))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(
            body.contains(r#"id="judge-tune""#),
            "the verdict carried no tuning block: {body}"
        );
    }

    #[tokio::test]
    async fn nothing_pending_says_so_rather_than_rendering_an_empty_card() {
        let (app, cookie, _core, _) = judge_app(0, &[]).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(
            body.to_lowercase().contains("nothing to review"),
            "an empty queue must say so"
        );
    }
}
