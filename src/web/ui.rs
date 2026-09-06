use crate::fmt::{ago, fmt_time};
use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::store::corpora::CorpusStatus;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

// ── View models ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct RenderedResult {
    /// What the rail entry links to: the detail pane for this chunk.
    pub artifact_id: String,
    /// Empty where the artifact has no title of its own. The rail then renders
    /// no heading at all — see `render_hit`.
    pub title: String,
    /// The title is the corpus's — see `SearchResult::titled_by_corpus`. The
    /// rail says so quietly rather than passing a passage off as the whole.
    pub titled_by_corpus: bool,
    /// Sanitized HTML from `markdown::render`. Rendered with `|safe`.
    pub html: String,
    /// Markup-free preview for the rail, where rendered HTML would not fit.
    pub snippet: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_id: String,
    /// Position in the list, as `#1`, `#2`, … Empty for a weak result.
    ///
    /// Not the raw score. That number is a fused rank from Qdrant plus a
    /// recency term, so it is comparable within one result list and meaningless
    /// between two — a hybrid query and a dense-only fallback do not even score
    /// on the same scale. Showing it invited a comparison it cannot support.
    ///
    /// Dropped entirely once a result is `weak`, because a rank is a claim
    /// about standing among answers, and something the query barely matches is
    /// not one. `#1` over a result the search itself calls loose is the exact
    /// false confidence this labelling exists to remove.
    pub rank: String,
    /// Only loosely related to the query — see `SearchResult::weak`.
    pub weak: bool,
    /// This hit moved up on activation. A small marker, because the claim is
    /// small: it passed a near-tie, it did not become a better match.
    pub primed: bool,
    /// This sitting has already been in it. Said beside `primed` rather than
    /// folded into it: "you were just reading this" and "this is reached
    /// often" are two different reasons to be higher up a list.
    pub in_sitting: bool,
    /// "in 2 h" when a reminder on this artifact is due inside the horizon.
    pub due_in: Option<String>,
    /// Past the point where this list's relevance falls off. Greyed, under a
    /// rule; the rank stays, because it did place — the claim withdrawn is
    /// "this is an answer", not "this is fifth". See `search::cliff`.
    pub past_cliff: bool,
    /// A reminder that is done. Badged, because a row that has quietly sunk
    /// with no reason given reads as a ranking bug.
    pub retired: bool,
    /// The title of the ranked hit that recalled this one. Set only on an
    /// associated hit, and it is what the row names.
    pub via_title: Option<String>,
    /// A model wrote this — merged, or generated from a pursuit. Badged, so
    /// it is never silently indistinguishable from captured text.
    pub model_written: bool,
    /// How many corpora it draws from, for the badge.
    pub origin_count: usize,
    /// The judge's line, where the link was judged.
    pub reason: Option<String>,
    /// Why this row ranked where it did, in one sentence — but only where the
    /// request asked for it. `None` on an ordinary search, so the quiet line
    /// stays quiet: see `why_ranked`.
    pub why_ranked: Option<String>,
    /// The document goes on past this passage, and what comes next did not
    /// place in this list. The answer to a question is often the paragraph
    /// after the one that matched, and the row says so rather than leaving the
    /// reader to go and find out.
    ///
    /// False where the continuation *did* place: it is already on the page,
    /// and `continues_in` names where.
    pub continues: bool,
    /// The rank of the next passage, where that passage is itself in this list
    /// — `#2`, as the row beside it is labelled. Empty otherwise, including
    /// when the next passage placed as something with no rank of its own.
    pub continues_in: String,
}

#[derive(Default)]
pub struct QueueRow {
    pub id: String,
    pub label: String,
    /// The capture's opening words, kept whether or not synthesis has named it.
    /// Never rendered on its own: it is what tells two rows apart when
    /// synthesis gave them the same name. Empty for a photo, or for a PDF whose
    /// extraction has not landed.
    pub opening: String,
    pub status: String,
    pub badge: &'static str,
    pub artifact_count: i64,
    pub created: String,
    /// `3/9` while windows are still being segmented, `None` once every window
    /// has resolved.
    pub progress: Option<String>,
    /// Percentage of the source that ended up inside some chunk, already
    /// formatted. `—` for a capture that has not been read yet.
    pub coverage: String,
    pub low_coverage: bool,
    /// Whether the loss can be placed in the source. False for a capture with
    /// no segment rows — one read before per-segment windows existed, whose
    /// coverage is still measured against the whole document but whose lines
    /// cannot be attributed to anything. The warning stays; the link to
    /// `#uncovered` does not, because that section renders nothing for it.
    pub locatable: bool,
    /// The open questions this capture answered, in the operator's words. What
    /// a capture did beyond being stored — said on the row that reported it
    /// arriving, because that is where somebody is looking. Empty for almost
    /// every capture, and silent when empty.
    pub covered: Vec<String>,
    /// Still on its way through the pipeline. Only these announce themselves;
    /// a finished capture is a title and a count.
    pub in_flight: bool,
    /// Read, and read successfully. False covers both halves of "not moving
    /// and not done" — failed, parked, partial — which are the states a count
    /// of artifacts describes least well, because it is usually zero and looks
    /// exactly like a finished capture that produced nothing.
    pub settled: bool,
    /// Waiting to be named. Shown differently from a capture that simply has
    /// no title, because this one is about to get one.
    pub unnamed: bool,
}

pub struct ArtifactView {
    pub id: String,
    pub title: String,
    /// Sanitized by `markdown::render`. One of the few `|safe` interpolations.
    pub html: String,
    pub text: String,
    pub tags: Vec<String>,
    pub embed_state: String,
    pub embed_badge: &'static str,
}


pub fn status_badge(status: &crate::store::corpora::CorpusStatus) -> &'static str {
    use crate::store::corpora::CorpusStatus::*;
    match status {
        Ready => "badge-success",
        Partial => "badge-warning",
        Failed => "badge-danger",
        // A parked capture is waiting on a person, not on a worker. It reads as
        // a warning because nothing will advance it on its own.
        NeedsReview => "badge-warning",
        Describing | Extracting | Raw | Segmenting | Segmented | Embedding => "badge-accent",
    }
}

pub fn embed_badge(state: &crate::store::artifacts::EmbedState) -> &'static str {
    use crate::store::artifacts::EmbedState::*;
    match state {
        Embedded => "badge-success",
        Failed => "badge-danger",
        Pending => "badge-muted",
    }
}

/// A sweep in words, for a page a person reads.
///
/// Housekeeping printed the queue's own identifiers — `arm_dedupe`,
/// `link_judge`, `segment_window` — in a column headed "Sweep". They are the
/// right names in the code and in a log, and they are the wrong ones on a page
/// somebody opens to see whether the base is well.
///
/// An identifier with no wording yet returns unchanged rather than blank: a
/// stage added later must show up as *something*. `every_stage_the_queue_can_run_has_a_word_for_it`
/// is what makes sure that fallback stays theoretical.
pub(crate) fn sweep_label(stage: &str) -> &str {
    match stage {
        "synthesize" => "Writing artifacts",
        // No stage constructs this any more. The arm stays because this
        // reads the string a row stored, and a queue written by an older
        // binary can still hold one; `Stage::parse` runs such a row as
        // `Synthesize`, which is where the variant always sent it.
        "enrich" => "Enriching",
        "segment_window" => "Segmenting",
        "title" => "Naming captures",
        "embed" => "Embedding",
        "consolidate" => "Consolidating",
        "dedupe" => "Judging duplicates",
        "relate" => "Finding near-identicals",
        "describe" => "Describing images",
        "extract" => "Reading documents",
        "associate" => "Associating",
        "link_judge" => "Judging links",
        "pursuit" => "Following up questions",
        "generate" => "Answering gaps",
        "retention" => "Retention",
        "arm_dedupe" => "Arming dedupe",
        "context" => "Learning situations",
        "moments" => "Reading time",
        "remind" => "Pushing what is due",
        "reap" => "Reaping the retired",
        "probe" => "Minting probes",
        "condense" => "Condensing",
        other => other,
    }
}

/// What to call an artifact that has no title of its own.
///
/// Not the ordinal. "Chunk 56" is a position in the ingest, not a name for
/// anything a reader went looking for — and it was the heading over every
/// verbatim passage in the pane. The opening of the body at least says what
/// the passage is about.
///
/// `title_of` itself, because the rule that strips markup off a *stored* title
/// belongs here too: without this the corpus page and the artifact pane showed
/// "**Was nicht abgedeckt ist:** * Es werden keine" with its asterisks while
/// Housekeeping showed it cleaned, which is the drift `title_of` was gathered
/// into one place to close.
pub(crate) fn artifact_title(c: &crate::store::artifacts::Chunk) -> String {
    title_of(c)
}

/// How an artifact's own text is rendered.
///
/// A passage is a slice of the document, kept as it was written; markdown is
/// the wrong reader for it. It eats the `#` of a section number, and it joins
/// lines whose breaks carry the structure — a table of contents lifted out of
/// a PDF collapses into one paragraph whose leader dots then stretch the width
/// of the card. Everything else here *was* written as markdown by a model, and
/// showing that as plain text would put the syntax on the page.
pub(crate) fn artifact_html(c: &crate::store::artifacts::Chunk) -> String {
    if c.provenance == crate::store::artifacts::Provenance::Passage {
        markdown::render_verbatim(&c.text)
    } else {
        markdown::render(&c.text)
    }
}

pub(crate) fn artifact_view(c: &crate::store::artifacts::Chunk) -> ArtifactView {
    ArtifactView {
        id: c.id.clone(),
        // A passage has no title by design and its first line is its body:
        // shown as both, the card said everything twice.
        title: if c.provenance == crate::store::artifacts::Provenance::Passage && c.title.is_none()
        {
            String::new()
        } else {
            artifact_title(c)
        },
        html: artifact_html(c),
        text: c.text.clone(),
        tags: c.tags.clone(),
        embed_state: c.embed_state.as_str().to_string(),
        embed_badge: embed_badge(&c.embed_state),
    }
}

// ── Templates ───────────────────────────────────────────────────────────────

/// One hole in the base, as the gap list carries it: enough to dismiss it,
/// and its text for a hole the sweep has not grouped yet, which is shown
/// under itself.
pub struct GapMember {
    /// The `GapKind`, for the forget route.
    pub kind: String,
    pub id: String,
    pub text: String,
}

pub struct GapGroup {
    pub label: String,
    pub members: Vec<GapMember>,
}

pub(crate) fn gap_member(g: crate::store::gaps::Gap) -> GapMember {
    GapMember {
        kind: g.kind.as_str().into(),
        id: g.id,
        text: g.text,
    }
}

#[derive(Template)]
#[template(path = "_intent_echo.html")]
pub(crate) struct IntentEchoTemplate {
    /// `will be synthesized`, `large paste`, or empty for an empty box.
    pub(crate) kind: &'static str,
    pub(crate) detail: String,
}

/// A template's markup as a string, for a fragment that is composed into
/// another rather than returned on its own. An echo that could not render is
/// no echo, never a 500 over a rail that is otherwise correct.
pub(crate) fn render_echo(t: &IntentEchoTemplate) -> String {
    t.render().unwrap_or_default()
}

/// What capture will do with the box, said before it is pressed.
///
/// Pure local arithmetic on the same counter and budget the size fork uses —
/// exact, no model call — riding the search response the box already makes on
/// every keystroke at a 120ms debounce.
///
/// `lang` because the budget is not one number: the window is what is left of
/// the synthesizer's context after its system prompt, and the ten prompts do
/// not cost the same. Told in English, the fork would promise one window for a
/// Russian paste that will actually be cut into two.
pub(crate) fn fate_echo(
    core: &crate::core::Core,
    q: &str,
    lang: crate::infer::lang::Lang,
) -> IntentEchoTemplate {
    if q.trim().is_empty() {
        return IntentEchoTemplate {
            kind: "",
            detail: String::new(),
        };
    }
    let budget = crate::jobs::synthesize::segment_budget(core, lang).max(1);
    let tokens = core.counter.count(q);
    if tokens <= budget {
        IntentEchoTemplate {
            kind: "will be synthesized",
            detail: "captured verbatim, then rewritten into structured artifacts".into(),
        }
    } else if q.len() <= EXACT_SPLIT_BYTES {
        // The splitter's own answer, not arithmetic beside it. A
        // `MarkdownSplitter` will not cut inside a paragraph unless it has to,
        // so ten paragraphs of six tenths of a budget each are ten windows and
        // `tokens.div_ceil(budget)` promised six — under-reporting by up to
        // half on ordinary prose, on the one line whose whole job is to say
        // what capture is about to do.
        let windows = crate::infer::split::split_into_segments(q, &core.counter, budget).len();
        IntentEchoTemplate {
            kind: "large paste",
            detail: format!("stored verbatim in {windows} windows; synthesis comes with use"),
        }
    } else {
        // Past the bound, the arithmetic — and the line says it is a floor
        // rather than pretending to a count it did not make.
        //
        // This branch is reached from `search_results`, which the box asks on
        // every keystroke behind a 120 ms debounce, and a full `MarkdownSplitter`
        // pass over a 40 KB article is not something the hottest route in the
        // app should do per keystroke. Under the bound the exact answer is
        // cheap and is what gets shown; over it, the difference between "9
        // windows" and "at least 6" is not what a person pasting a book is
        // reading the line for.
        let windows = tokens.div_ceil(budget);
        IntentEchoTemplate {
            kind: "large paste",
            detail: format!(
                "stored verbatim in at least {windows} windows; synthesis comes with use"
            ),
        }
    }
}

/// How much text the fate echo will run the splitter over.
///
/// See the `else` arm of [`fate_echo`]: above this the window count is
/// estimated instead, because the exact answer costs a whole `MarkdownSplitter`
/// pass on a route the capture box asks on every keystroke.
const EXACT_SPLIT_BYTES: usize = 20_000;

#[derive(Template)]
#[template(path = "_results.html")]
struct ResultsTemplate {
    results: Vec<RenderedResult>,
    /// Recalled by association with a ranked hit, never ranked against the
    /// query itself. Shown below the ranked list, under its own rule.
    associated: Vec<RenderedResult>,
    /// Every result is only loosely related, so the page says so once above the
    /// list instead of repeating it on each card.
    all_weak: bool,
    /// The query's indexable terms, for client-side highlighting.
    terms: String,
    /// This fragment is the refining pass — the reranker confirmed the order.
    /// The tick beside the count is what makes the reordering legible as a
    /// refinement rather than a glitch.
    reranked: bool,
    /// The search that filled this rail, where it was recorded. Every row
    /// carries it onward, so an open, a verdict and the "nothing here has it"
    /// button all name the one search they are answers about. `None` while
    /// searches are not being recorded, and the button and the links go
    /// without.
    event_id: Option<String>,
    /// The echo under the box, pre-rendered and shipped out of band with the
    /// rail. See `intent_echo`.
    echo: String,
    /// The query this rail was drawn for, carried by the gap button alone: a
    /// gap is a verdict about a wording, and a trailing keystroke can fold a
    /// later one into the same row before the button is pressed. See
    /// `Store::gap_event`.
    q: String,
}

/// Test-only, for the reason `NewArtifact`'s is: a field added to a template
/// must break every place that renders it until somebody decides what it says
/// there. Seven tests in this file were spelling all nine out.
#[cfg(test)]
impl Default for ResultsTemplate {
    fn default() -> Self {
        Self {
            results: vec![],
            associated: vec![],
            all_weak: false,
            terms: String::new(),
            reranked: false,
            event_id: None,
            echo: String::new(),
            q: String::new(),
        }
    }
}

impl ResultsTemplate {
    /// How many of the ranked results are loose. Said in the heading when
    /// the list is mixed; when every one is loose the flag above the list
    /// already says so and this stays out of the heading. Computed rather
    /// than carried, so it cannot disagree with the rows it counts.
    fn loose(&self) -> usize {
        self.results.iter().filter(|r| r.weak).count()
    }
}

/// The rail before anything is asked: the base introducing itself.
///
/// Rendered twice by design — inlined into the workspace page when it opens
/// with an empty box, and returned by the results endpoint when the box is
/// emptied — so the idle state is one account however it is reached, and
/// clearing a query goes back to it rather than to a "No matches." nobody
/// searched for.
#[derive(Template)]
#[template(path = "_idle_foot.html")]
pub(crate) struct IdleFootTemplate {
    pub(crate) artifacts: i64,
    pub(crate) corpora: i64,
    pub(crate) recent: Vec<IdleRecentRow>,
    /// Whether the base holds anything at all. With nothing held there are no
    /// counts to print and no last capture to name, so the line says what the
    /// program is for instead.
    pub(crate) held: bool,
    /// The echo slot, emptied — and only where this fragment is a *swap*.
    ///
    /// An empty box proves no intent, and an echo left standing over one would
    /// be describing text that is gone, so the box-clear response has to carry
    /// this. On first paint it is empty instead: the slot already exists in
    /// `_box_hint.html` under the box, an out-of-band attribute is inert on a
    /// page that was never swapped, and rendering it anyway gave the document
    /// two `id="intent-echo"` elements — of which htmx would only ever resolve
    /// the first.
    pub(crate) echo: String,
}

pub(crate) struct IdleRecentRow {
    pub(crate) id: String,
    pub(crate) label: String,
    /// "today", "3 days ago" — a jog, not a timestamp. See `ago`.
    pub(crate) when: String,
    /// The day, as a link target, in UTC — the no-JS fallback and nothing
    /// more. The day page builds its window in the *viewer's* zone, so east of
    /// Greenwich every capture between local midnight and the offset landed on
    /// the day before and the page said "Nothing on this day". `?tz=` could not
    /// save it: the date is already in the path by then. app.js rewrites the
    /// whole href from `at` below, in the zone only the browser knows.
    pub(crate) day: String,
    /// When the capture landed, in Unix seconds, for that rewrite.
    pub(crate) at: i64,
}

/// The name a capture goes by before synthesis titles it: the hint, or its
/// opening words — with a word for the two origins that have none to open
/// with. One rule, because the queue and the idle rail naming the same row
/// differently would read as two captures.
pub(crate) fn corpus_label(title_hint: Option<String>, raw_text: &str, origin: &str) -> String {
    title_hint.unwrap_or_else(|| {
        if raw_text.is_empty() && origin == crate::core::ingest::ORIGIN_IMAGE {
            "photo".into()
        } else if raw_text.is_empty() && origin == crate::core::ingest::ORIGIN_PDF {
            // A PDF has no opening words until the extraction lands. Without
            // this the row renders an empty anchor: nothing to read and
            // nothing to click through to the corpus.
            "document".into()
        } else {
            markdown::snippet(raw_text, 60)
        }
    })
}

/// Two counts and the last few captures, off the slimmest reads there are:
/// the idle rail is on the most-opened screen, re-renders on every box-clear,
/// and must cost nothing.
/// `oob` says which of the two renderings this is: the swap that returns the
/// page to idle carries the emptied echo, the inline first paint does not.
pub(crate) async fn idle_foot(tenant: &Tenant, oob: bool) -> Result<IdleFootTemplate> {
    let (corpora, artifacts) = tenant.core.store.held_brief().await?;
    let recent = tenant
        .core
        .store
        .recent_captures(5)
        .await?
        .into_iter()
        .map(
            |(id, title_hint, origin, created_at, opening)| IdleRecentRow {
                day: chrono::DateTime::from_timestamp(created_at, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default(),
                at: created_at,
                when: ago(created_at),
                label: corpus_label(title_hint, &opening, &origin),
                id,
            },
        )
        .collect();
    Ok(IdleFootTemplate {
        artifacts,
        corpora,
        recent,
        held: corpora > 0,
        echo: if oob {
            render_echo(&IntentEchoTemplate {
                kind: "",
                detail: String::new(),
            })
        } else {
            String::new()
        },
    })
}

#[derive(Template)]
#[template(path = "_queue.html")]
struct QueueTemplate {
    rows: Vec<QueueRow>,
    /// Whether anything is still moving. The fragment carries its own polling
    /// trigger only while this holds, so an idle page makes no requests.
    active: bool,
}

/// What a count in a sweep's `detail` is called on the page.
///
/// Keyed by stage as well as by field, because two sweeps both call a count
/// `armed` and they are not the same thing. A field with no entry here is not
/// rendered: the summary is a sentence about what happened, not a dump of every
/// number a sweep returned.
const SWEEP_WORDS: &[(&str, &str, &str)] = &[
    ("associate", "events", "searches replayed"),
    ("associate", "verdicts", "verdicts replayed"),
    ("associate", "forgotten", "links forgotten"),
    ("associate", "armed", "links sent to the judge"),
    ("consolidate", "superseded", "artifacts merged"),
    ("consolidate", "judged", "pairs sent to the judge"),
    ("arm_dedupe", "armed", "duplicates sent to the judge"),
    ("retention", "expired", "records expired"),
    ("retention", "named", "gaps named"),
    ("pursuit", "pursuits", "pursuits opened"),
];

/// Add up one run's `detail` into `totals`, keyed by the words it earns.
pub(crate) fn tally_sweep(stage: &str, detail: &str, totals: &mut Vec<(String, i64)>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(detail) else {
        return;
    };
    for (s, field, word) in SWEEP_WORDS {
        if *s != stage {
            continue;
        }
        let n = v
            .get(field)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if n == 0 {
            continue;
        }
        match totals.iter_mut().find(|(w, _)| w == word) {
            Some((_, t)) => *t += n,
            None => totals.push((word.to_string(), n)),
        }
    }
}

pub struct SourceRow {
    pub id: String,
    pub title: String,
    /// See `SupersededRow::subtitle`. A merge written from two sources that
    /// shared a title listed that title twice and said nothing else.
    pub subtitle: String,
    /// Empty when the source belongs to no corpus — a merge of merges resolves
    /// to captured roots, so in practice this is always set.
    pub corpus_id: String,
}

/// When an artifact was written and how it opens, for a table where the title
/// alone may not be unique.
pub(crate) fn row_subtitle(c: &crate::store::artifacts::Chunk) -> String {
    format!(
        "{} · {}",
        fmt_time(c.created_at),
        markdown::snippet(&c.text, 60)
    )
}

/// The source list a merge renders: its lineage roots, fetched and titled.
/// One shape for Ops and the detail pane — the two must stay behaviorally
/// identical (same self-guard, same tolerance for deleted sources, same
/// corpus fallback), and a copy in each is how they come to disagree about
/// what a merge was made of.
pub(crate) async fn source_rows(
    store: &crate::store::Store,
    merged_id: &str,
    roots: &[String],
) -> Vec<SourceRow> {
    let mut sources = Vec::new();
    for rid in roots {
        // A source deleted since leaves no row; skipping it is what the
        // `orphaned` flag exists to say out loud. `roots_of` answers an empty
        // list for a merge that lost every source; the self guard stays as
        // defense against a base written before that change.
        if rid == merged_id {
            continue;
        }
        if let Ok(r) = store.get_artifact(rid).await {
            sources.push(SourceRow {
                corpus_id: r.corpus_id.clone().unwrap_or_default(),
                title: title_of(&r),
                subtitle: row_subtitle(&r),
                id: r.id,
            });
        }
    }
    sources
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ForgetForm {
    /// `kind:id` pairs, comma-joined — one row of `_gaps.html` is a group,
    /// and forgetting is said of the group.
    members: String,
}

/// The operator's word that a hole is not worth an answer: every question in
/// the group is dismissed, and the row is gone.
///
/// A pair that does not parse is a 400 rather than a skipped member. The
/// template writes every pair, so a bad one is a bug, and a row that stays
/// half-forgotten would come back on reload under the same name.
///
/// A member that is *gone*, though, is not a bug and must not stop the loop.
/// The group's members are resolved when the page is rendered, and retention
/// expires the very rows they name — so a `search_events` row dropped between
/// the render and the press made `dismiss_gap` answer `NotFound`, aborting
/// part-way: the members before it were dismissed, the rest were not, htmx saw
/// a 404 and swapped nothing, and the row came back on reload under the same
/// label carrying the remainder. A question that no longer exists is already
/// forgotten, which is what was asked for.
async fn gap_forget(tenant: Tenant, Form(f): Form<ForgetForm>) -> Result<Response> {
    let mut members = Vec::new();
    for pair in f.members.split(',').filter(|p| !p.is_empty()) {
        let (kind, id) = pair
            .split_once(':')
            .ok_or_else(|| Error::Validation(format!("malformed gap member {pair}")))?;
        let kind = crate::store::gaps::GapKind::parse(kind)
            .ok_or_else(|| Error::Validation(format!("unknown gap kind {kind}")))?;
        members.push((kind, id.to_string()));
    }
    for (kind, id) in members {
        match tenant.core.store.dismiss_gap(kind, &id).await {
            Ok(()) | Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(().into_response())
}

async fn gap_dismiss(tenant: Tenant, Path((kind, id)): Path<(String, String)>) -> Result<Response> {
    let kind = crate::store::gaps::GapKind::parse(&kind)
        .ok_or_else(|| Error::Validation(format!("unknown gap kind {kind}")))?;
    tenant.core.store.dismiss_gap(kind, &id).await?;
    Ok(axum::http::StatusCode::OK.into_response())
}

/// Chips per row. Long enough to cover a real vocabulary, short enough that the
/// row stays a row.
pub(crate) const FACET_LIMIT: usize = 12;

/// One offer, flattened for the template. Every decision — which rung, which
/// blocks, how the stamp reads — is made here, so the template holds no logic
/// and a new block in the encoder changes no markup.
#[derive(Default)]
pub struct OfferView {
    pub id: String,
    pub title: String,
    /// The first line or so of what the artifact says.
    ///
    /// A title is not enough to decide whether to open something. On the two
    /// established rungs the reason line carries the rest of the card, but the
    /// random card claims no reason by design — and a card that is a title and
    /// nothing else asks to be clicked on faith. This says what the thing is
    /// without claiming why it is here, which is the one thing the random rung
    /// must not do.
    pub snippet: String,
    /// What the line leads with. Fixed wording for the two established rungs;
    /// for a thin one it is the count in words, because "Twice before" is the
    /// honest thing to say about two occurrences and "Pattern" is not. Empty
    /// on the random card, which claims nothing.
    pub rung: String,
    /// The blocks that decided it, joined. Empty on the lower two rungs.
    pub blocks: String,
    /// `08.08., 15:04`, or empty.
    pub when: String,
    /// `?rec=<slot>&rung=<rung>`, or empty — what tells `artifact_detail` this
    /// open came from an offer, and which rung it was offered on.
    pub rec: String,
    /// The raw bundle and the contribution numbers, for the `<details>`.
    pub detail: String,
    /// The rung's machine name — `pattern`, `similar`, `tentative`, `random`.
    /// What the impression confirmation posts back, and what `Rung::parse`
    /// reads at the other end.
    pub kind: String,
    /// The winning cluster's slot as text, or empty on the random card, which
    /// has no cluster.
    pub slot: String,
}

#[derive(Template, Default)]
#[template(path = "_context.html")]
struct ContextTemplate {
    offer: Option<OfferView>,
}

#[derive(serde::Deserialize)]
struct ContextForm {
    #[serde(default)]
    bundle: String,
}

/// One endpoint, two jobs: it writes the situation and answers with the
/// fragment. Recording happens even when nothing is recommended — a base that
/// has learned nothing yet is exactly the one that most needs its situations
/// written down.
async fn context_offer(tenant: Tenant, Form(f): Form<ContextForm>) -> Result<Response> {
    if !tenant.core.recommends() {
        return Ok(HtmlTemplate(ContextTemplate::default()).into_response());
    }
    let bundle = crate::core::context::parse_bundle(&f.bundle);
    tenant
        .core
        .record_context_event(&f.bundle, &bundle, Some(&tenant.user.subject));

    // A recommendation that cannot be computed is not worth a 500: the area is
    // what it was yesterday, which is empty.
    let offer = tenant
        .core
        .offer(Some(&tenant.user.subject), &bundle)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not build a recommendation");
            None
        });

    // Nothing is recorded here. This function *computes* an offer; whether a
    // person was ever shown one is a different fact, and only the browser
    // knows it — the fetch races the first keystroke, so an answer that
    // arrives after the box has been typed in is dropped client-side and was
    // never on screen. Recording at this point counted those, and they are not
    // a random sample: a visit that goes straight to typing is exactly a visit
    // where nothing would have been clicked. That is a structural zero
    // folded into the denominator of the one number the block weights are
    // meant to be fitted against later. `/ui/context/seen` is the other half.
    // Fetched here rather than carried on `Offer`: the recommender ranks
    // artifacts and has no business knowing how a card reads. A row that has
    // gone since the profile was built leaves an empty snippet, which is the
    // card it was before this line existed rather than an error.
    let snippet = match &offer {
        Some(o) => match tenant.core.store.get_artifact(&o.artifact_id).await {
            Ok(c) => markdown::snippet(&c.text, 160),
            Err(_) => String::new(),
        },
        None => String::new(),
    };
    Ok(HtmlTemplate(ContextTemplate {
        offer: offer.map(|o| offer_view(o, snippet)),
    })
    .into_response())
}

#[derive(serde::Deserialize)]
struct SeenForm {
    artifact_id: String,
    rung: String,
    slot: Option<i64>,
}

/// The browser confirming an offer actually reached the screen.
///
/// Posted by `app.js` after the fragment is swapped in and not dismissed, and
/// it is what writes `recommended_shown`. The pair it forms with
/// `recommended_open` is the hit rate on Ops, so both halves have to mean what
/// they say: shown is shown.
///
/// Everything in the form comes from a page and a page can be made to say
/// anything, so nothing is trusted. The rung goes through `Rung::parse` — the
/// same gate the open marker passes, and for the same reason: an unrecognised
/// word would appear on Ops as a fifth rung of a four-rung ladder. The
/// artifact must exist. Neither failure is worth a status code, because
/// nothing is waiting on the answer.
async fn context_seen(tenant: Tenant, Form(f): Form<SeenForm>) -> Result<Response> {
    use crate::core::recommend::Rung;
    let Some(rung) = Rung::parse(&f.rung) else {
        return Ok(axum::http::StatusCode::NO_CONTENT.into_response());
    };
    if tenant
        .core
        .store
        .get_artifact(&f.artifact_id)
        .await
        .is_err()
    {
        return Ok(axum::http::StatusCode::NO_CONTENT.into_response());
    }
    tenant.core.record_recommendation(
        &f.artifact_id,
        "recommended_shown",
        rung.as_str(),
        f.slot,
        Some(&tenant.user.subject),
    );
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

fn offer_view(o: crate::core::recommend::Offer, snippet: String) -> OfferView {
    use crate::core::recommend::Rung;
    OfferView {
        // A sentence, not a rung name. "Pattern · weekday, hour, network · like
        // 26.08., 20:36" is this code's own vocabulary read out loud to a
        // reader who has never seen the ladder it comes from; the signals
        // themselves moved into Details, where the rest of the arithmetic
        // already lives.
        rung: match o.rung {
            Rung::Pattern => {
                "Offered because you tend to open things like this around now — like".to_string()
            }
            Rung::Similar => "Offered because it is like what you opened".to_string(),
            // The count, in words a person reads. `weight` is the decayed
            // number the ranking uses and nobody can read 1.9 and know it means
            // twice — so the undecayed count is stored alongside it and said
            // out loud here.
            Rung::Tentative => match o.events {
                0 | 1 => "Offered on one earlier occasion like this —".to_string(),
                2 => "Offered on two earlier occasions like this —".to_string(),
                n => format!("Offered on {n} earlier occasions like this —"),
            },
            // Nothing about the situation produced it, so nothing is claimed.
            Rung::Random => String::new(),
        },
        blocks: o.blocks.join(", "),
        // The device's own reading of when this happened, in the zone it
        // happened in. One date format, and the whole of the third part of the
        // line.
        when: o
            .at
            .map(|at| {
                let t = crate::core::context::local_time(at, o.at_tz.as_deref(), None);
                format!(
                    "{:02}.{:02}., {:02}:{:02}",
                    t.day,
                    t.month,
                    t.hour as u32,
                    ((t.hour % 1.0) * 60.0).round() as u32
                )
            })
            .unwrap_or_default(),
        // The rung rides on the link because that is the only place it still
        // exists: the offer was computed on a previous request, and Ops's
        // breakdown is a breakdown only if the click knows which rung it came
        // from. Every rung, including the floor: the random card has no cluster
        // and so no slot, and hanging the whole marker off `slot` left it
        // linking like an ordinary result — its opens counted as opens, so its
        // hit rate read zero for ever and the card it drew was fed back into
        // the profile at full weight. `rec` is the slot when there is one;
        // `rung` is always there.
        rec: match o.slot {
            Some(s) => format!("?rec={s}&rung={}", o.rung.as_str()),
            None => format!("?rung={}", o.rung.as_str()),
        },
        // The machine name of the rung, and the slot as text. Not for reading —
        // these are what the browser posts back to confirm the offer reached
        // the screen, and they are the same two values the link carries on a
        // click, so the shown and the open agree about what was offered.
        kind: o.rung.as_str().to_string(),
        slot: o.slot.map(|s| s.to_string()).unwrap_or_default(),
        id: o.artifact_id,
        title: o.title,
        snippet,
        detail: o.detail,
    }
}

/// Append `value` to a facet row if the store did not report it. `count` is 0
/// because the two reasons it is missing — nothing carries it, or it was
/// crowded out of the top `FACET_LIMIT` — are not distinguishable from here;
/// the template renders no number rather than a wrong one.
pub(crate) fn ensure_facet(row: &mut Vec<crate::vector::FacetCount>, value: &str) {
    if value.is_empty() || row.iter().any(|f| f.value == value) {
        return;
    }
    row.push(crate::vector::FacetCount {
        value: value.to_string(),
        count: 0,
    });
}

#[derive(serde::Deserialize)]
pub(crate) struct UiSearchParams {
    #[serde(default)]
    pub(crate) q: String,
    #[serde(default)]
    pub(crate) tags: Option<String>,
    #[serde(default)]
    pub(crate) category: Option<String>,
    /// The refining pass. Absent on every keystroke — typing gets vector
    /// order at embedding speed — and `true` on the request app.js fires once
    /// the operator stops, whose answer reorders the rail it just painted.
    #[serde(default)]
    pub(crate) rerank: bool,
    /// The search event this page is holding — the id the last answer handed
    /// it, sent back so a typing burst folds into its own chain. A second tab
    /// carries a different one, or none yet, and the two never collide. See
    /// `Store::record_search`.
    #[serde(default)]
    pub(crate) fold: Option<String>,
    /// Ask the rail to say why a row is where it is. Off unless the link
    /// carries it: the line is for an operator looking into a ranking, not
    /// something every keystroke paints. `/ui?explain=1` puts it on the form
    /// as a hidden field, and `hx-params` names it so the fragment request
    /// keeps it.
    ///
    /// Through `query_flag` rather than a bare `bool`, which `serde_urlencoded`
    /// reads only as `true`/`false`: `?explain=1` — the spelling the link and
    /// every hand-written URL carry — was a 400 for the whole fragment, so
    /// asking why a row ranked emptied the rail instead of explaining it.
    #[serde(default, deserialize_with = "crate::web::api::query_flag")]
    pub(crate) explain: Option<bool>,
}

/// Function words carry no signal and appear in every chunk, so highlighting
/// them marks the whole card and hides the terms that actually matched.
const STOPWORDS: [&str; 40] = [
    "a", "an", "the", "and", "or", "but", "if", "of", "to", "in", "on", "at", "by", "for", "with",
    "from", "into", "is", "are", "was", "were", "be", "been", "do", "does", "did", "how", "what",
    "when", "where", "why", "which", "that", "this", "it", "its", "my", "i", "you", "can",
];

/// Query terms worth marking in a result, space separated for the client.
fn highlightable_terms(query: &str) -> String {
    crate::vector::sparse::tokenize(query)
        .into_iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_tags(t: Option<String>) -> Vec<String> {
    t.map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// Past this the box is holding something to be kept, not something to be
/// searched for. Generous on purpose: it is a bound on cost, not a rule about
/// how to phrase a query, and a query phrased as a whole situation is still
/// only a few hundred characters.
const MAX_QUERY_CHARS: usize = 2000;

pub(crate) async fn search_results(
    tenant: Tenant,
    // Alongside the tenant, not instead of it: a `Tenant` is cached across
    // requests and cannot carry the session this one came in on, which is the
    // whole of what keeps the sitting at the web door.
    identity: crate::auth::Identity,
    headers: axum::http::HeaderMap,
    Query(p): Query<UiSearchParams>,
) -> Result<Response> {
    // Resolved once, at the top: the fate echo needs it on both roads out of
    // here, and by the time the second one renders the subject has been moved
    // into the recall.
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
    // Clearing the box fires a request with an empty query. That is not an
    // error, and it is not "No matches." either — an empty ResultsTemplate
    // rendered exactly that, which is a claim about a base nobody searched.
    // An empty box is the idle state, so the idle rail comes back: the base
    // introducing itself, with its heading swapped in out of band the same
    // way a result count is.
    // A box holding more than a query is the idle state too. The one box is
    // also where a chapter gets pasted to be captured, and /ui/capture?from_ask=
    // opens it holding a whole model answer: the template suppresses the `load`
    // search for those doors, but the first keystroke afterwards fired one
    // anyway, and an incremental search is an embedding call, an activation
    // bump and a coalesced Judge-queue row — for a paragraph nobody was
    // looking for. Nothing anyone types as a question comes near this; the
    // limit is on the door rather than in app.js because it is the embedder's
    // bill either way, whatever the client was.
    if p.q.trim().is_empty() || p.q.chars().count() > MAX_QUERY_CHARS {
        let mut t = idle_foot(&tenant, true).await?;
        // A box holding a whole document is not searched, but its fate is
        // still worth a line: this is exactly the paste the size fork will
        // store verbatim, and the echo is what says so before Capture.
        if !p.q.trim().is_empty() {
            t.echo = render_echo(&fate_echo(&tenant.core, &p.q, lang));
        }
        return Ok(HtmlTemplate(t).into_response());
    }

    // The same terms the sparse branch derives, handed to the client so
    // highlighting never has to touch the sanitized HTML on this side.
    // Function words are dropped: a query phrased as a situation is mostly
    // stopwords, and highlighting every "to" marks the whole card.
    let terms = highlightable_terms(p.q.trim());
    // The wording this rail is about to be drawn for, kept because `p.q` is
    // about to be moved into the search. Only the gap button reads it.
    let q = p.q.trim().to_string();
    // What this sitting is working on. A typing burst folds into one entry
    // here as it does in the log, so what is carried is the query that was
    // meant rather than every prefix of it.
    if let Some(sess) = &identity.session {
        tenant.core.sittings.queried(
            sess,
            p.q.trim(),
            crate::store::now(),
            tenant.core.pursuit.idle_secs as i64,
        );
    }
    // Read into a local: a lock guard living inside the call expression would
    // still be held across the await, and a future holding one is not `Send`.
    let cap = tenant
        .core
        .ranking
        .read()
        .expect("ranking lock")
        .per_source_cap;
    let explain = p.explain.unwrap_or(false);
    let (hits, outcome) = tenant
        .core
        .search_with(
            &SearchQuery {
                q: p.q,
                limit: 0,
                tags: split_tags(p.tags),
                category: p.category.filter(|c| !c.is_empty()),
                // Incremental: a prefix must not stamp what it happened to match.
                mark: false,
                include_deprecated: false,
                include_superseded: false,
                rerank: p.rerank,
                explain,
            },
            cap,
            // Scoped to the operator, because coalescing folds a keystroke into
            // the query it was an early spelling of, and two people typing at
            // once are not spelling the same thing.
            crate::store::feedback::Door::Ui
                .by(tenant.user.subject)
                // The live sitting, for priming. Off unless `sitting.prime` is
                // on, and impossible at any door with no session.
                .in_sitting(identity.session.clone())
                // The event this page is already holding, so a burst folds into
                // its own chain and not into whatever this operator's other tab
                // wrote last. Empty on the first search of a page.
                .folding_onto(p.fold.filter(|f| !f.is_empty())),
        )
        .await?;

    // The ranked answer and what it recalled are two lists on the page, and one
    // list here: an associated hit carries the id of the hit that recalled it,
    // and the title is looked up among the ranked ones rather than fetched.
    let titles = ranked_titles(&hits);
    let (ranked, recalled): (Vec<_>, Vec<_>) = hits.into_iter().partition(|h| h.via.is_none());
    let mut results: Vec<RenderedResult> = ranked
        .into_iter()
        .enumerate()
        .map(|(i, h)| render_hit(i, h, &titles, explain))
        .collect();
    // What each hit's document does next, in one read for the whole list. Only
    // over the ranked rows: an associated one is not an answer to the query,
    // and telling the reader to read on from something the query never matched
    // is an invitation into a document they did not ask about.
    //
    // Best-effort, exactly like the reach `ask` makes: a marker is a bonus, and
    // failing to read one must not cost the results that were already found.
    let next = match tenant
        .core
        .store
        .continuations_of(
            &results
                .iter()
                .map(|r| r.artifact_id.clone())
                .collect::<Vec<_>>(),
        )
        .await
    {
        Ok(next) => next,
        Err(e) => {
            tracing::warn!(error = %e, "could not read what these hits continue into");
            Default::default()
        }
    };
    mark_continuations(&mut results, &next);
    let associated: Vec<RenderedResult> = recalled
        .into_iter()
        .map(|h| render_hit(0, h, &titles, explain))
        .collect();
    let echo = render_echo(&fate_echo(&tenant.core, &q, lang));
    let mut res = HtmlTemplate(ResultsTemplate {
        // Only when *every* result is loose. One weak hit at the bottom of a
        // good list is ordinary — it is the tail of any ranking — and saying
        // "nothing matches" over a list that plainly does would train the
        // operator to ignore the warning. Computed from `results` only: an
        // association is not an answer to the query and cannot make the
        // answer look better or worse than it was.
        all_weak: !results.is_empty() && results.iter().all(|r| r.weak),
        results,
        associated,
        terms,
        // From the search's outcome, not from the request's intent: a rerank
        // that failed or was skipped answered in vector order, and the tick
        // would assert a confirmation that never took place.
        reranked: outcome.timing.reranked,
        event_id: outcome.event,
        echo,
        q,
    })
    .into_response();
    // Measured as before, reported where a browser already knows to show it.
    // On the page it was a line of debug telemetry floated beside the results
    // — a number nobody searching has a use for, in a place the eye lands.
    if let Ok(v) = format!(
        "embed;dur={}, total;dur={}",
        outcome.timing.embed_ms, outcome.timing.total_ms
    )
    .parse()
    {
        res.headers_mut().insert("server-timing", v);
    }
    Ok(res)
}

/// The ranked hits' titles, by artifact id, for the associated rows that name
/// the hit that recalled them.
///
/// Untitled hits are left out rather than named "Untitled": a row reading
/// `seen together with "Untitled"` says nothing and looks like it does.
fn ranked_titles(
    hits: &[crate::core::search::SearchResult],
) -> std::collections::HashMap<String, String> {
    hits.iter()
        .filter(|h| h.via.is_none())
        .filter_map(|h| Some((h.artifact_id.clone(), h.title.clone()?)))
        .collect()
}

/// The rail's half of the explanation: the consequence, in a sentence.
///
/// Deliberately not the MCP form. An agent reads a list of stages; a person
/// reads why this row is above the one below it, and a stage that changed
/// nothing is not part of that answer.
fn why_ranked(e: &crate::core::explain::HitExplanation) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &e.rerank
        && s.from > s.to
    {
        parts.push("moved up by the reranker".to_string());
    }
    if matches!(e.cap, crate::core::explain::CapEffect::Refilled) {
        parts.push("kept only because one source filled the list".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub(crate) fn render_hit(
    position: usize,
    h: crate::core::search::SearchResult,
    titles: &std::collections::HashMap<String, String>,
    // Whether the door asked for the explanation. The object is on every
    // ranked hit either way — the flag gates rendering and nothing else.
    explain: bool,
) -> RenderedResult {
    RenderedResult {
        artifact_id: h.artifact_id,
        // Empty, never "Untitled": a verbatim passage has no title by design,
        // and a rail of "Untitled" headings is a column of a word that says
        // nothing where a name would say something. The row shows its snippet.
        title: h.title.unwrap_or_default(),
        titled_by_corpus: h.titled_by_corpus,
        html: markdown::render(&h.text),
        snippet: markdown::snippet(&h.text, 140),
        category: h.category,
        tags: h.tags,
        corpus_id: h.corpus_id,
        // No rank on an associated hit — the same reasoning that drops the
        // rank on a weak one: a rank is a claim about standing among answers,
        // and this did not compete for one.
        rank: if h.weak || h.via.is_some() {
            String::new()
        } else {
            format!("#{}", position + 1)
        },
        weak: h.weak,
        primed: h.primed,
        in_sitting: h.in_sitting,
        due_in: h.due_in.clone(),
        past_cliff: h.past_cliff,
        retired: h.retired,
        via_title: h.via.as_ref().and_then(|v| titles.get(v).cloned()),
        reason: h.reason.clone(),
        why_ranked: explain
            .then(|| h.explanation.as_ref().and_then(why_ranked))
            .flatten(),
        model_written: h.model_written,
        origin_count: h.origin_count,
        // Filled by `mark_continuations` over the finished list: whether a
        // passage's continuation is *on the page* is a fact about the list, not
        // about the hit, and one row cannot answer it.
        continues: false,
        continues_in: String::new(),
    }
}

/// Say, on each row, what its document does next.
///
/// `next` maps an artifact to the passage that follows it — `Store::
/// continuations_of`, one read for the whole list. Two outcomes, and they are
/// deliberately exclusive: a continuation that *placed* is named by its rank,
/// because the row is already on the page and offering to fetch it would claim
/// two things are there when one is; a continuation that did not place is
/// announced as such, and the pane is where it gets read.
///
/// A row with no rank is not a destination. Associated rows and weak ones carry
/// no rank by design, and `setzt sich fort in` followed by nothing is worse
/// than the marker's absence — so such a continuation falls back to the plain
/// announcement, which is still true.
pub(crate) fn mark_continuations(
    results: &mut [RenderedResult],
    next: &std::collections::HashMap<String, String>,
) {
    let ranks: std::collections::HashMap<&str, &str> = results
        .iter()
        .filter(|r| !r.rank.is_empty())
        .map(|r| (r.artifact_id.as_str(), r.rank.as_str()))
        .collect();
    let marks: Vec<(bool, String)> = results
        .iter()
        .map(|r| match next.get(&r.artifact_id) {
            None => (false, String::new()),
            Some(n) => match ranks.get(n.as_str()) {
                Some(rank) => (false, (*rank).to_string()),
                None => (true, String::new()),
            },
        })
        .collect();
    for (r, (continues, continues_in)) in results.iter_mut().zip(marks) {
        r.continues = continues;
        r.continues_in = continues_in;
    }
}

/// The ten most recent captures, under the box that made them.
///
/// Ten rather than everything: an index of every corpus was a page nobody read,
/// and anything older than the last handful is found by searching for what it
/// says rather than by scrolling a list of what it is called.
/// Recent lists ten captures, and synthesis names a capture by lifting a
/// heading out of it. A heading repeats across every document that carries it,
/// so six rows read `HOCHSCHULE MITTWEIDA` and named nothing — the one column
/// that exists to tell captures apart could not.
///
/// Where a label is not unique in the list, the capture's opening words are
/// appended, because that is the one thing that differs between them. Three
/// rows are left alone: one whose label was already unique, because the suffix
/// is a repair rather than a decoration; one with no opening words to offer,
/// because `document · document` tells no one anything; and one already called
/// by its opening words, because a label repeated back to itself is worse than
/// the collision.
fn disambiguate_labels(rows: &mut [QueueRow]) {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in rows.iter() {
        *counts.entry(r.label.as_str()).or_insert(0) += 1;
    }
    let collides: std::collections::HashSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(l, _)| l.to_string())
        .collect();
    for r in rows.iter_mut() {
        // The opening is the capture's first words and the label is a heading
        // lifted out of those same words, so the opening usually begins by
        // repeating it: "HOCHSCHULE MITTWEIDA" beside "HOCHSCHULE MITTWEIDA
        // Ein Verfahren zur…". Only the part that differs is worth the room —
        // and the doubled reading is what the deployment showed, truncated to
        // "HOCHSCHULE MITTWEIDA · HOCHSCH…", which is how a repair that had
        // run looked exactly like one that never had.
        if let Some(rest) = r.opening.strip_prefix(r.label.as_str()) {
            let rest = rest
                .trim_start_matches([' ', ':', '·', '—', '-', ','])
                .trim();
            r.opening = rest.to_string();
        }
        // Kept beside the label rather than folded into it. Appending it was
        // the whole of this repair, and the row then truncated the appended
        // half away — `.qtitle` is one `nowrap` line — so six captures still
        // read `HOCHSCHULE MITTWEIDA · HOCHSCH…` and the column that exists to
        // tell them apart still could not. A field of its own has somewhere to
        // wrap to.
        if !(collides.contains(&r.label) && !r.opening.is_empty() && r.opening != r.label) {
            r.opening.clear();
        }
    }
}

async fn queue_fragment(tenant: Tenant) -> Result<Response> {
    let mut rows = Vec::new();
    let corpora = tenant.core.store.list_corpora(10, 0).await?;
    // Asked once for the page rather than once per row: this fragment is polled
    // while anything is in flight, and the coverage read is a three-way join.
    // Failure is the empty map for the reason a missing capture is: the line is
    // what a capture did beyond being stored, and a page that cannot say so
    // says nothing rather than failing to render the queue.
    let covered = tenant
        .core
        .store
        .gaps_covered_by_each(&corpora.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
        .await
        .unwrap_or_default();
    for s in corpora {
        let (resolved, total) = tenant.core.store.segment_progress(&s.id).await?;
        let progress = (total > 0 && resolved < total).then(|| format!("{resolved}/{total}"));
        // Terminal states: nothing else will happen without someone asking.
        // NeedsReview is terminal in this sense — it is waiting on a person.
        let in_flight = !matches!(
            s.status,
            CorpusStatus::Ready
                | CorpusStatus::Failed
                | CorpusStatus::NeedsReview
                | CorpusStatus::Partial
        );
        let low_coverage = s
            .coverage
            .is_some_and(|c| c < crate::infer::verify::LOW_COVERAGE);
        rows.push(QueueRow {
            progress,
            locatable: total > 0,
            coverage: s
                .coverage
                .map(|c| format!("{:.0}%", c * 100.0))
                .unwrap_or_else(|| "—".into()),
            low_coverage,
            // Until synthesis names it, a capture is called by its opening
            // words — the only thing anything knows about it, and the only
            // thing that tells three captures pasted in a row apart. `unnamed`
            // is what says the name is still coming; the label itself is not
            // the place to say it.
            opening: markdown::snippet(&s.raw_text, 60),
            label: corpus_label(s.title_hint.clone(), &s.raw_text, &s.origin),
            unnamed: s.title_hint.is_none() && in_flight,
            in_flight,
            settled: matches!(s.status, CorpusStatus::Ready),
            badge: status_badge(&s.status),
            status: s.status.as_str().to_string(),
            artifact_count: tenant.core.store.count_artifacts_for_corpus(&s.id).await?,
            created: fmt_time(s.created_at),
            covered: covered
                .get(&s.id)
                .map(|gs| gs.iter().map(|g| g.text.clone()).collect())
                .unwrap_or_default(),
            id: s.id,
        });
    }
    disambiguate_labels(&mut rows);
    let active = rows.iter().any(|r| r.in_flight);
    Ok(HtmlTemplate(QueueTemplate { rows, active }).into_response())
}

/// What to call an artifact in a place that must call it something.
///
/// A title is what makes two near-identical artifacts tellable apart at a
/// glance; falling back to the opening of the body beats an id. Sixty
/// characters of raw body was that fallback, and it is where the sitting's
/// "…darin vo" and the dedupe queue's `Keep "- schneller Schreibzugriff …"`
/// both came from. The rule lives in one place now — see
/// `markdown::stand_in_title` — so the sitting and the pair cards
/// cannot drift apart again.
pub(crate) fn title_of(c: &crate::store::artifacts::Chunk) -> String {
    // The stored title goes through the same rule, because synthesis writes it
    // and nothing stopped it writing markup into one: Housekeeping listed a
    // merged artifact as "**Was nicht abgedeckt ist:** * Es werden keine". A
    // title is a name, and a name is never marked up.
    let name = match &c.title {
        Some(t) => crate::web::markdown::stand_in_title(t, 80),
        None => crate::web::markdown::stand_in_title(&c.text, 60),
    };
    // `stand_in_title` strips markup and leading punctuation, so a body that is
    // only those leaves nothing at all — a rule the sitting rail cannot use,
    // since a list entry with no text is a link nobody can see or click. The id
    // is a poor name and a working one.
    if name.is_empty() {
        return c.id.clone();
    }
    name
}

/// Whether a passage stops in the middle of a sentence.
///
/// The pane rendered "…der bereits vorgestellte Einsatz von" and stopped,
/// while the source column beside it showed the rest — a segmentation boundary
/// landing mid-clause, with nothing on the artifact saying it had. This cannot
/// know whether a boundary was semantic; it can tell that a sentence did not
/// finish, which is the only claim the link it drives makes.
///
/// A closing bracket or quote after the stop counts as the stop: "…(siehe
/// unten)" ends a sentence as much as the period would. A table row or a list
/// marker does not — that passage ended where its structure ended, not
/// mid-thought.
pub(crate) fn ends_mid_sentence(text: &str) -> bool {
    let t = text.trim_end();
    let t = t.trim_end_matches([')', ']', '"', '»', '\'', '“', '”']);
    match t.chars().last() {
        None => false,
        // A table row or a fence closes on its own punctuation.
        Some('|') | Some('`') => false,
        Some(c) => !matches!(c, '.' | '!' | '?' | ':' | ';' | '…'),
    }
}


#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundTemplate {}

/// The app's own answer to a path it does not have.
///
/// Only for the pages: an agent asking `/api/v1` for a route that does not
/// exist must not be handed an HTML document to parse, which is what a router
/// fallback would do to every door at once. `/api/` is not the only such door.
/// This is the fallback for the whole application, so a missing static asset,
/// an unrouted `/mcp` path and every request that is not a `GET` arrive here
/// too — and each of them was answered with the page, which for a browser with
/// no session meant a 401 that `redirect_unauthenticated_browsers` turned into
/// the login screen. A stylesheet that 303s to a login is not a missing
/// stylesheet, it is a mystery. They get the plain 404 they asked for.
///
/// The page is behind a session like every other page. `Identity` is asked for
/// optionally rather than required so that the `/api` answer above stays a 404
/// for a caller with no credentials, but a browser with no session gets the
/// same 401 the rest of the app gives it — which
/// `redirect_unauthenticated_browsers` turns into the login. Without that, the
/// one path nobody routed was the one path that rendered the whole nav to a
/// visitor with no session.
pub async fn not_found(
    tenant: Option<Tenant>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> Response {
    let path = uri.path();
    let machine = path.starts_with("/api/")
        || path.starts_with("/assets/")
        || path == "/mcp"
        || path.starts_with("/mcp/");
    if machine || method != axum::http::Method::GET {
        return (axum::http::StatusCode::NOT_FOUND, "not found").into_response();
    }
    let Some(_tenant) = tenant else {
        return crate::error::Error::Unauthorized.into_response();
    };
    let page = NotFoundTemplate {};
    match askama::Template::render(&page) {
        Ok(html) => (
            axum::http::StatusCode::NOT_FOUND,
            axum::response::Html(html),
        )
            .into_response(),
        Err(_) => (axum::http::StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

pub fn ui_router() -> Router<AppState> {
    Router::new()
        // The bare domain and `/ui` are the same door, and both open on the
        // page the app starts at. Without the first of them the router simply
        // had no answer for `/`, and a browser typing the domain got a 404 —
        // signed in or not, because an unmatched path never reaches the
        // authentication that would have redirected it to a login.
        .route("/ui/context", post(context_offer))
        .route("/ui/context/seen", post(context_seen))
        .route("/ui/queue", get(queue_fragment))
        // An installed PWA may still hold /ui/browse as its start URL, and a
        // bookmark outlives the page it pointed at.
        // Takes an `Identity` like every other page: a gone page must still send
        // a signed-out visitor to sign in rather than bouncing them onward.
        .route(
            "/ui/browse",
            get(|_: Tenant| async { Redirect::to("/ui/capture") }),
        )
        .route("/ui/gaps/{kind}/{id}/dismiss", post(gap_dismiss))
        .route("/ui/gaps/forget", post(gap_forget))
        // The page's spoken names. Housekeeping was the nav word for a while,
        // and `/ui/ops` still answers as the old door — but this goes straight
        // to the page rather than chaining through that shim, and it takes an
        // `Identity` like every other `/ui` route: a redirect that answers
        // before the session is checked tells an anonymous caller which paths
        // exist.
        .route(
            "/ui/housekeeping",
            get(|_: Tenant| async { Redirect::to("/ui/insights") }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Chunk` with every field named, so a test can say the one thing it
    /// cares about and nothing else. `Chunk` has no `Default` on purpose —
    /// most of its fields are decisions — so the fixture carries them here
    /// rather than putting a misleading default on the type.
    fn chunk_fixture(title: Option<&str>, text: &str) -> crate::store::artifacts::Chunk {
        crate::store::artifacts::Chunk {
            id: "a".into(),
            corpus_id: Some("s".into()),
            provenance: crate::store::artifacts::Provenance::Captured,
            source_count: 0,
            ordinal: 56,
            text: text.into(),
            corpus_span: None,
            title: title.map(str::to_string),
            category: None,
            tags: vec![],
            embed_state: crate::store::artifacts::EmbedState::Embedded,
            embed_model: None,
            created_at: 0,
            embed_rev: 0,
            segment_idx: None,
            flags: vec![],
            flag_detail: None,
            superseded_by: None,
            caveats: vec![],
            status: crate::store::artifacts::ArtifactStatus::Active,
            last_verified_at: None,
            cues: vec![],
            retired_at: None,
            reaped_at: None,
        }
    }

    fn queue_row_fixture(label: &str, opening: &str) -> QueueRow {
        QueueRow {
            label: label.into(),
            opening: opening.into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_opening_that_repeats_its_own_label_says_only_the_rest() {
        // Found by walking the running app, not by a fixture: the label is a
        // heading lifted out of the capture's first words, so the opening
        // beside it began by repeating it — "HOCHSCHULE MITTWEIDA" over
        // "HOCHSCHULE MITTWEIDA Ein Verfahren zur…".
        let mut rows = vec![
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "HOCHSCHULE MITTWEIDA Ein Verfahren zur Sicherung",
            ),
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "HOCHSCHULE MITTWEIDA Fachbereich Angewandte",
            ),
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].opening, "Ein Verfahren zur Sicherung");
        assert_eq!(rows[1].opening, "Fachbereich Angewandte");
    }

    #[test]
    fn a_disambiguated_row_shows_the_part_that_distinguishes_it() {
        // `disambiguate_labels` appended the opening words and `.qtitle`
        // truncated them away, so six rows still read "HOCHSCHULE MITTWEIDA ·
        // HOCHSCH…" and the one column that exists to tell captures apart
        // still could not. The opening needs an element of its own.
        let mut rows = vec![
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "Fachbereich Angewandte Computer- und Biowissenschaften",
            ),
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "Ein Verfahren zur Sicherung fluechtiger Daten",
            ),
            queue_row_fixture("SQLite und WAL", "Pragma-Abfragen"),
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(
            rows[0].label, "HOCHSCHULE MITTWEIDA",
            "the label keeps its own name; the opening is said beside it"
        );
        assert!(!rows[0].opening.is_empty(), "nothing tells row 0 apart");
        assert!(
            rows[2].opening.is_empty(),
            "a unique label needs no opening beside it: {:?}",
            rows[2].opening
        );
        let html = askama::Template::render(&QueueTemplate {
            rows,
            active: false,
        })
        .unwrap();
        assert!(html.contains("qtitle-opening"), "{html}");
        assert!(html.contains("Fachbereich Angewandte"), "{html}");
    }


    #[test]
    fn a_gap_row_offers_a_box_to_fill_it_and_a_word_to_forget_it() {
        // One row per hole, whether the sweep named it or not, and nothing on
        // it about how the question failed: a person deciding what to do
        // about a hole has the same two choices whichever way it was made.
        // `_gaps.html` is only ever included, so it has no template struct of
        // its own; this is one, standing in for the page that includes it.
        #[derive(Template)]
        #[template(path = "_gaps.html")]
        struct Gaps {
            gaps: Vec<GapGroup>,
        }
        let html = askama::Template::render(&Gaps {
            gaps: vec![GapGroup {
                label: "Chipkarten".into(),
                members: vec![
                    GapMember {
                        kind: "ask".into(),
                        id: "g1".into(),
                        text: "wie werden bei chipkarten die private keys geschützt?".into(),
                    },
                    GapMember {
                        kind: "unmatched".into(),
                        id: "s2".into(),
                        text: "chipkarte schlüssel".into(),
                    },
                ],
            }],
        })
        .unwrap();
        assert!(
            html.contains(r#"hx-post="/ui/capture""#),
            "no box to fill it: {html}"
        );
        assert!(
            html.contains(r#""members": "ask:g1,unmatched:s2""#),
            "forget names every question in the group: {html}"
        );
        assert!(!html.contains("ask again"), "{html}");
        assert!(!html.contains("covered"), "{html}");
        assert!(
            !html.contains("nothing near") && !html.contains("chipkarte schlüssel"),
            "a group is its name, not its members: {html}"
        );
    }


    #[test]
    fn a_sweep_stage_reads_as_words_and_keeps_its_identifier() {
        // Housekeeping listed `arm_dedupe`, `link_judge` and `segment_window`
        // — the identifiers the queue keys on, on a page a person reads.
        assert_eq!(sweep_label("arm_dedupe"), "Arming dedupe");
        assert_eq!(sweep_label("consolidate"), "Consolidating");
        assert_eq!(sweep_label("retention"), "Retention");
        assert_eq!(sweep_label("link_judge"), "Judging links");
        // An identifier nobody has worded yet is shown, never swallowed: a new
        // sweep must not render as a blank cell.
        assert_eq!(sweep_label("some_new_sweep"), "some_new_sweep");
    }

    #[test]
    fn every_stage_the_queue_can_run_has_a_word_for_it() {
        // The list above is a map, and a map goes stale silently. This is what
        // notices when a stage is added and nothing on Housekeeping names it.
        for stage in crate::store::jobs::Stage::ALL {
            let id = stage.as_str();
            assert_ne!(
                sweep_label(id),
                id,
                "no wording for the {id} stage — add one to `sweep_label`"
            );
        }
    }

    #[test]
    fn a_stored_title_that_carries_markup_is_shown_without_it() {
        // Housekeeping listed a merged artifact as "**Was nicht abgedeckt
        // ist:** * Es werden keine". Synthesis writes the title, and nothing
        // stopped it writing markup into one — a title is a name, and a name
        // is never marked up.
        let t = title_of(&chunk_fixture(
            Some("**Was nicht abgedeckt ist:** * Es werden keine"),
            "body",
        ));
        assert!(t.starts_with("Was nicht abgedeckt ist:"), "{t:?}");
        assert!(!t.contains("**"), "{t:?}");
        assert_eq!(
            title_of(&chunk_fixture(Some("# 3.4.2 FESTE MFT RECORDS"), "body")),
            "3.4.2 FESTE MFT RECORDS"
        );
        // An ordinary title passes through untouched.
        assert_eq!(
            title_of(&chunk_fixture(Some("LevelDB: Funktionsweise"), "body")),
            "LevelDB: Funktionsweise"
        );
    }

    #[test]
    fn a_passage_that_stops_mid_sentence_is_known_to_have_stopped() {
        // The pane ended "…der bereits vorgestellte Einsatz von" while the
        // source column beside it showed the rest of the sentence. The pane
        // cannot know whether a boundary was semantic; it can tell that a
        // sentence did not finish.
        assert!(ends_mid_sentence(
            "Die erste Vorkehrung ist der bereits vorgestellte Einsatz von"
        ));
        assert!(!ends_mid_sentence("Das ist der ganze Satz."));
        assert!(!ends_mid_sentence("Ist das der ganze Satz?"));
        assert!(!ends_mid_sentence("Ein Listenpunkt:"));
        // A passage ending in a fenced block or a table row has not stopped
        // mid-sentence; it has stopped where its structure ended.
        assert!(!ends_mid_sentence("| ext4 | ja |"));
        assert!(!ends_mid_sentence(""));
    }

    #[test]
    fn the_copy_control_does_not_sit_on_top_of_the_passage() {
        // A fenced code sample is short and the button over its top-right
        // corner cost nothing. A passage kept as the document wrote it is one
        // `<pre>` from end to end, and there the button landed on the first
        // sentence of the artifact — at both widths.
        let css = include_str!("../../assets/css/30-components.css");
        assert!(
            css.contains(".codewrap { position: relative; padding-top:"),
            "no room is reserved for the copy control"
        );
    }

    #[tokio::test]
    async fn a_search_with_nothing_open_leaves_the_grid_free_to_widen_the_rail() {
        // 22rem of rail beside a thousand pixels holding one line of
        // placeholder is the whole complaint. `pane-open` is what the pane
        // gains when something is opened into it, so its absence on first
        // paint is what the wide-rail rule keys on — see `20-layout.css`.
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/search?q=write+blocker").await;
        assert!(page.contains("regions-rail-focus-source"), "{page}");
        assert!(
            !page.contains("has-selection") && !page.contains("pane-open"),
            "a fresh search already claims something is open: {page}"
        );
    }

    #[test]
    fn the_artifact_pane_shows_a_stored_title_by_the_same_rule_as_the_rest() {
        // Synthesis writes titles and nothing stopped it writing markup into
        // one. Housekeeping showed it cleaned while the corpus page and the
        // pane showed the asterisks — the drift `title_of` was gathered into
        // one place to close, still open on the path that read `c.title`
        // straight.
        assert_eq!(
            artifact_title(&chunk_fixture(
                Some("**Was nicht abgedeckt ist:** * Es werden keine"),
                "body"
            )),
            "Was nicht abgedeckt ist: * Es werden keine"
        );
    }

    #[test]
    fn an_artifact_whose_opening_is_only_markup_still_has_a_name() {
        // `stand_in_title` takes markup and leading punctuation off the front,
        // so a body that is only those leaves nothing at all. The sitting rail
        // rendered that as a list entry with no text — a link nobody can see
        // or click. The id is a poor name and a working one.
        let t = title_of(&chunk_fixture(None, "---"));
        assert!(!t.is_empty(), "a rail entry with no text is not a link");
        assert_eq!(t, "a", "the fixture's id");
    }

    #[test]
    fn the_artifact_pane_does_not_call_a_passage_chunk_fifty_six() {
        // "Chunk 56" is a position in the ingest, not a name for anything a
        // reader asked for. The fixture's ordinal is 56 for exactly that.
        let t = artifact_title(&chunk_fixture(
            None,
            "Die digitale Forensik unterscheidet sich zusätzlich",
        ));
        assert!(!t.starts_with("Chunk"), "{t:?}");
        assert!(t.starts_with("Die digitale Forensik"), "{t:?}");
        assert_eq!(
            artifact_title(&chunk_fixture(Some("SQLite und WAL"), "body")),
            "SQLite und WAL"
        );
    }

    #[test]
    fn an_untitled_artifact_is_named_by_its_opening_not_by_its_first_sixty_bytes() {
        // Both of these came off the deployment: the sitting cut a name
        // mid-word, and "Needs you" offered a button reading
        // `Keep "- schneller Schreibzugriff (…) -"`.
        let t = title_of(&chunk_fixture(
            None,
            "Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort",
        ));
        assert!(!t.ends_with("vo"), "cut mid-word: {t:?}");
        assert_eq!(
            title_of(&chunk_fixture(
                None,
                "- schneller Schreibzugriff (Änderungen vom Key auf Stapel) -"
            )),
            "schneller Schreibzugriff (Änderungen vom Key auf Stapel) -"
        );
        assert_eq!(
            title_of(&chunk_fixture(Some("LevelDB"), "body")),
            "LevelDB",
            "a real title is never replaced"
        );
    }
    use crate::web::test_support::{
        app_holding_something, app_recommending, app_session_and_core,
        app_session_and_core_with_feedback, app_with_cookie, app_with_embedded_corpus,
        app_with_session, artifacts, ask_over_sse, body_of, done_html, drain, flat, form,
        get_body, get_stream, hold_something, post_ask, pulled, searched_app,
        searched_app_tuned, trigger_of,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn highlighting_skips_function_words_but_keeps_short_technical_terms() {
        // A query phrased as a situation is mostly stopwords; marking every
        // "to" and "how" highlights the entire card and hides the real hits.
        let terms = super::highlightable_terms("how do i write an iso to a usb stick with dd");
        assert!(terms.contains("iso"));
        assert!(terms.contains("usb"));
        assert!(terms.contains("dd"), "short technical terms must survive");
        for noise in ["how", "the", " to ", " an ", " with "] {
            assert!(
                !format!(" {terms} ").contains(noise),
                "{noise} should not be highlighted"
            );
        }
    }

    #[tokio::test]
    async fn a_rail_entry_carries_the_chunk_id_it_links_to() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let hits = core
            .search(
                &crate::core::search::SearchQuery {
                    q: "alpha".into(),
                    limit: 0,
                    tags: vec![],
                    category: None,
                    mark: false,
                    include_deprecated: false,
                    include_superseded: false,
                    rerank: true,
                    explain: false,
                },
                crate::store::feedback::Door::Ui,
            )
            .await
            .unwrap();
        let r = super::render_hit(0, hits[0].clone(), &Default::default(), false);

        assert!(
            !r.artifact_id.is_empty(),
            "the rail needs a chunk id to link to"
        );
        assert!(!r.snippet.is_empty(), "the rail shows a plain-text snippet");
        assert!(
            !r.snippet.contains('<'),
            "the snippet must not carry markup"
        );
    }

    #[tokio::test]
    async fn a_failed_segment_is_picked_up_without_anyone_asking() {
        // What replaced the "re-synthesize segment" button. The sweep sees a
        // segment that is not done, queues the corpus, and the run retries it.
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        core.store
            .set_segment_state(
                &out.id,
                0,
                crate::store::segments::SegmentState::Failed,
                Some("boom"),
            )
            .await
            .unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        assert_eq!(crate::jobs::reconcile::run(&core).await.unwrap(), 1);
        let mut found = false;
        let want = crate::jobs::window::unit_target(&out.id, 0);
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::SegmentWindow && j.target_id == want {
                found = true;
            }
        }
        assert!(found, "nothing would ever retry the segment");
    }

    /// A session on the given core, for pages that need a core built a
    /// particular way.
    async fn app_for(core: crate::core::Core) -> (axum::Router, String) {
        app_with_cookie(core).await
    }

    /// A core holding one pair waiting on a person, so "Needs you" has
    /// something to say on whichever page is supposed to be carrying it.
    async fn app_with_a_waiting_pair() -> (axum::Router, String) {
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "The reindex job holds a file descriptor on the old mount.".into(),
                        title: Some("reindex holds an fd".into()),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "The reindex job holds an fd on the old mount.".into(),
                        title: Some("reindex holds an fd (again)".into()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .record_pair_with_detail(&made[0].id, &made[1].id, 0.94, "near duplicate")
            .await
            .unwrap();
        app_for(core).await
    }

    /// The ask door, which is the workspace with the question already in the
    /// box and Ask one press away — the link a question is carried by.
    ///
    /// Filled and still. A filled box otherwise carries a `load` trigger that
    /// searches it on arrival, and through this door that meant the question
    /// was searched for rather than asked — the one thing the door is named
    /// after was the one thing it did not do.
    #[tokio::test]
    async fn the_ask_door_fills_the_one_box_and_runs_nothing_on_arrival() {
        let core = crate::core::test_support::test_core().await;
        hold_something(&core).await;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui/ask?q=why+did+the+reindex+fail", &cookie).await;
        assert!(
            html.contains("why did the reindex fail"),
            "the box carries the question: {html}"
        );
        assert!(
            !trigger_of(&html).contains("load"),
            "and nothing is searched for on the way in: {html}"
        );
        // Still is not blank. Nothing is coming through this door, so the page
        // renders the state it is actually in — the idle column, with the base
        // saying what it holds — rather than the columns of nothing that state
        // was written to remove.
        assert!(
            html.contains(r#"id="idle-foot""#),
            "the idle column says what the base holds: {html}"
        );
        // Every id the stream driver writes into has to survive the move; the
        // browser suite targets each of these by name.
        for id in [
            "ask-live",
            "ask-result",
            "ask-status",
            "ask-stop",
            "ask-progress",
        ] {
            assert!(html.contains(id), "the driver's target {id} is on the page");
        }
    }

    /// On a wide window the workspace is pinned to the viewport and the body
    /// does not scroll; the height is handed down the chain to the artifact
    /// card, which scrolls. The answer is not on that chain — `#ask-live` and
    /// `#ask-result` are siblings of `#pane-content` — so a long answer grew
    /// the column past the window and the body's `overflow: hidden` clipped
    /// it, with nothing anywhere to scroll. Each has to be its own scroller
    /// inside the pinned block, and the live one has to follow its own tail.
    #[test]
    fn a_long_answer_scrolls_inside_the_pinned_workspace() {
        let css = crate::web::assets::Assets::get("app.css").expect("app.css is embedded");
        let css = String::from_utf8(css.data.into_owned()).unwrap();
        let pinned = css
            .split_once(".regions-rail-focus-source { grid-template-rows:")
            .expect("the pinned block")
            .1;
        for target in ["#ask-live", "#ask-result"] {
            let rule = pinned
                .split_once(&format!(".pane > {target}"))
                .unwrap_or_else(|| panic!("{target} is not a scroller in the pinned block"))
                .1;
            let rule = &rule[..rule.find('}').unwrap()];
            assert!(rule.contains("overflow-y: auto"), "{target}: {rule}");
            assert!(rule.contains("min-height: 0"), "{target}: {rule}");
        }

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let token = js
            .split_once("addEventListener('token'")
            .expect("the driver handles token")
            .1;
        let token = &token[..token.find("addEventListener").unwrap()];
        assert!(
            token.contains("live.scrollTop = live.scrollHeight"),
            "the live answer does not follow its tail: {token}"
        );
    }

    /// No ask model, no ask door: not a greyed-out button over a page that
    /// explains itself, and not a route that 500s. The door is simply absent.
    #[tokio::test]
    async fn the_ask_door_is_absent_without_a_model() {
        let core = crate::core::test_support::test_core_without_ask().await;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(
            !html.contains(r#"data-verb="ask""#),
            "no button where there is no model: {html}"
        );

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/ask")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "and the door is not there"
        );
    }

    /// The capture door, which is the workspace with the box already filled.
    /// The extension posts here and so does *keep this answer*, and neither
    /// knows anything about the page having folded into one.
    #[tokio::test]
    async fn the_capture_door_fills_the_one_box_and_keeps_its_provenance() {
        let (app, cookie, _core, _html, id) = ask_recorded().await;

        let html = get(&app, &format!("/ui/capture?from_ask={id}"), &cookie).await;
        // The box holds a whole model answer. Searching for it on arrival was
        // an embedding call, an activation bump on whatever it retrieved and,
        // where searches are recorded, a Judge-queue row — for a paragraph the
        // operator is about to store, not look for.
        assert!(
            !trigger_of(&html).contains("load"),
            "and the answer in the box is not searched for: {html}"
        );
        assert!(html.contains(r#"id="box-form""#), "and it is the workspace");
        assert!(
            html.contains(r#"id="idle-foot""#),
            "which paints its idle column, not two empty ones: {html}"
        );
        assert!(
            html.contains(&format!(r#"name="from_ask" value="{id}""#)),
            "the ask rides the form as provenance: {html}"
        );
        // In one removable block with the line that explains it: the claim is
        // about this text, and app.js takes the pair away when the capture
        // lands, so the next thing pasted into the same box is not stored as
        // the same model answer.
        assert!(
            html.contains(r#"<div id="kept-from">"#),
            "and it is retirable in one piece: {html}"
        );
        assert!(
            html.contains("Kept from"),
            "the question it answered is named"
        );

        // The plain door is the same page with an empty box.
        let plain = get(&app, "/ui/capture", &cookie).await;
        assert!(plain.contains(r#"id="box-form""#), "still the workspace");
        assert!(
            !plain.contains("Kept from"),
            "an ordinary visit claims no provenance: {plain}"
        );
    }

    /// The file control offers what the installation can actually read. Off,
    /// it offers text only rather than a picker that fails.
    #[tokio::test]
    async fn the_file_control_offers_images_only_when_vision_is_configured() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(html.contains("image/*"), "the picker accepts images");

        let (app, cookie) =
            app_for(crate::core::test_support::test_core_without_vision().await).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(!html.contains("image/*"));
        assert!(html.contains(r#"accept=".txt,text/plain,.pdf,application/pdf""#));
    }

    /// The one page. Capture, search and ask were three of them, and moving
    /// between them meant retyping or carrying a prefill: the same words are a
    /// query on one, a question on the second and a document on the third, and
    /// the operator navigated to say which.
    #[tokio::test]
    async fn the_workspace_is_one_page_carrying_the_box_and_the_three_regions() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;

        for uri in ["/ui", "/ui/search"] {
            let html = get(&app, uri, &cookie).await;
            assert!(
                html.contains("regions-rail-focus-source"),
                "{uri}: the grid"
            );
            assert!(html.contains("name=\"q\""), "{uri}: the box");
            assert!(
                html.contains("hx-get=\"/ui/search/results\""),
                "{uri}: typing still asks the same endpoint"
            );
        }

        // A deep link restores the box and asks for its results without a
        // keystroke, because no keystroke is coming.
        let html = get(&app, "/ui/search?q=volume+move", &cookie).await;
        assert!(html.contains("volume move"), "the box comes back filled");
        assert!(
            trigger_of(&html).contains("load"),
            "and the results are fetched on load: {html}"
        );
    }

    /// Typing is the third verb, and `input` is what it fires. It was `keyup`,
    /// which a paste made with the mouse — the context menu, the middle button,
    /// the phone's own bubble — does not fire at all; and the box's `change` is
    /// scoped away to the chip row on purpose, because it also fires on the
    /// blur of the very click that opens a result. So nothing was left to
    /// notice a pasted paragraph: the box grew, the verbs lit up, and the rail
    /// under it did not move. On a page whose placeholder ends "paste anything
    /// worth keeping", that is the hole this test exists to keep shut.
    ///
    /// The filter is the other half. `input` fires for every keystroke inside
    /// an IME composition, so without it a Japanese or Chinese query embedded
    /// each half-formed romaji fragment on the way to the word.
    #[tokio::test]
    async fn a_paste_searches_because_typing_is_an_input_event() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core.clone()).await;
        let html = get(&app, "/ui/search", &cookie).await;
        let trigger = trigger_of(&html);

        assert!(
            trigger.contains("input[!event.isComposing] changed delay:120ms from:textarea[name=q]"),
            "the box does not search on `input`: {trigger}"
        );
        assert!(
            !trigger.contains("keyup"),
            "`keyup` is back, and a mouse paste searches for nothing: {trigger}"
        );
    }

    /// What the box says it is, to something that is not looking at it.
    ///
    /// A placeholder is not a name — a screen reader may ignore it as one, and
    /// it is gone by the second keystroke — so the one control the page is
    /// built around announced itself as an unlabelled text field. And the rail
    /// is replaced under a box that keeps its focus, so twelve results, or
    /// none, arrived in silence; `#rail-head` is the only part of it that
    /// survives every swap, and what it already holds is the announcement.
    #[tokio::test]
    async fn the_box_has_a_name_and_the_rail_says_what_it_found() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core.clone()).await;
        let html = get(&app, "/ui/search", &cookie).await;

        assert!(
            html.contains(r#"aria-label="Search, ask, or capture""#),
            "the box has no accessible name: {html}"
        );
        // The long sentence names all three verbs and the phone has no room
        // for it: one row at that width clips it, and the hint that would have
        // carried the rest is `display: none` there.
        assert!(
            html.contains("data-placeholder-narrow="),
            "nothing says what the box is for on a phone: {html}"
        );
        let head = html
            .split(r#"id="rail-head""#)
            .nth(1)
            .expect("the rail's heading");
        assert!(
            head.split('>')
                .next()
                .unwrap()
                .contains(r#"aria-live="polite""#),
            "a search announces nothing: {head}"
        );
    }

    /// The idle state is designed, not left over. With an empty box the rail
    /// introduces the base instead of standing empty, and the file picker is
    /// on the page from first paint: it once lived inside the staged box,
    /// which is hidden until a file is staged — a state that rendered
    /// correctly and a control nobody could reach.
    /// The one contract behind "a reminder appears while you watch": the
    /// column is hidden and revealed rather than removed, and the band is
    /// re-fetchable in place.
    ///
    /// Asserted over the sources because the behaviour is a browser's. The
    /// band used to be *removed* from the document on the first keystroke —
    /// after which a capture emptied the box, the idle state was correct
    /// again, and there was no `#due` left for anything to swap into. A
    /// reminder armed a second ago was invisible until a reload, and no
    /// server-side test could see it, because on the server nothing was wrong.
    #[test]
    fn the_idle_column_is_hidden_and_the_band_re_fetched_never_removed() {
        let js = include_str!("../../assets/app.js");
        assert!(
            js.contains("function showIdle(force)"),
            "nothing brings the column back"
        );
        assert!(
            js.contains("htmx.trigger(due, 'refresh')"),
            "the column comes back holding what was due a minute ago"
        );
        assert!(
            !js.contains("if (due) due.remove()"),
            "removing the band is the bug this pair of functions replaced"
        );
        let due = include_str!("templates/_due.html");
        assert!(
            due.contains(r#"hx-trigger="refresh"#),
            "and the band has nothing to answer that event with"
        );
        let ws = include_str!("templates/workspace.html");
        assert!(
            ws.contains(r#"<div id="idle""#) && ws.contains(r#"<div id="due""#),
            "the band lives inside the column, or hiding one does not hide the other"
        );
    }

    #[tokio::test]
    async fn a_small_paste_echoes_the_synthesis_it_will_get() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        let html = get(
            &app,
            "/ui/search/results?q=remind+me+tomorrow+to+send+the+invoice",
            &cookie,
        )
        .await;
        assert!(html.contains(r#"id="intent-echo""#), "{html}");
        assert!(
            html.contains("will be synthesized"),
            "it says its fate: {html}"
        );
        assert!(
            html.contains("structured artifacts"),
            "and what that means: {html}"
        );
    }

    #[tokio::test]
    async fn a_large_paste_echoes_its_verbatim_windows() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        let big = "filler+words+".repeat(400);
        let html = get(&app, &format!("/ui/search/results?q={big}"), &cookie).await;
        // The slot is swapped whatever the answer, so an echo for text that is
        // no longer in the box cannot outlive it.
        assert!(html.contains(r#"id="intent-echo""#), "{html}");
        assert!(html.contains("large paste"), "{html}");
        assert!(html.contains("verbatim"), "{html}");
    }

    #[tokio::test]
    async fn the_echo_counts_the_windows_the_splitter_will_actually_make() {
        // `tokens.div_ceil(budget)` is arithmetic the splitter does not
        // perform: it will not cut inside a paragraph unless forced, so
        // paragraphs that each sit under the budget are each their own window
        // and the promise was short by up to half.
        let core = crate::core::test_support::test_core().await;
        let lang = crate::infer::lang::Lang::En;
        let budget = crate::jobs::synthesize::segment_budget(&core, lang).max(1);
        let para = "filler words ".repeat(budget * 6 / 10);
        // Three, not ten: the property holds from three paragraphs up — three
        // windows against the arithmetic's two — and ten put the fixture over
        // `EXACT_SPLIT_BYTES`, where the echo estimates on purpose.
        let text = std::iter::repeat_n(para.trim(), 3)
            .collect::<Vec<_>>()
            .join("\n\n");
        let echo = fate_echo(&core, &text, lang);
        assert!(
            text.len() <= EXACT_SPLIT_BYTES,
            "the fixture has to stay under the bound the exact split is done below: {}",
            text.len()
        );
        assert_eq!(echo.kind, "large paste");
        let windows = crate::infer::split::split_into_segments(&text, &core.counter, budget).len();
        assert!(
            echo.detail.contains(&format!("in {windows} windows")),
            "the echo promises what the splitter does: {} against {windows}",
            echo.detail
        );
        assert!(
            windows > core.counter.count(&text).div_ceil(budget),
            "the fixture must be one the old arithmetic under-reported"
        );
    }

    /// And the bound the exact answer stops at. `search_results` asks this on
    /// every debounced keystroke, so a whole `MarkdownSplitter` pass over a
    /// pasted article is work the hottest route in the app must not repeat per
    /// character. Over the bound the line says "at least", which is what the
    /// arithmetic actually knows.
    #[tokio::test]
    async fn a_paste_too_big_to_split_twice_a_second_is_estimated_and_says_so() {
        let core = crate::core::test_support::test_core().await;
        let lang = crate::infer::lang::Lang::En;
        let budget = crate::jobs::synthesize::segment_budget(&core, lang).max(1);
        let para = "filler words ".repeat(budget * 6 / 10);
        let text = std::iter::repeat_n(para.trim(), 40)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(text.len() > EXACT_SPLIT_BYTES);
        let echo = fate_echo(&core, &text, lang);
        assert_eq!(echo.kind, "large paste");
        let floor = core.counter.count(&text).div_ceil(budget);
        assert!(
            echo.detail.contains(&format!("at least {floor} windows")),
            "an estimate is offered as one: {}",
            echo.detail
        );
    }

    #[tokio::test]
    async fn the_examples_under_the_box_are_in_the_readers_language() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .header("accept-language", "de-DE,de;q=0.9,en;q=0.8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("erinnere mich morgen"),
            "a German reader is shown German: {html}"
        );
        assert!(html.contains("chip-example"), "and it is pressable");
    }

    #[tokio::test]
    async fn the_idle_page_introduces_the_base_and_the_picker_is_reachable() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core.clone()).await;

        // Empty base: the one instruction that matters, said once — under the
        // box, where the eye already is. The foot below it says nothing at all.
        let html = get(&app, "/ui", &cookie).await;
        assert!(
            html.contains("Paste anything worth keeping"),
            "the empty base says what to do"
        );
        assert!(
            !html.contains("Nothing here yet"),
            "and does not say it a second time under the column"
        );
        assert!(
            html.contains(r#"<label class="btn btn-ghost" id="drop""#),
            "the picker is in the verb row, not inside the hidden staged box"
        );
        let staged = html.split(r#"id="staged""#).nth(1).unwrap();
        assert!(
            !staged[..staged.find("</div>").unwrap()].contains("type=\"file\""),
            "and the staged box holds only the file, never the way to pick one"
        );

        // The column, and what it is not. Four muted prose lines, a chip row
        // marooned at the far end of the verb row, and the same five captures
        // rendered twice in two shapes is what this page used to be.
        let html = get(&app, "/ui", &cookie).await;
        assert!(
            html.contains(r#"id="idle""#),
            "the column exists as one element"
        );
        // Asserted against the template source, not a render: facets come from
        // the vector store and the fixture seeds none, so a render of this page
        // carries no chip row to be wrong about. What is being asserted is the
        // gate, and the gate is in the markup.
        let tpl = include_str!("templates/workspace.html");
        assert!(
            tpl.contains(
                r#"<span id="kind-row" class="kind-row"{% if idle_state %} hidden{% endif %}>"#
            ),
            "chips qualify a search, and an idle page has none"
        );
        assert!(
            !html.contains("or drop one anywhere on the page."),
            "the attach prose is on the button's title: {html}"
        );
        // One of each, and both under the box.
        //
        // `_box_hint.html` was included twice — once in the search form and
        // once inside `#idle` — so an idle page printed the guidance sentence
        // and both example chips twice and carried duplicate `id="box-hint"`
        // and `id="intent-echo"` elements. An out-of-band swap resolves the
        // first match only, which made every copy below it dead markup, and
        // the textarea's `aria-describedby` names the id once.
        assert_eq!(
            html.matches(r#"id="box-hint""#).count(),
            1,
            "the hint under the box is in the document once: {html}"
        );
        assert_eq!(
            html.matches(r#"id="intent-echo""#).count(),
            1,
            "and so is the slot the echo swaps into: {html}"
        );
        // Under the box, not inside the column app.js hides on the first
        // keystroke: the echo says what is being typed *now*.
        let tpl = include_str!("templates/workspace.html");
        let hint = tpl
            .find(r#"{% include "_box_hint.html" %}"#)
            .expect("the include");
        assert!(hint < tpl.find(r#"<div id="idle""#).expect("the column"));

        // A base with something in it introduces itself.
        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let html = get(&app, "/ui", &cookie).await;
        assert!(
            html.contains("artifact"),
            "the closing line counts what is held"
        );
        assert!(html.contains("last kept"), "and names what last went in");
        assert!(
            html.contains("LevelDB tombstones"),
            "in the operator's words"
        );
        // One list, not two. The rail used to render these same rows beside a
        // middle column already rendering them, which is what made the page
        // read as clutter.
        assert!(!html.contains("Last captured"), "and only once: {html}");

        // Clearing the box returns to idle, not to "No matches." — which
        // would be a claim about a base nobody searched.
        let frag = get(&app, "/ui/search/results?q=", &cookie).await;
        assert!(
            frag.contains(r#"id="idle-foot""#),
            "an empty query is idle again"
        );
        assert!(!frag.contains("No matches"), "not a verdict on the base");
    }

    /// The list is retired; the sitting behind it is not. Its own comment gave
    /// the reason it existed — "the pages had nothing between them, so a hit
    /// opened on search and wanted again on ask meant searching for it twice"
    /// — and this is the commit that removes the pages.
    #[tokio::test]
    async fn the_read_just_now_list_is_gone_but_the_sitting_is_not() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(!html.contains("Read just now"), "the list is retired");

        // The mechanism it read from stays: Ask's carried citations ride on
        // it and `[sitting] prime` reads it. That half is covered
        // behaviourally by `marking_a_carrier_marks_the_answer_right_and_updates_the_bar_out_of_band`
        // — an assertion on `citations[..].carried`, which is false unless
        // `sittings.touched()` still runs on every artifact open.
    }

    /// The three surfaces that are maintenance rather than searching. Capture
    /// is about to stop being a page at all, and none of these belonged on it
    /// even while it was one: a merge decision, a hole in the base and a list
    /// of what was just stored are all work *on* the base rather than work
    /// with it.
    #[tokio::test]
    async fn the_three_maintenance_surfaces_are_on_insights_and_not_on_capture() {
        let (app, cookie) = app_with_a_waiting_pair().await;

        let insights = get(&app, "/ui/insights", &cookie).await;
        assert!(insights.contains("Needs you"), "pairs are on Insights");
        assert!(
            insights.contains("Recent"),
            "the capture queue is on Insights"
        );
        assert!(
            insights.contains("/ui/queue"),
            "and it loads the same fragment it always did"
        );

        let capture = get(&app, "/ui/capture", &cookie).await;
        assert!(
            !capture.contains("Needs you"),
            "pairs left the capture page"
        );
        assert!(!capture.contains("/ui/queue"), "so did the queue");
    }

    /// Housekeeping is Insights now. The old door stays a door: it is in
    /// bookmarks, in the quiet link at the bottom of the page, and in at least
    /// one runbook — a 404 there is a broken promise, not a tidy-up.
    /// Two words for one thing, switched between without a rule. This picks
    /// the one already doing most of the work; the URLs keep theirs, because a
    /// path is not addressed to the reader.
    #[tokio::test]
    async fn a_source_is_called_a_source_everywhere_a_person_reads_it() {
        let core = crate::core::test_support::test_core().await;
        // `ingest_capture` answers with an `IngestOutcome`; the corpus id is
        // the field on it, not the value itself.
        let id = core
            .ingest_capture(crate::core::ingest::Capture::new(
                "LevelDB tombstones survive compaction longer than the manual admits.",
                "ui",
            ))
            .await
            .unwrap()
            .id;
        let (app, cookie) = app_for(core).await;

        let corpus = get(&app, &format!("/ui/corpora/{id}"), &cookie).await;
        assert!(
            !corpus.contains("Corpus — engram"),
            "the page a person reads does not say corpus"
        );
        assert!(corpus.contains("Source — engram"), "it says source");
        assert!(
            !corpus.contains("Raw corpus"),
            "nor in the card over the text itself"
        );
        assert!(
            corpus.contains("/ui/corpora/"),
            "and the URL is untouched, because a path is not addressed to anyone"
        );

        let settings = get(&app, "/ui/settings", &cookie).await;
        assert!(!settings.contains(">Mint<"), "Mint is a word about coins");
        assert!(
            settings.contains(">Create<"),
            "the button says what it does"
        );

        let ext = get(&app, "/extension/install", &cookie).await;
        assert!(
            !ext.contains("Housekeeping → API tokens"),
            "Housekeeping is retired and tokens were never there anyway"
        );
        assert!(
            ext.contains("Settings → API tokens"),
            "named where they are"
        );
    }

    /// The page ships the install nudge hidden, on every page, and app.js
    /// decides — so a browser without JS, or one that already installed the
    /// app, never sees a banner offering it.
    #[tokio::test]
    async fn the_install_nudge_ships_hidden_and_app_js_gates_it() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        for uri in ["/ui", "/ui/settings"] {
            let html = get(&app, uri, &cookie).await;
            let nudge = html
                .split_once(r#"class="installnudge""#)
                .unwrap_or_else(|| panic!("no install nudge on {uri}: {html}"))
                .1;
            let tag = &nudge[..nudge.find('>').unwrap()];
            assert!(
                tag.contains("hidden"),
                "the nudge is not hidden on {uri}: {tag}"
            );
            assert!(nudge.contains("data-install-how"), "{uri}");
            assert!(
                nudge.contains("Add to Home Screen"),
                "the Safari route: {uri}"
            );
            assert!(nudge.contains("data-install"), "{uri}");
            assert!(nudge.contains("data-dismiss-install"), "{uri}");
        }

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        // The constants sit just above the function; the slice takes both.
        let body = js
            .split_once("var INSTALL_KEY")
            .expect("app.js has no installNudge()")
            .1;
        let body = &body[..body.find("\n  }\n").unwrap()];
        // Not in an installed window, not on a desktop, not more than once a
        // week, and the prompt where the browser offers one.
        for guard in [
            "display-mode: standalone",
            "navigator.standalone",
            "pointer: coarse",
            "max-width: 40rem",
            "7 * 24 * 60 * 60 * 1000",
            "engram.install-nudged",
        ] {
            assert!(
                body.contains(guard),
                "installNudge() lost `{guard}`: {body}"
            );
        }
        assert!(
            js.contains("    installNudge();\n"),
            "installNudge() is not called on load"
        );
        // Chrome fires the event once, early; a listener added at load time
        // may miss it. It is captured at script scope.
        let before = js.split_once("function installNudge() {").unwrap().0;
        assert!(
            before.contains("addEventListener('beforeinstallprompt'"),
            "beforeinstallprompt is not captured before load"
        );
    }

    #[tokio::test]
    async fn housekeeping_moved_to_insights_and_the_old_door_still_opens() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;

        let html = get(&app, "/ui/insights", &cookie).await;
        assert!(
            html.contains("What the machine is doing"),
            "the maintenance section is there, under the name that says what \
             it holds rather than what a person might do about it"
        );

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/ops")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            res.headers().get("location").unwrap(),
            "/ui/insights",
            "the old door points at the new one"
        );
    }

    #[tokio::test]
    async fn a_link_derived_pair_never_claims_a_similarity_once_the_judge_has_settled_it() {
        // The link judge files these with `detail = "link"` and a score of 0.0,
        // because no cosine was ever measured. But `detail` is where the dedupe
        // judge then writes its own prose — `set_pair_state` and
        // `set_pair_superseded` both overwrite it — so provenance read out of
        // that field survives only while the pair is pending. Once it settles,
        // the page would go back to rendering the placeholder score, and
        // "0% alike" reads as a measurement meaning "nothing alike".
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "the first one".into(),
                        title: Some("first".into()),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "the second one".into(),
                        title: Some("second".into()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .record_pair_with_detail(&made[0].id, &made[1].id, 0.0, "link")
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM artifact_pairs")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        // The dedupe judge answers, and writes its line over the marker.
        core.store
            .set_pair_state(
                id,
                crate::store::pairs::PairState::Contradiction,
                Some("one says the opposite of the other"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();

        let (app, cookie) = app_for(core).await;
        let html = flat(&get(&app, "/ui/capture", &cookie).await);

        assert!(
            !html.contains("0% alike"),
            "a pair no cosine was ever measured for reports a measured similarity"
        );
    }

    #[tokio::test]
    async fn the_capture_page_offers_images_only_when_vision_is_configured() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        let html = get(&app, "/ui/capture", &cookie).await;
        assert!(html.contains("image/*"), "picker accepts images");

        let (app, cookie) =
            app_for(crate::core::test_support::test_core_without_vision().await).await;
        let html = get(&app, "/ui/capture", &cookie).await;
        assert!(!html.contains("image/*"));
        assert!(html.contains("accept=\".txt,text/plain,.pdf,application/pdf\""));
    }

    #[tokio::test]
    async fn the_capture_page_takes_a_pdf_whether_or_not_vision_is_configured() {
        for core in [
            crate::core::test_support::test_core().await,
            crate::core::test_support::test_core_without_vision().await,
        ] {
            let (app, cookie) = app_for(core).await;
            let html = get(&app, "/ui/capture", &cookie).await;
            assert!(html.contains("application/pdf"), "picker accepts PDFs");
        }
    }

    /// Nothing knows what a PDF says until the extraction lands, so the row has
    /// no opening words to be called by — and an empty label is an anchor with
    /// nothing to read and nothing to click.
    #[tokio::test]
    async fn a_pdf_waiting_to_be_extracted_is_called_a_document_in_the_queue() {
        let core = crate::core::test_support::test_core().await;
        core.ingest_pdf(crate::core::ingest::PdfCapture {
            bytes: include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec(),
            filename: Some("plan.pdf".into()),
            title_hint: None,
            note: None,
            lang: crate::infer::lang::Lang::default(),
        })
        .await
        .unwrap();

        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui/queue", &cookie).await;
        assert!(
            html.contains(">document</span>"),
            "the row has no title to click: {html}"
        );
    }



    /// The same base, plus one established situation matching the bundle the
    /// tests post — so the reason line actually renders.
    async fn app_with_a_learned_situation() -> (axum::Router, String, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "when the recycling centre is open".into(),
                    title: Some("recycling centre".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id;
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: vec![1.0; 8],
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: aid.clone(),
                    corpus_id: src.id.clone(),
                    text: "when the recycling centre is open".into(),
                    title: Some("recycling centre".into()),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();

        // The centroid is this very situation, so the offer lands on `Pattern`.
        let at = crate::store::now();
        let bundle = crate::core::context::Bundle {
            tz: Some("Europe/Berlin".into()),
            ..Default::default()
        };
        let v = crate::core::context::encode(at, &bundle, &core.recommend.weights);
        core.store
            .replace_context_clusters(
                &aid,
                &[crate::store::context::StoredCluster {
                    scope: Some("user-1".into()),
                    artifact_id: aid.clone(),
                    slot: 0,
                    centroid: v.clone(),
                    weight: 6.0,
                    events: 6,
                    last_at: at,
                    encoder_version: crate::core::context::encoder_version(&core.recommend.weights),
                    representative: serde_json::json!({ "at": at, "bundle": bundle }).to_string(),
                }],
            )
            .await
            .unwrap();
        core.vectors
            .set_context_vectors(&aid, vec![v])
            .await
            .unwrap();

        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        (app, cookie, aid)
    }

    #[tokio::test]
    async fn a_page_view_is_recorded_even_when_nothing_is_offered() {
        // The endpoint has two jobs and does the first unconditionally. A base
        // that has learned nothing yet is exactly the base that most needs its
        // situations written down.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = app
            .clone()
            .oneshot(form(
                "/ui/context",
                &cookie,
                "bundle=%7B%22tz%22%3A%22Europe%2FBerlin%22%7D",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        background.wait_idle().await;

        let rows = store.context_events_since(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tz.as_deref(), Some("Europe/Berlin"));
        assert!(rows[0].local_hour.is_some(), "denormalised for the sweep");
        assert!(rows[0].weekday.is_some());
        assert_eq!(rows[0].scope.as_deref(), Some("user-1"));
        // Stored whole, including what the encoder does not read today.
        assert!(rows[0].bundle.contains("Europe/Berlin"));
    }

    #[tokio::test]
    async fn a_bundle_the_browser_could_not_build_does_not_break_the_page() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = app
            .clone()
            .oneshot(form("/ui/context", &cookie, "bundle=%7B%7Bnope"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "an empty bundle still works");
        background.wait_idle().await;
        assert_eq!(store.context_events_since(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_area_is_not_rendered_when_the_faculty_is_off() {
        // One gate, in one place: no placeholder, no request, nothing recorded.
        let core = crate::core::test_support::test_core().await;
        let store = core.store.clone();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let page = get(&app, "/ui/search", &cookie).await;
        assert!(!page.contains("/ui/context"), "no placeholder");

        let res = app
            .clone()
            .oneshot(form("/ui/context", &cookie, "bundle=%7B%7D"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        background.wait_idle().await;
        assert!(store.context_events_since(0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_placeholder_reserves_its_height_so_the_page_does_not_jump() {
        let (app, cookie, _store, _aid) = app_recommending().await;
        let page = get(&app, "/ui/search", &cookie).await;
        assert!(page.contains(r#"id="context-offer""#));
        assert!(page.contains(r#"hx-post="/ui/context""#));
        assert!(page.contains("engramContext()"));
        // The class the reserved height hangs off.
        assert!(page.contains(r#"class="offer""#));
    }

    /// A phone hid the whole card and kept the instrument that counts it.
    ///
    /// The offer sits inside `.region-bar`, which under 40rem is the fixed bar
    /// at the thumb, and the rule that sheds the bar's page furniture — the
    /// hint, the chips, the key teaching — took the offer with it. Hidden did
    /// not mean absent: `hx-trigger="load"` still fetched, the swap still
    /// fired, and `confirmOffer` still posted `/ui/context/seen`. Every phone
    /// view wrote an impression for a card no thumb could reach, into the
    /// denominator of the one hit rate the rung weights are to be fitted
    /// against. If it is not shown it must not be counted, and the card is
    /// rendered only while the box is empty — which on a phone is the one
    /// moment the screen above the bar is empty too.
    #[test]
    fn the_phone_does_not_hide_a_card_it_still_counts_as_seen() {
        let phone = include_str!("../../assets/css/50-phone.css");
        assert!(
            !phone.contains(".regions-rail-focus-source .region-bar .offer"),
            "the phone bar hides the offer while app.js still confirms it seen"
        );
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("/ui/context/seen"),
            "the confirmation this rule must not outrun is gone"
        );
    }

    #[tokio::test]
    async fn the_offer_is_absent_from_a_page_that_already_carries_a_query() {
        // The area is for the state "no intent expressed yet". A deep link, a
        // reload or a back-navigation renders the box with its query restored
        // and results below it — and app.js only removes the area on a
        // *keystroke*, which never comes on any of those. Left ungated, the
        // offer sat beside real results and wrote a `recommended_shown` row per
        // results page view, inflating the denominator of the one hit rate this
        // feature is measured by.
        let (app, cookie, _store, _aid) = app_recommending().await;
        let page = get(&app, "/ui/search?q=recycling", &cookie).await;
        assert!(
            !page.contains(r#"id="context-offer""#),
            "an offer beside results: {page}"
        );
        // And it is back on the next fresh page view: dismissal is per view,
        // not a state anything remembers.
        let page = get(&app, "/ui/search", &cookie).await;
        assert!(page.contains(r#"id="context-offer""#));
    }

    #[tokio::test]
    async fn a_confirmation_a_page_made_up_records_nothing() {
        // The impression now comes from the browser, which means it comes from
        // whatever anyone chooses to post. Both halves go into `offer_rates`'
        // `GROUP BY rung` — a made-up rung would appear on Ops as a fifth rung
        // of a four-rung ladder, and a made-up artifact would put a shown
        // against something that was never offered and cannot ever be clicked,
        // which is a zero in the denominator of the hit rate for ever.
        let (app, cookie, store, aid) = app_recommending().await;

        for body in [
            format!("artifact_id={aid}&rung=excellent"),
            "artifact_id=no-such-artifact&rung=random".to_string(),
        ] {
            let res = app
                .clone()
                .oneshot(form("/ui/context/seen", &cookie, &body))
                .await
                .unwrap();
            // Nothing is waiting on the answer, so neither is an error — but
            // neither is a row.
            assert_eq!(res.status(), StatusCode::NO_CONTENT);
        }
        drain().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(
            rows.is_empty(),
            "a page wrote its own row into the hit rate: {rows:?}"
        );
    }

    #[tokio::test]
    async fn the_reason_line_is_markup_a_browser_will_not_rearrange() {
        // `details` and `pre` are flow content and a `p` may hold only phrasing
        // content, so a `p` here is closed by the parser before the `details`
        // and leaves a stray empty paragraph behind — a DOM the stylesheet is
        // not written against, with the Details control on its own line.
        let (app, cookie, _aid) = app_with_a_learned_situation().await;
        let body = crate::web::test_support::body_of(
            app.clone()
                .oneshot(form(
                    "/ui/context",
                    &cookie,
                    "bundle=%7B%22tz%22%3A%22Europe%2FBerlin%22%7D",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            body.contains("Offered because"),
            "no reason line at all: {body}"
        );
        assert!(body.contains(r#"<div class="muted offer-why">"#), "{body}");
        assert!(
            !body.contains("<p class=\"muted offer-why\">"),
            "the reason line must not be a paragraph: {body}"
        );
    }

    #[tokio::test]
    async fn what_was_offered_is_written_down_with_its_rung() {
        // Shown against clicked, broken down by rung, is a hit rate. It is the
        // only number that can later settle whether the weights are right, and
        // a recommender with no visible hit rate becomes `[sitting] prime`:
        // a default nobody ever measured.
        let (app, cookie, store, aid) = app_recommending().await;

        let res = app
            .clone()
            .oneshot(form("/ui/context", &cookie, "bundle=%7B%7D"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(body.contains(&aid), "{body}");
        // Nothing about the situation produced it, so nothing is claimed: no
        // rung name, no blocks, no reason line at all. A card with a sentence
        // under it would be the area borrowing authority it does not have.
        assert!(
            !body.contains("offer-why"),
            "the card explains nothing: {body}"
        );
        assert!(!body.contains("Offered because"), "{body}");
        // The fragment carries what the confirmation posts back, so the shown
        // and the open agree about what was offered.
        assert!(
            body.contains(r#"data-rec-rung="random""#),
            "the browser cannot say what it was shown: {body}"
        );
        drain().await;

        // Computing an offer is not showing one. Until the browser says it
        // reached the screen, nothing is recorded: this fetch races the first
        // keystroke, and the answer that loses is dropped without ever being
        // seen. Counting those put a population that cannot click into the
        // denominator of the one number the weights would be fitted against.
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(
            !rows.iter().any(|r| r.kind == "recommended_shown"),
            "an offer nobody has confirmed seeing is already counted: {rows:?}"
        );

        app.clone()
            .oneshot(form(
                "/ui/context/seen",
                &cookie,
                &format!("artifact_id={aid}&rung=random"),
            ))
            .await
            .unwrap();
        drain().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let shown: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == "recommended_shown")
            .collect();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].detail.as_deref().unwrap().contains("random"),
            "{:?}",
            shown[0].detail
        );
    }

    #[tokio::test]
    async fn ops_shows_shown_against_clicked_by_rung() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "opening hours".into(),
                    title: Some("hours".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id;
        let now = crate::store::now();
        for _ in 0..4 {
            store
                .record_recommendation(
                    &aid,
                    "recommended_shown",
                    r#"{"rung":"pattern"}"#,
                    Some("me"),
                    now,
                )
                .await
                .unwrap();
        }
        store
            .record_recommendation(
                &aid,
                "recommended_open",
                r#"{"rung":"pattern"}"#,
                Some("me"),
                now,
            )
            .await
            .unwrap();
        store
            .record_recommendation(
                &aid,
                "recommended_shown",
                r#"{"rung":"forgotten"}"#,
                Some("me"),
                now,
            )
            .await
            .unwrap();

        let rates = store.offer_rates(0).await.unwrap();
        assert_eq!(rates.len(), 2, "one row per rung: {rates:?}");
        let pattern = rates.iter().find(|r| r.rung == "pattern").unwrap();
        assert_eq!(pattern.shown, 4);
        assert_eq!(pattern.opened, 1);
        let forgotten = rates.iter().find(|r| r.rung == "forgotten").unwrap();
        assert_eq!(forgotten.shown, 1);
        assert_eq!(forgotten.opened, 0, "nobody took that one");

        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let page = get(&app, "/ui/insights", &cookie).await;
        assert!(page.contains("What was offered"), "no heading");
        assert!(page.contains("pattern"), "no rung");
        assert!(page.contains("forgotten"));
    }

    #[tokio::test]
    async fn ops_says_nothing_about_offers_when_the_faculty_is_off() {
        // A heading over no rows is a claim that something is being measured
        // when nothing is.
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let page = get(&app, "/ui/insights", &cookie).await;
        assert!(!page.contains("What was offered"));
    }

    /// `get_body` under the argument order this module's fifty-two call sites
    /// were written against. The body was a second copy of it.
    async fn get(app: &axum::Router, uri: &str, cookie: &str) -> String {
        get_body(app, cookie, uri).await
    }

    /// A session on an installation that is recording searches, with `pending`
    /// of them captured and waiting for a verdict.
    async fn app_recording_searches(pending: usize) -> (axum::Router, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        for i in 0..pending {
            core.store
                .record_search(
                    crate::store::feedback::NewEvent {
                        fold_onto: None,
                        query: format!("search number {i}"),
                        door: crate::store::feedback::Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        // A pool, because a search that returned nothing is a
                        // hole rather than a card and the deck does not deal
                        // one — see `dealable!`. These stand for searches
                        // waiting to be judged, so they have something to
                        // judge.
                        candidates: vec![crate::store::feedback::NewCandidate {
                            artifact_id: format!("a{i}"),
                            score: 0.9,
                            similarity: Some(0.8),
                            shown: true,
                            ..Default::default()
                        }],
                        answered: false,
                        context: None,
                    },
                    // No folding: these stand for separate searches, not one
                    // being typed.
                    0,
                )
                .await
                .unwrap();
        }
        // Searches were recorded against something. Without a source the ask
        // door redirects, and a test walking the nav across every page would
        // be walking one that is not there.
        let handle = core.clone();
        let out = app_with_cookie(core).await;
        hold_something(&handle).await;
        out
    }

    /// One name for one destination. The nav entry, the tab and the empty
    /// state were renamed together and two in-page links were left behind, so
    /// "Judge some" on Insights and "Judge them" on Settings landed on a page
    /// that called itself something else in all three of the places a reader
    /// looks to confirm they arrived.
    #[tokio::test]
    async fn every_link_to_the_review_screen_calls_it_what_it_calls_itself() {
        let (app, cookie) = app_recording_searches(3).await;
        for page in ["/ui/insights", "/ui/settings"] {
            let html = flat(&get(&app, page, &cookie).await);
            for stale in ["Judge some", "Judge them", ">Judge<"] {
                assert!(
                    !html.contains(stale),
                    "{page} still calls the review screen \"{stale}\""
                );
            }
        }
    }

    #[tokio::test]
    async fn the_nav_offers_no_judge_entry_even_while_searches_are_recorded() {
        // The deck is gone: a verdict is given under the result it is about,
        // at the moment of the search, and the nav has nothing to advertise.
        let (app, cookie) = app_recording_searches(3).await;
        for page in ["/ui/search", "/ui/insights"] {
            let html = flat(&get(&app, page, &cookie).await);
            assert!(!html.contains("/ui/judge"), "{page} still links the deck");
        }
    }

    #[tokio::test]
    async fn the_search_page_offers_a_chip_for_what_the_collection_contains() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search", &cookie).await);

        // The fake synthesizer files everything under `reference`, so that is
        // the value the payload index holds. There is no tag row to render:
        // subject words have no vocabulary that can be closed, so nothing
        // offers a list of them.
        assert!(
            html.contains(r#"name="category" value="reference""#),
            "no category chip was rendered"
        );
        assert!(!html.contains(r#"name="tags""#), "the tag row is gone");
        assert!(
            html.contains(r#"name="category" value="" checked"#),
            "there must be a selected way back to every category"
        );
    }

    #[tokio::test]
    async fn a_deep_linked_filter_comes_back_selected() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search?q=alpha&category=note", &cookie).await);
        assert!(
            html.contains(r#"name="category" value="note" checked"#),
            "the chip a link arrived with must render selected"
        );
        assert!(
            !html.contains(r#"name="category" value="" checked"#),
            "picking a category must deselect `all`"
        );
    }

    #[tokio::test]
    async fn a_deep_linked_filter_the_facets_do_not_list_still_gets_a_chip() {
        // `recipe` is a category nothing carries, so the payload index never
        // reports it — but the rail is narrowed by it all the same. Without a
        // chip the page would read as unfiltered over a filtered rail, with no
        // way to click back out.
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search?q=alpha&category=recipe", &cookie).await);
        assert!(
            html.contains(r#"name="category" value="recipe" checked"#),
            "a filter outside the facet list must still render, and selected"
        );
        assert!(
            !html.contains(r#"name="category" value="" checked"#),
            "`all` must not look selected while a filter is applied"
        );
    }

    #[tokio::test]
    async fn the_search_page_renders_without_chips_when_there_is_nothing_to_narrow() {
        let (app, cookie) = app_with_session().await;
        let html = get(&app, "/ui/search", &cookie).await;
        assert!(html.contains(r#"name="q""#), "the search box must remain");
        assert!(
            !html.contains(r#"name="category""#),
            "an empty collection offers nothing to filter by"
        );
    }

    #[tokio::test]
    async fn a_chip_narrows_the_result_list() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let matching = get(
            &app,
            "/ui/search/results?q=alpha&category=reference",
            &cookie,
        )
        .await;
        let missing = get(
            &app,
            "/ui/search/results?q=alpha&category=procedure",
            &cookie,
        )
        .await;

        assert!(matching.contains("rail-item"), "the filter matched nothing");
        assert!(
            !missing.contains("rail-item"),
            "a category no artifact carries must return no results"
        );
    }

    #[test]
    fn a_loose_result_is_labelled_and_never_ranked() {
        // `#1` over something the search itself calls a poor match is the false
        // confidence this exists to remove: a rank is a claim about standing
        // among answers, and a barely-matching artifact is not one.
        let result = |weak: bool| crate::core::search::SearchResult {
            artifact_id: "a".into(),
            corpus_id: "s".into(),
            title: Some("t".into()),
            text: "body".into(),
            score: 0.5,
            weak,
            ..Default::default()
        };

        let loose = render_hit(0, result(true), &Default::default(), false);
        assert!(loose.weak);
        assert!(loose.rank.is_empty(), "a loose result was presented as #1");
        assert_eq!(
            render_hit(0, result(false), &Default::default(), false).rank,
            "#1"
        );

        let html = askama::Template::render(&ResultsTemplate {
            results: vec![loose],
            all_weak: true,
            ..Default::default()
        })
        .unwrap();
        assert!(html.contains("Nothing matches closely"), "{html}");
        assert!(!html.contains("#1"), "{html}");

        // A mixed list says how many of its rows are the loose ones, which is
        // the split "3 results" alone hid. An all-loose list does not: the flag
        // above the list already says it.
        let mixed = askama::Template::render(&ResultsTemplate {
            results: vec![
                render_hit(0, result(false), &Default::default(), false),
                render_hit(1, result(false), &Default::default(), false),
                render_hit(2, result(true), &Default::default(), false),
            ],
            ..Default::default()
        })
        .unwrap();
        assert!(mixed.contains("3 results · 1 loose"), "{mixed}");
    }

    /// A rail row reduced to what the continuation marker reads: which artifact
    /// it is, and whether it carries a rank to be named by.
    #[cfg(test)]
    fn row(id: &str, rank: &str) -> RenderedResult {
        RenderedResult {
            why_ranked: None,
            artifact_id: id.into(),
            title: String::new(),
            titled_by_corpus: false,
            html: String::new(),
            snippet: String::new(),
            category: None,
            tags: vec![],
            corpus_id: "s".into(),
            rank: rank.into(),
            weak: false,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: None,
            model_written: false,
            origin_count: 0,
            reason: None,
            continues: false,
            continues_in: String::new(),
        }
    }

    /// The two markers are exclusive on the page as well as in the struct: a
    /// row that names a rank must not also carry the offer to fetch, or the
    /// rail says both "it is over there" and "there is more" about one thing.
    #[test]
    fn the_rail_prints_one_continuation_marker_or_the_other() {
        let mut named = row("a", "#1");
        named.continues_in = "#2".into();
        let mut offered = row("b", "#2");
        offered.continues = true;

        let html = askama::Template::render(&ResultsTemplate {
            results: vec![named, offered],
            ..Default::default()
        })
        .unwrap();

        assert!(html.contains("continues in #2"), "{html}");
        assert!(html.contains("continues in the next passage"), "{html}");
        assert_eq!(
            html.matches("rail-continues").count(),
            2,
            "one marker per row, and no row wearing both: {html}"
        );
    }

    /// The silent case, and the one worth pinning: a row that continues nowhere
    /// must print nothing at all. An empty marker element is a line of space
    /// the reader reads as meaning something.
    #[test]
    fn a_row_that_continues_nowhere_prints_no_marker() {
        let html = askama::Template::render(&ResultsTemplate {
            results: vec![row("a", "#1")],
            ..Default::default()
        })
        .unwrap();

        assert!(!html.contains("rail-continues"), "{html}");
    }

    /// A search result and its next passage both placed. The rail says so by
    /// pointing at the rank the reader can already see, because the row is
    /// there and sending them to a second copy of it would be a claim that two
    /// things are on the page when one is.
    #[test]
    fn a_hit_whose_next_passage_also_placed_names_its_rank() {
        let mut rows = vec![row("a", "#1"), row("b", "#2")];
        let next = [("a".to_string(), "b".to_string())].into_iter().collect();
        super::mark_continuations(&mut rows, &next);

        assert_eq!(rows[0].continues_in, "#2");
        assert!(
            !rows[0].continues,
            "a hit whose continuation is on the page must not also offer to fetch it"
        );
    }

    /// The ordinary case: the document goes on, and what comes next did not
    /// place. Nothing about it is on the page, so the row says the one true
    /// thing — there is more — and the pane is where it gets read.
    #[test]
    fn a_hit_whose_next_passage_did_not_place_says_only_that_it_continues() {
        let mut rows = vec![row("a", "#1")];
        let next = [("a".to_string(), "elsewhere".to_string())]
            .into_iter()
            .collect();
        super::mark_continuations(&mut rows, &next);

        assert!(rows[0].continues);
        assert!(rows[0].continues_in.is_empty());
    }

    /// The last passage of a document, and the case the whole marker must not
    /// get wrong: an offer to read on where there is nothing to read.
    #[test]
    fn a_hit_at_the_end_of_its_document_is_marked_neither_way() {
        let mut rows = vec![row("a", "#1")];
        super::mark_continuations(&mut rows, &Default::default());

        assert!(!rows[0].continues);
        assert!(rows[0].continues_in.is_empty());
    }

    /// An associated row is not a ranked one and carries no rank. Naming it as
    /// a destination would print "setzt sich fort in" followed by nothing.
    #[test]
    fn a_continuation_into_a_row_with_no_rank_is_not_named_by_rank() {
        let mut rows = vec![row("a", "#1"), row("b", "")];
        let next = [("a".to_string(), "b".to_string())].into_iter().collect();
        super::mark_continuations(&mut rows, &next);

        assert!(rows[0].continues_in.is_empty());
        assert!(
            rows[0].continues,
            "the continuation exists and must still be offered"
        );
    }

    #[test]
    fn a_result_with_no_title_of_its_own_is_given_no_heading() {
        // "Untitled" is a heading that says nothing and looks like one that
        // says something. A verbatim passage has no title by design, and ten
        // rows of "Untitled" is what the rail then reads as.
        let hit = |title: Option<&str>, via: Option<&str>| crate::core::search::SearchResult {
            artifact_id: "a".into(),
            corpus_id: "s".into(),
            title: title.map(str::to_string),
            text: "body".into(),
            score: 0.5,
            via: via.map(str::to_string),
            ..Default::default()
        };
        let titles = super::ranked_titles(&[hit(None, None)]);
        assert!(
            titles.is_empty(),
            "an untitled hit must not lend its name to what it recalled: {titles:?}"
        );
        let r = render_hit(0, hit(None, None), &titles, false);
        assert!(r.title.is_empty(), "{:?}", r.title);
        let html = askama::Template::render(&ResultsTemplate {
            results: vec![r],
            associated: vec![render_hit(0, hit(None, Some("a")), &titles, false)],
            ..Default::default()
        })
        .unwrap();
        assert!(!html.contains("Untitled"), "{html}");
        assert!(!html.contains("rail-title"), "{html}");
    }

    fn rendered(via: Option<&str>, reason: Option<&str>) -> RenderedResult {
        RenderedResult {
            why_ranked: None,
            artifact_id: "a1".into(),
            title: "The one that was recalled".into(),
            titled_by_corpus: false,
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: String::new(),
            weak: false,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: via.map(str::to_string),
            reason: reason.map(str::to_string),
            model_written: false,
            origin_count: 0,
            continues: false,
            continues_in: String::new(),
        }
    }

    /// The rule is drawn once, before the first row past the cliff, and the
    /// rows past it are greyed but keep their ranks: they placed, they just
    /// stopped being answers.
    #[test]
    fn the_rail_draws_the_cliff_once_and_greys_what_lies_past_it() {
        let mut above = rendered(None, None);
        above.rank = "#1".into();
        let mut past = rendered(None, None);
        past.rank = "#3".into();
        past.past_cliff = true;
        let mut also_past = past.clone();
        also_past.rank = "#4".into();
        let body = ResultsTemplate {
            results: vec![above.clone(), above.clone(), past, also_past],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert_eq!(
            body.matches("Relevance falls off here").count(),
            1,
            "{body}"
        );
        assert_eq!(body.matches("rail-past").count(), 2, "{body}");
        assert!(body.contains("#3") && body.contains("#4"), "{body}");
        // The rule comes after the second row and before the third.
        let rule = body.find("Relevance falls off here").unwrap();
        assert!(body.find("#3").unwrap() > rule, "{body}");
        assert!(body.rfind("#1").unwrap() < rule, "{body}");

        // No cliff, no rule.
        let flat = ResultsTemplate {
            results: vec![above.clone(), above.clone(), above],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(!flat.contains("Relevance falls off here"), "{flat}");
        assert!(!flat.contains("rail-past"), "{flat}");
    }

    #[test]
    fn a_title_borrowed_from_the_note_is_marked_as_the_notes() {
        let mut own = rendered(None, None);
        own.title = "Sourdough".into();
        let mut borrowed = own.clone();
        borrowed.titled_by_corpus = true;
        let body = ResultsTemplate {
            results: vec![own, borrowed],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert_eq!(body.matches("Sourdough").count(), 2, "{body}");
        assert_eq!(body.matches("rail-title-corpus").count(), 1, "{body}");
        // And the class has to *do* something. It shipped with no rule behind
        // it anywhere in the sheet, so a borrowed title rendered identical to
        // a heading the passage owns and the distinction lived only in the
        // tooltip — while this assertion passed on the class string alone.
        let css = include_str!("../../assets/app.css");
        assert!(
            css.contains(".rail-title-corpus"),
            "the borrowed-title class has no rule in app.css"
        );
    }

    #[tokio::test]
    async fn the_results_name_what_recalled_an_associated_hit() {
        // An associated hit says which hit recalled it, or it is an unexplained
        // result in a list the reader believes is ranked. Rendered directly
        // rather than driven through a search: the UI handler asks for the
        // default limit, so on any base small enough to reason about, every
        // artifact is already ranked and there is nothing left to recall. What
        // this task changed is the split and the copy, and that is what this
        // pins.
        let template = ResultsTemplate {
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            ..Default::default()
        };
        let body = template.render().unwrap();
        assert!(body.contains("Recalled by association"), "{body}");
        assert!(body.contains("seen together with"), "{body}");
        assert!(body.contains("Mounting E01 images"), "{body}");

        // A judged link says what the relation is instead of what was asked.
        let judged = ResultsTemplate {
            associated: vec![rendered(
                Some("Mounting E01 images"),
                Some("the tool and its errors"),
            )],
            ..Default::default()
        };
        let body = judged.render().unwrap();
        assert!(body.contains("the tool and its errors"), "{body}");
        assert!(!body.contains("seen together with"), "{body}");
    }

    #[tokio::test]
    async fn a_reranked_fragment_says_it_was_refined() {
        // The refining pass visibly reorders the rail; the tick beside the
        // count is what says the movement was the reranker rather than a
        // glitch. The fast pass must not carry it: it is claiming an order
        // the reranker never confirmed.
        let refined = ResultsTemplate {
            results: vec![rendered(Some("Mounting E01 images"), None)],
            reranked: true,
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(refined.contains("refined"), "{refined}");

        let fast = ResultsTemplate {
            results: vec![rendered(Some("Mounting E01 images"), None)],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(!fast.contains("refined"), "{fast}");
    }

    #[tokio::test]
    async fn an_unlinked_search_shows_no_association() {
        // Nothing was linked, so there is nothing to recall. This only pins
        // the absence of the section on a corpus with no links — the
        // `all_weak` invariant itself is proven separately below, since this
        // search never has an association present to prove it against.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();

        let body = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(!body.contains("Recalled by association"), "{body}");
    }

    #[tokio::test]
    async fn the_search_fragment_takes_the_explain_flag_as_a_url_writes_it() {
        // `serde_urlencoded` reads a bare `bool` only as `true`/`false`, so
        // `explain=1` — the spelling `/ui?explain=1` puts on the form and the
        // one every hand-written URL carries — was a 400 for the whole
        // fragment: asking why a row ranked emptied the rail instead.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();

        for spelling in ["explain=1", "explain=true", "explain=on", "explain"] {
            let uri = format!("/ui/search/results?q=alpha&{spelling}");
            // `get_body` asserts the 200 itself.
            let body = get_body(&app, &cookie, &uri).await;
            assert!(body.contains("rail-item"), "{uri} returned no rail: {body}");
        }
    }

    fn ranked(weak: bool) -> RenderedResult {
        RenderedResult {
            why_ranked: None,
            artifact_id: "r1".into(),
            title: "The ranked hit".into(),
            titled_by_corpus: false,
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: if weak { String::new() } else { "#1".into() },
            weak,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: None,
            reason: None,
            model_written: false,
            origin_count: 0,
            continues: false,
            continues_in: String::new(),
        }
    }

    #[test]
    fn an_association_cannot_make_the_answer_look_worse_than_it_was() {
        // `all_weak` is a statement about how well the *query* was answered. An
        // associated hit did not answer the query at all, so its presence must
        // not move this verdict either way. Proven both directions: a weak
        // ranked answer still warns with an association beside it, and a good
        // ranked answer stays silent with one beside it too.
        let weak_with_association = ResultsTemplate {
            results: vec![ranked(true)],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            all_weak: true,
            ..Default::default()
        };
        let body = weak_with_association.render().unwrap();
        assert!(
            body.contains("Nothing matches closely"),
            "an association hid a real warning: {body}"
        );

        let good_with_association = ResultsTemplate {
            results: vec![ranked(false)],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            ..Default::default()
        };
        let body = good_with_association.render().unwrap();
        assert!(
            !body.contains("Nothing matches closely"),
            "an association manufactured a warning: {body}"
        );
    }

    #[test]
    fn a_chosen_theme_beats_the_system_preference() {
        // The light palette has been in the stylesheet since the port from
        // Vestigo and nobody has ever seen it: it activated only on
        // prefers-color-scheme. A choice has to override the system in both
        // directions, or it is not a choice.
        let css = include_str!("../../assets/app.css");
        assert!(
            css.contains(r#":root[data-theme="dark"]"#),
            "an explicit dark choice cannot beat a light system"
        );
        assert!(
            css.contains(r#":root:not([data-theme="light"])"#),
            "the system dark block does not yield to an explicit light choice"
        );
    }

    #[test]
    fn the_theme_is_applied_before_the_first_paint() {
        // A deferred script runs after the first paint and a stylesheet cannot
        // know a stored choice, so either way the wrong theme flashes on every
        // load — on a phone, brightly. The inline script has to come before the
        // stylesheet it is correcting.
        let layout = include_str!("templates/layout.html");
        let script = layout.find("engram.theme").expect("no pre-paint script");
        let sheet = layout.find("/assets/app.css").expect("no stylesheet link");
        assert!(
            script < sheet,
            "the theme is applied after the stylesheet loads, which is the flash"
        );
    }

    #[test]
    fn headings_are_headings_and_labels_are_labels() {
        // h3 was restyled globally into a small uppercase muted label, which is
        // why no page had hierarchy: the element that would carry it had been
        // spent on a style. Every <h3> in the templates was a real heading —
        // Recent, Merged, Pursuits, API tokens — wearing a label's clothes.
        let css = include_str!("../../assets/app.css");
        assert!(
            css.contains(".label {"),
            "no .label class to carry the old h3 style"
        );
        assert!(
            !css.contains("h3 { font-size: 0.8125rem"),
            "h3 is still restyled as a label"
        );
        assert!(css.contains("--text-lg:"), "the type scale is missing");

        // The two classes that had independently reinvented the label style now
        // defer to it, so there is one label vocabulary rather than three.
        let detail = include_str!("templates/_artifact_detail.html");
        assert!(
            detail.contains(r#"class="label pane-label""#),
            "the pane label does not compose .label"
        );
        let workspace = include_str!("templates/workspace.html");
        assert!(
            workspace.contains(r#"class="label facet-label""#),
            "the facet label does not compose .label"
        );
    }

    #[test]
    fn the_artifact_actions_carry_labels() {
        // One screen carried three button vocabularies: unlabelled icon
        // buttons stranded at the top of a wide row, text links inside the
        // card, and solid buttons elsewhere. An icon alone is a guess — a
        // check mark could as easily mean "done" as "still true".
        //
        // Asserted against the template source rather than a render: the
        // fragment is `ArtifactDetailFragment { d: ArtifactDetail }` and
        // building an ArtifactDetail by hand is thirty lines of scaffolding to
        // check for three words. The words are the whole change.
        //
        // The words themselves are `the_pane_controls_name_the_question_they
        // _answer`'s subject; this is only that each control has one.
        let tpl = include_str!("templates/_artifact_detail.html");
        for word in ["Still accurate", "Hide from results", "Delete"] {
            assert!(
                tpl.contains(&format!("<span>{word}</span>")),
                "the {word} control has no label"
            );
        }
        // And a result row offers none of them. Deleting from the rail was the
        // one irreversible act in the app that could be fired on something
        // nobody had opened, and the only one-click act a result carried — so
        // the permanent choice was the easy one, while hiding, which can be
        // undone, meant opening the artifact first. The square icon button is
        // still right where controls repeat down a list the operator is working
        // through: the corpus page's own artifacts, and the pairs on Ops.
        let rail = include_str!("templates/_results.html");
        assert!(
            !rail.contains("/delete"),
            "a result row must not delete what it is only showing"
        );
        assert!(
            include_str!("templates/_artifact.html").contains("btn-icon btn-icon-danger"),
            "the corpus page's artifact list stopped offering delete"
        );
    }

    #[test]
    fn the_open_rail_card_keeps_a_line_of_itself() {
        // The rail is the ranking as well as a list of links. A card
        // collapsing to a bare stub when opened punched a hole in the ordering
        // and lost the reader's place in it; the accent border and background
        // were always what said which one was open.
        let css = include_str!("../../assets/app.css");
        assert!(
            !css.contains(r#".rail-item[aria-selected="true"] .rail-snippet { display: none; }"#),
            "the open card still erases its snippet"
        );
        // Demoted, not unreadable: 0.55 over the dark base is very likely
        // under AA, and a result past the cliff is still a result.
        assert!(
            !css.contains(".rail-past { opacity: 0.55; }"),
            "past-cliff results are still dimmed below the contrast floor"
        );
    }

    /// The two controls in the pane say which question they answer.
    ///
    /// "Verified" sat a few lines above *Was this what you were looking for?*
    /// and its Yes, and read as the same question asked twice. They are not:
    /// Yes labels the search — this query found the right artifact — and feeds
    /// recall; this one labels the artifact — the text is still accurate — and
    /// resets the age that search's recency term reads in place of
    /// `created_at`. "Hide" carried what it hides from, and that the artifact
    /// survives it, only in a `title`, which a phone never shows.
    #[test]
    fn the_pane_controls_name_the_question_they_answer() {
        let html = include_str!("templates/_artifact_detail.html");
        assert!(
            html.contains("<span>Still accurate</span>"),
            "the confirm control still reads as an answer about the search"
        );
        assert!(
            html.contains("<span>Hide from results</span>"),
            "the hide control still says only `Hide`"
        );
        // And the search bar it sits over is unchanged: that one is right, and
        // the confusion was never its half.
        let bar = include_str!("templates/_search_verdict.html");
        assert!(bar.contains("Was this what you were looking for?"));
    }

    /// The offer is hidden by a keystroke, never destroyed by one.
    ///
    /// It used to be removed, on the argument that it is a measured impression
    /// and one reappearing in a new situation would be a second impression
    /// nobody had. `confirmOffer` runs on `htmx:afterSwap` and nowhere else,
    /// so that argument is false: the impression is written once, when the
    /// fragment arrives, and hiding a card already on screen writes nothing.
    /// What the removal did was destroy the card on the first keystroke, after
    /// which the idle column came back without it for the rest of the session.
    ///
    /// The race it was reaching for is handled where it happens: an offer
    /// whose fetch lands after the keystroke was never seen, and `dropOffer`
    /// on the swap refuses to count it.
    #[test]
    fn a_keystroke_hides_the_offer_and_does_not_destroy_it() {
        let js = include_str!("../../assets/app.js");
        let start = js.find("function hideIdle()").expect("no hideIdle");
        let body = &js[start..start + 260];
        assert!(
            !body.contains("area.remove()"),
            "hideIdle still destroys the offer instead of hiding it: {body}"
        );
        // And the one case that must still drop it stays.
        assert!(js.contains("function dropOffer()"));
        assert!(js.contains("if (offerDismissed) dropOffer();"));
    }

    #[test]
    fn every_page_anchors_to_the_same_left_edge() {
        // Three shell widths meant the content column moved under a brand that
        // did not, so navigating jolted — and on Search the query box lined up
        // with nothing else on its own page. A page now declares which regions
        // it uses and never declares a width; the grid puts `rail` and `focus`
        // in the same columns everywhere, which is what makes the anchor
        // single.
        let css = include_str!("../../assets/app.css");
        assert!(
            !css.contains("shell-wide"),
            "shell-wide still sets a per-page width"
        );
        assert!(
            css.contains(".regions-rail-focus-source"),
            "the three-up region tier is missing"
        );
    }

    #[test]
    fn colliding_capture_labels_get_told_apart() {
        // Synthesis names a capture by lifting a heading out of it, and a
        // heading repeats across every document that carries it: six rows read
        // HOCHSCHULE MITTWEIDA and the column that exists to tell captures
        // apart could not. The opening words are the one thing that differs.
        let mut rows = vec![
            QueueRow {
                label: "HOCHSCHULE MITTWEIDA".into(),
                opening: "Kapitel 1 Einleitung".into(),
                ..Default::default()
            },
            QueueRow {
                label: "HOCHSCHULE MITTWEIDA".into(),
                opening: "Kapitel 5 Malware".into(),
                ..Default::default()
            },
            QueueRow {
                label: "Configure auditd".into(),
                opening: "auditctl -w /etc".into(),
                ..Default::default()
            },
        ];
        disambiguate_labels(&mut rows);
        // The label keeps its own name and the opening is kept beside it,
        // rather than being folded into it. Appended, it was cut off by the
        // one `nowrap` line the row gives a title — so this repair ran on the
        // deployment and six rows still read the same six words.
        assert_eq!(rows[0].label, "HOCHSCHULE MITTWEIDA");
        assert_eq!(rows[0].opening, "Kapitel 1 Einleitung");
        assert_eq!(rows[1].label, "HOCHSCHULE MITTWEIDA");
        assert_eq!(rows[1].opening, "Kapitel 5 Malware");
        // A label that was already unique is left alone: the opening beside it
        // is a repair, not a decoration.
        assert_eq!(rows[2].label, "Configure auditd");
        assert!(rows[2].opening.is_empty());
    }

    #[test]
    fn a_collision_with_no_opening_words_is_left_alone() {
        // A photo, or a PDF whose extraction has not landed, has no opening
        // words — and "document · document" tells no one anything.
        let mut rows = vec![
            QueueRow {
                label: "document".into(),
                opening: String::new(),
                ..Default::default()
            },
            QueueRow {
                label: "document".into(),
                opening: String::new(),
                ..Default::default()
            },
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].label, "document");
        assert_eq!(rows[1].label, "document");
    }

    #[test]
    fn a_label_is_not_repeated_back_to_itself() {
        // An untitled capture is already called by its opening words. Appending
        // them would render "auditctl -w /etc · auditctl -w /etc".
        let mut rows = vec![
            QueueRow {
                label: "auditctl -w /etc".into(),
                opening: "auditctl -w /etc".into(),
                ..Default::default()
            },
            QueueRow {
                label: "auditctl -w /etc".into(),
                opening: "auditctl -w /etc".into(),
                ..Default::default()
            },
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].label, "auditctl -w /etc");
    }

    #[test]
    fn a_primed_hit_says_why_it_arrived() {
        // primed, loose and model-written already reached the rail as chips
        // scattered across the header, each with its explanation hidden in a
        // title attribute. The badge said what the result is; nothing said why
        // it was here.
        let mut r = ranked(false);
        r.primed = true;
        let body = ResultsTemplate {
            results: vec![r],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(body.contains("rail-why"), "no provenance line: {body}");
        assert!(
            body.contains("opened, confirmed or cited more than the hits it passed"),
            "{body}"
        );
    }

    #[test]
    fn the_rail_sentence_names_a_cap_that_redistributed_nothing() {
        let e = crate::core::explain::HitExplanation {
            retrieved_rank: Some(3),
            cap: crate::core::explain::CapEffect::Refilled,
            ..Default::default()
        };
        let s = why_ranked(&e).expect("a refilled hit has something to say");
        assert!(
            s.contains("one source filled the list"),
            "the operator gets the consequence, not the mechanism: got {s:?}"
        );
    }

    #[test]
    fn a_hit_no_stage_touched_says_nothing() {
        let e = crate::core::explain::HitExplanation {
            retrieved_rank: Some(0),
            cap: crate::core::explain::CapEffect::Kept,
            ..Default::default()
        };
        assert!(
            why_ranked(&e).is_none(),
            "a quiet stage renders nothing; a row of no-ops is noise"
        );
    }

    #[test]
    fn an_ordinary_hit_explains_nothing() {
        // A line under every result saying "this matched your query" is noise
        // that makes the lines worth reading harder to see.
        let body = ResultsTemplate {
            results: vec![ranked(false)],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(
            !body.contains("rail-why"),
            "an ordinary hit explained itself: {body}"
        );
    }

    #[test]
    fn a_primed_hit_gets_a_small_marker() {
        let mut r = ranked(false);
        r.primed = true;
        let body = ResultsTemplate {
            results: vec![r],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(body.contains("primed"), "{body}");
    }

    #[test]
    fn status_maps_to_the_right_badge_class() {
        use crate::store::corpora::CorpusStatus::*;
        assert_eq!(status_badge(&Ready), "badge-success");
        assert_eq!(status_badge(&Partial), "badge-warning");
        assert_eq!(status_badge(&Failed), "badge-danger");
        assert_eq!(status_badge(&Raw), "badge-accent");
        assert_eq!(status_badge(&Embedding), "badge-accent");
    }

    #[tokio::test]
    async fn every_ui_route_requires_a_session() {
        let (app, _) = app_with_session().await;
        for uri in [
            "/ui/capture",
            "/ui/search",
            "/ui/search/results?q=x",
            "/ui/browse",
            "/ui/queue",
            "/ui/corpora/abc",
            "/ui/ask",
            "/ui/insights",
        ] {
            // A plain GET is a browser loading a page, so a missing session
            // sends it to sign in rather than showing it JSON it cannot act
            // on. `redirect_unauthenticated_browsers` (web/mod.rs) is what
            // rewrites the 401 into this.
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{uri} was unprotected");
            // And it names the page it bounced, so signing in comes back here
            // rather than dropping everyone on Search.
            let go: String = url::form_urlencoded::byte_serialize(uri.as_bytes()).collect();
            assert_eq!(
                res.headers().get("location").unwrap(),
                &format!("/auth/login?go={go}"),
                "{uri} did not send an unauthenticated page load to sign in"
            );
        }
        for uri in [
            "/ui/capture",
            "/ui/ops/tokens",
            "/ui/corpora/abc/delete",
            "/ui/corpora/abc/reprocess",
            "/ui/ops/pairs/1/dismiss",
            "/ui/ask",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .method("POST")
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from("name=x&text=y&q=z"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "POST {uri} was unprotected"
            );
        }
    }

    #[tokio::test]
    async fn a_parked_capture_says_so_instead_of_claiming_it_is_processing() {
        // The confirmation is the only page the writer sees. Telling them a
        // parked capture is "processing" means it silently never is.
        let (app, cookie, core) = app_session_and_core().await;
        let body: String = (0..200)
            .map(|i| format!("step {i} run the mount command and read its output"))
            .collect::<Vec<_>>()
            .join("\n");
        core.ingest(&body, "web", None).await.unwrap();

        // Hand-encoded rather than pulling in a dependency: the body is plain
        // words, so spaces and newlines are all there is to escape.
        let edited = body
            .replacen("step 7 ", "step seven ", 1)
            .replace(' ', "+")
            .replace('\n', "%0A");
        let res = app
            .oneshot(form("/ui/capture", &cookie, &format!("text={edited}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = flat(&body_of(res).await).to_lowercase();
        assert!(
            html.contains("waiting on a decision"),
            "the parked capture rendered as an ordinary one: {html}"
        );
        assert!(
            !html.contains("badge-accent\">processing"),
            "a parked capture must not claim to be processing: {html}"
        );
    }

    #[tokio::test]
    async fn capturing_text_stores_it_and_the_idle_line_says_so() {
        // The press answers with nothing, and that is the point: everything it
        // used to write went into `#capture-result`, which lives inside the
        // bar that is fixed to the bottom of a phone's screen — so the answer
        // to a capture pushed the box out of the viewport.
        //
        // A box that empties itself with no acknowledgment would be
        // indistinguishable from data loss, so the acknowledgment has to be
        // somewhere. It is the "last kept" line of `_idle_foot.html`, which
        // names the capture and links to it, and which the same `submit` that
        // refreshes the rail swaps out of band.
        let (app, cookie, core) = app_session_and_core().await;
        let res = app
            .clone()
            .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.trim().is_empty(),
            "the press still writes over the box it cleared: {html}"
        );

        let stored = core.store.list_corpora(10, 0).await.unwrap();
        assert_eq!(stored.len(), 1, "the capture itself still landed");
        let idle = get_body(&app, &cookie, "/ui").await;
        assert!(
            idle.contains(&format!("/ui/corpora/{}", stored[0].id)),
            "and nothing names what was last kept: {idle}"
        );
    }

    #[tokio::test]
    async fn capturing_the_same_text_twice_says_so() {
        // The one thing the queue cannot report: the second paste adds no row,
        // so without this the page looks like nothing happened at all.
        let (app, cookie) = app_with_session().await;
        for _ in 0..1 {
            app.clone()
                .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
                .await
                .unwrap();
        }
        let res = app
            .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
            .await
            .unwrap();
        assert!(
            body_of(res).await.to_lowercase().contains("already stored"),
            "a duplicate paste must say why nothing new appeared"
        );
    }

    #[tokio::test]
    async fn capture_takes_only_text() {
        // The label field is gone from the form. A client still sending one —
        // a cached page, a script written against the old form — must not get
        // a 422 for a field the server stopped caring about.
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=another+one&title=ignored",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_deep_link_runs_its_query_instead_of_only_filling_the_box() {
        // `/ui/search?q=dd` restored the text but not the results, so the page
        // opened as a filled box over an empty rail until someone typed.
        let (app, cookie) = app_with_session().await;
        let page = |uri: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let res = app
                    .oneshot(
                        Request::builder()
                            .uri(uri)
                            .header("cookie", cookie)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::OK);
                body_of(res).await
            }
        };

        let linked = page("/ui/search?q=mounting").await;
        assert!(
            linked.contains("load"),
            "the deep link never asks for its own results"
        );
        // The form's own trigger, not the page's text: the idle column below
        // the box carries `load` triggers of its own — the offer and the due
        // band each fetch themselves — and asserting over the whole document
        // would read one of those as a search.
        assert!(
            !trigger_of(&page("/ui/search").await).contains("load"),
            "an empty box has nothing to search for"
        );
    }

    /// The spinner names which of the two passes is running.
    ///
    /// A seam with a compiler on neither side: the id lives in the template,
    /// the words live in `app.js`, and the discrimination is `wasRefine` —
    /// which the fast branch also reads. Renaming any one of the three
    /// silently leaves the box saying the wrong thing about what it is doing,
    /// with nothing failing anywhere.
    #[test]
    fn the_spinner_says_which_pass_is_in_flight() {
        let js = include_str!("../../assets/app.js");
        assert!(
            js.contains("'reranking…' : 'retrieving…'"),
            "the two passes are no longer named apart in app.js"
        );
        assert!(
            js.contains("getElementById('search-spinner')"),
            "app.js no longer reaches the element the template renders"
        );
        let tpl = include_str!("templates/workspace.html");
        assert!(
            tpl.contains(r#"id="search-spinner""#),
            "the element the words are written into is gone from the template"
        );
    }

    #[tokio::test]
    async fn search_results_are_a_fragment_not_a_page() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(!html.contains("<html"), "results must be a fragment");
    }

    #[tokio::test]
    async fn a_box_with_no_search_reranker_never_claims_refinement() {
        // The tick says "the reranker confirmed this order". With no reranker
        // wired — or one scoped to ask alone — a `rerank=true` request still
        // answers, but the claim must not appear: it would be asserting a
        // confirmation that never happened.
        let (app, cookie, core) = app_session_and_core().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("result-count"),
            "the fragment must actually carry results for this test to mean \
             anything: {html}"
        );
        assert!(
            !html.contains("refined"),
            "a box with no search reranker claimed a refinement: {html}"
        );
    }

    /// The tick's title is "Order confirmed by the reranker", so it has to be
    /// derived from what the rerank call did — not from what the request asked
    /// for. A reranker that is configured but down degrades to vector order
    /// with a warning, and the fragment must degrade its claim with it.
    #[tokio::test]
    async fn a_fragment_whose_rerank_failed_never_claims_refinement() {
        let core = crate::core::test_support::test_core_with_failing_reranker().await;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        while crate::jobs::run_one(&handle).await.unwrap() {}

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("result-count"),
            "the fragment must actually carry results for this test to mean \
             anything: {html}"
        );
        assert!(
            !html.contains("refined"),
            "the rerank call failed, so the order is vector order and the \
             fragment must not claim the reranker confirmed it: {html}"
        );
    }

    /// The same claim over nothing: the server skips the rerank call for an
    /// empty result set, so "0 results · refined" would be a confirmation of
    /// an order that was never sent anywhere.
    #[tokio::test]
    async fn an_empty_result_set_never_claims_refinement() {
        let (core, _reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let (app, cookie) = app_with_cookie(core).await;

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=nothing+matches+this&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            !html.contains("refined"),
            "no rerank call was made for an empty answer, so nothing was \
             confirmed: {html}"
        );
    }

    /// The positive half, end to end: a working reranker asked for by the
    /// request is what earns the tick.
    #[tokio::test]
    async fn a_rerank_that_actually_ran_earns_the_refined_tick() {
        let (core, _reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        while crate::jobs::run_one(&handle).await.unwrap() {}

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("refined"),
            "the reranker ran and confirmed the order; the tick is earned: {html}"
        );
    }

    #[tokio::test]
    async fn rendered_chunk_html_is_sanitized() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=%3Cscript%3Ealert(1)%3C%2Fscript%3E+plus+some+words",
            ))
            .await
            .unwrap();
        // Drain the queue so the chunk is embedded and therefore searchable.
        let state_app = app.clone();
        let _ = state_app;

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=words")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            !html.contains("<script"),
            "unsanitized chunk reached the page: {html}"
        );
    }

    #[tokio::test]
    async fn an_empty_query_returns_an_empty_fragment_not_an_error() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "typing then clearing the box must not error"
        );
    }

    #[tokio::test]
    async fn a_box_holding_a_document_is_idle_rather_than_a_query() {
        // The one box is also where a chapter is pasted to be captured, and
        // /ui/capture?from_ask= opens it holding a whole model answer. The
        // template holds the `load` search back on those doors; this is what
        // holds back the keystroke after it, which would otherwise spend an
        // embedding call and a Judge-queue row on a paragraph nobody asked for.
        let (app, cookie) = app_with_session().await;
        let long = "answer+".repeat(MAX_QUERY_CHARS / 4);
        let frag = get(&app, &format!("/ui/search/results?q={long}"), &cookie).await;
        assert!(
            frag.contains(r#"id="idle-foot""#),
            "a pasted document must land on the idle state:\n{frag}"
        );
        assert!(!frag.contains("No matches"), "and not as a verdict on it");
    }

    #[tokio::test]
    async fn the_queue_lists_recent_captures_and_polls_only_while_busy() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();

        // Freshly captured and still queued: the fragment has to ask to be
        // refreshed, or the row would sit at its opening words forever.
        let body = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            body.contains("alpha line"),
            "a capture nothing has read yet is called by its opening words, \
             which is what tells two of them apart"
        );
        assert!(body.contains("every 3s"), "work in flight keeps polling");

        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let body = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            body.contains("alpha line"),
            "the row is called by the name capture derived"
        );
        assert!(
            !body.contains("every 3s"),
            "an idle queue stops polling itself"
        );
        assert!(
            body.contains("captured from:body"),
            "an idle queue still listens, or a capture pasted onto it never \
             appears without a reload"
        );
    }

    #[tokio::test]
    async fn a_capture_that_stopped_without_finishing_says_which_way() {
        // Failed, parked and partial are all "not moving and not done", and
        // all three usually have no artifacts — so the count that describes a
        // finished capture described these as `0 artifacts · —`, which is
        // exactly what a capture that was read and yielded nothing looks like.
        // The only list of captures there now is must distinguish them.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();

        for (status, badge) in [
            (crate::store::corpora::CorpusStatus::Failed, "badge-danger"),
            (
                crate::store::corpora::CorpusStatus::NeedsReview,
                "badge-warning",
            ),
            (
                crate::store::corpora::CorpusStatus::Partial,
                "badge-warning",
            ),
        ] {
            let name = status.as_str();
            core.store.set_corpus_status(&out.id, status).await.unwrap();
            let body = get_body(&app, &cookie, "/ui/queue").await;
            assert!(
                body.contains(badge) && body.contains(name),
                "{name} renders no status of its own"
            );
            assert!(
                !body.contains("0 artifacts"),
                "{name} reads as a finished capture that produced nothing"
            );
            assert!(
                !body.contains("every 3s"),
                "{name} waits on a person or on nobody; polling it changes nothing"
            );
        }
    }

    #[tokio::test]
    async fn capture_offers_a_few_decisions_and_counts_the_rest() {
        // The whole backlog used to render here, on what is now the app's
        // start page: three fifty-row queries and two point lookups per pair
        // on every open, and a screen of warning boxes above the captures.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(
            &core,
            &[
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n",
            ],
        )
        .await;
        for w in ids.chunks(2) {
            core.store.record_pair(&w[0], &w[1], 0.9).await.unwrap();
        }

        let body = get_body(&app, &cookie, "/ui/insights").await;
        assert_eq!(
            body.matches("/supersede").count(),
            crate::web::ops::PAIR_LIMIT * 2,
            "five pairs, both sides offered for each, and nothing beyond that"
        );
        // Seven pairs, five shown. Said on the page, because there is no
        // second page to go and find the other two on.
        assert!(
            body.contains("2 more waiting"),
            "a capped list that does not say it is capped reads as an empty queue"
        );
    }

    #[tokio::test]
    async fn browse_redirects_to_capture() {
        // An installed PWA may still have /ui/browse as its start URL.
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/browse")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/capture");
    }

    #[tokio::test]
    async fn ops_says_what_the_sweeps_did_and_shows_the_runs() {
        let (app, cookie, core) = app_session_and_core().await;
        // Two runs of one sweep: the summary adds them up, the history keeps
        // them apart. That difference is the whole reason both are there.
        for _ in 0..2 {
            core.store
                .record_sweep_run(
                    "associate",
                    crate::store::now(),
                    "ok",
                    r#"{"events":0,"verdicts":0,"forgotten":206,"reopened":0,"armed":0}"#,
                )
                .await
                .unwrap();
        }
        core.store
            .record_sweep_run(
                "consolidate",
                crate::store::now(),
                "failed",
                r#"{"error":"the endpoint was down"}"#,
            )
            .await
            .unwrap();

        let html = get(&app, "/ui/insights", &cookie).await;
        assert!(
            html.contains("412 links forgotten"),
            "the last day did not add the runs up: {html}"
        );
        assert!(html.contains("1 run failed"), "a failed run went unsaid");
        assert!(
            html.contains("the endpoint was down"),
            "the history did not say why a run failed"
        );
    }

    #[tokio::test]
    async fn ops_shows_queue_state() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        // The counts read as a sentence now rather than as a row of badges.
        assert!(html.contains("artifacts,"), "the counts are still stated");
        // The tokens moved to Settings; `the_installation_lives_on_its_own_page`
        // is where they are asserted now.
        // An empty base says so once, instead of answering five headings with
        // "None."
        assert!(html.contains("Nothing hidden"));
        assert!(!html.contains("<h3>Hidden as stale</h3>"));
    }

    #[tokio::test]
    async fn ops_says_how_many_links_there_are_and_how_many_are_named() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        // One `bump_link` call between one pair is one row in `artifact_links`
        // — see `the_counts_say_how_many_links_there_are_and_how_many_are_named`
        // in store::links, which needs two calls between two different pairs
        // to reach a total of two.
        assert!(page.contains("1 links"), "{page}");
    }

    #[tokio::test]
    async fn ops_reports_what_is_retrying_rather_than_asking_for_a_click() {
        let (app, cookie, core) = app_session_and_core().await;
        core.store
            .enqueue(crate::store::jobs::Stage::Embed, "artifact", "a1")
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap().unwrap();
        core.store
            .fail_job(job.id, 9, "endpoint down")
            .await
            .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(html.contains("Retrying"), "{html}");
        assert!(html.contains("endpoint down"));
        assert!(
            !html.contains("Re-synthesize segment"),
            "the review queue is still a to-do list"
        );
    }

    /// Offered on every card, because the judge is not the only reader who can
    /// tell. A pair it called a duplicate can still be two artifacts that say
    /// nothing, and the person looking at it should not have to keep one to
    /// clear it.
    #[tokio::test]
    async fn every_pair_can_be_discarded_whatever_the_judge_said() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();

        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(html.contains("Discard both"), "{html}");
    }

    #[tokio::test]
    async fn the_counts_say_what_they_count() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            page.contains("jobs") || page.contains("No jobs queued"),
            "a job count must not read as an artifact count: {page}"
        );
    }

    #[tokio::test]
    async fn both_reversals_are_called_the_same_thing() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["kept one", "hidden one"]).await;
        core.store
            .set_superseded_by(&ids[1], Some(&ids[0]))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(!page.contains("Put it back"), "{page}");
        assert!(!page.contains("Undo merge"), "{page}");
        assert!(page.contains(">Undo<"), "{page}");
    }

    #[tokio::test]
    async fn identically_titled_rows_are_told_apart() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["Windows Update-Typen", "Windows Update-Typen"]).await;
        core.store
            .set_superseded_by(&ids[1], Some(&ids[0]))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            page.contains("body of Windows Update-Typen"),
            "a row has to say which artifact it is, and two can share a title: {page}"
        );
    }

    #[tokio::test]
    async fn the_installation_lives_on_its_own_page() {
        let (app, cookie) = app_with_session().await;

        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(settings.contains("API tokens"), "{settings}");
        assert!(settings.contains("Browser extension"), "{settings}");

        let ops = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            !ops.contains("API tokens"),
            "housekeeping is about the corpus: {ops}"
        );
        assert!(!ops.contains("Browser extension"), "{ops}");
    }

    #[tokio::test]
    async fn both_pages_are_reachable_once_capture_stops_being_one() {
        let (app, cookie) = app_with_session().await;

        // Insights is a destination in the top row now, so it is reachable
        // from every page rather than from one quiet paragraph at the bottom
        // of a page that is about to stop existing.
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("/ui/insights"), "{page}");

        // Settings had exactly one door, and it was that paragraph. It moved
        // rather than went: an installation you cannot open the settings of
        // is the regression this assertion exists to catch.
        let insights = get_body(&app, &cookie, "/ui/insights").await;
        assert!(insights.contains("/ui/settings"), "{insights}");
    }

    #[tokio::test]
    async fn the_result_list_says_how_many_and_keeps_debug_timing_off_the_page() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=alpha")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Still measured, and still reported — to the place a browser already
        // knows to show it rather than to the operator's page.
        assert!(
            res.headers().contains_key("server-timing"),
            "the measurement moved to a header, it was not dropped"
        );
        let frag = body_of(res).await;
        assert!(frag.contains("result-count"), "the count is stated: {frag}");
        // Out of band into the rail's heading, which sits outside the swapped
        // list: the fragment that fills the rail is also what names the act
        // that filled it.
        assert!(
            frag.contains(r#"hx-swap-oob="innerHTML:#rail-head""#),
            "the count does not reach the rail heading: {frag}"
        );
        // The timing fragment this test was written against is what must stay
        // gone. It used to ride the same response as an out-of-band swap, so
        // the assertion names the timing rather than the mechanism — the
        // mechanism is legitimately in use above.
        assert!(
            !frag.contains("embed ") && !frag.contains("server-timing"),
            "timing is not operator-facing: {frag}"
        );
    }

    #[tokio::test]
    async fn a_settled_row_states_its_count_and_mentions_coverage_only_when_it_is_short() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        // Embedding is what settles a corpus; `finish` alone leaves it in
        // flight, and an in-flight row states its status rather than a count.
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains("artifacts"),
            "a settled row states its count: {frag}"
        );
        // Ten rows all reading "100% covered" is a column that says nothing,
        // and it crowded out the one number on the row that differs. Coverage
        // speaks when it is short — see the low-coverage test below — and stays
        // quiet when it is whole.
        assert!(
            !frag.contains(" covered"),
            "a fully covered row announced a number that is the same on every row: {frag}"
        );
        assert!(
            !frag.contains("badge-warning"),
            "the warning is carried by colour on the number, not by a badge: {frag}"
        );
    }

    #[tokio::test]
    async fn a_pending_pair_leads_with_the_titles_not_with_the_verdict() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(
            &core,
            &["Speicherorte der MS Mail App", "MS Mail App File Locations"],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.94)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        let title = page
            .find("Speicherorte der MS Mail App")
            .expect("a title is on the card");
        let verdict = page
            .find("cover the same ground")
            .expect("the verdict is on the card");
        assert!(
            title < verdict,
            "the titles are the content and lead the sentence: {page}"
        );
    }

    /// A feedback-enabled session over an embedded base, an ask on it, and the
    /// recorded event id. Built like `app_with_embedded_corpus`: synthesis and
    /// embedding are run on the core before the router takes it, because a
    /// capture through the page alone leaves nothing to retrieve.
    async fn ask_recorded() -> (axum::Router, String, crate::core::Core, String, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert_eq!(
            handle.store.ask_stats().await.unwrap().asked,
            1,
            "the UI ask was not recorded"
        );
        let id: String = sqlx::query_scalar("SELECT id FROM ask_events LIMIT 1")
            .fetch_one(&handle.store.pool)
            .await
            .unwrap();
        (app, cookie, handle, html, id)
    }

    /// The second door, for an operator who wants to rewrite the answer before
    /// it is stored: prefilled, and nothing saved until they say so.
    #[tokio::test]
    async fn editing_an_answer_first_fills_the_capture_box_and_stores_nothing() {
        let (app, cookie, core, html, id) = ask_recorded().await;
        assert!(
            html.contains(&format!("/ui/capture?from_ask={id}")),
            "the answer offers no way to edit it before keeping it: {html}"
        );
        let before = core.store.list_corpora(100, 0).await.unwrap().len();

        let page = get_body(&app, &cookie, &format!("/ui/capture?from_ask={id}")).await;
        let answer = core.store.ask_event(&id).await.unwrap().unwrap().answer;
        assert!(
            page.contains(answer.trim()),
            "the answer is not in the box: {page}"
        );
        assert!(
            page.contains(&format!(r#"name="from_ask" value="{id}""#)),
            "the ask does not ride the form, so nothing would record where the text came from: {page}"
        );
        assert_eq!(
            core.store.list_corpora(100, 0).await.unwrap().len(),
            before,
            "opening the capture page must store nothing"
        );
    }

    /// Keep means keep. The button stores the answer where it is read — one
    /// source, queued for the same pipeline every paste goes through, carrying
    /// the question and the artifacts it was written from — rather than
    /// shuttling the text to another page for the operator to save by hand.
    #[tokio::test]
    async fn keeping_an_answer_stores_it_and_queues_it_like_any_capture() {
        let (app, cookie, core, html, id) = ask_recorded().await;
        assert!(
            html.contains(&format!("/ui/ask/{id}/keep")),
            "the answer offers no way to keep it in place: {html}"
        );
        let answer = core.store.ask_event(&id).await.unwrap().unwrap().answer;

        let res = app
            .clone()
            .oneshot(form(&format!("/ui/ask/{id}/keep"), &cookie, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;

        let (corpus_id, origin, metadata): (String, String, String) =
            sqlx::query_as("SELECT id, origin, metadata FROM corpora WHERE raw_text = ?")
                .bind(&answer)
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(origin, "ask", "a kept answer must not read as a paste");
        let meta: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(meta["ask"]["event_id"], id.as_str());
        assert_eq!(meta["ask"]["question"], "what is alpha");
        assert!(
            meta["ask"]["artifact_ids"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
            "the artifacts the answer was written from are the provenance: {meta}"
        );
        assert!(
            body.contains(&format!("/ui/corpora/{corpus_id}")),
            "the operator is not told where the answer went: {body}"
        );
        // Queued, not merely stored: the artifacts and their vectors are what
        // the next stage makes of it, at every synthesis setting.
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE target_id = ?")
            .bind(&corpus_id)
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert!(queued > 0, "a kept answer was stored but never processed");
    }

    /// The point of carrying the id through the edit: what is stored says a
    /// model wrote the text and what it was written from, however much the
    /// operator changed before saving.
    #[tokio::test]
    async fn a_kept_answer_is_stored_as_a_paste_that_records_the_question() {
        let (app, cookie, core, _html, id) = ask_recorded().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                &format!("text=edited+by+hand&from_ask={id}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let corpus: (String, String) = sqlx::query_as(
            "SELECT origin, metadata FROM corpora WHERE raw_text = 'edited by hand'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(corpus.0, "ask", "a kept answer must not read as a paste");
        let meta: serde_json::Value = serde_json::from_str(&corpus.1).unwrap();
        assert_eq!(meta["ask"]["event_id"], id.as_str());
        assert_eq!(meta["ask"]["question"], "what is alpha");
        assert!(
            meta["ask"]["artifact_ids"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
            "the artifacts the answer was written from are the provenance: {meta}"
        );
    }

    /// Retention deletes unjudged questions, so an ask can vanish between the
    /// page load and the save. Storing `origin = "ask"` with no provenance would
    /// leave a corpus asserting a model wrote it and no way to check the claim,
    /// which is worse than not making it.
    #[tokio::test]
    async fn a_kept_answer_whose_ask_is_gone_is_stored_as_an_ordinary_paste() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=an+answer+whose+question+expired&from_ask=no-such-ask",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let row: (String, String) = sqlx::query_as(
            "SELECT origin, metadata FROM corpora WHERE raw_text = 'an answer whose question expired'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(
            row.0, "web",
            "a claim of model authorship must not outlive the evidence for it"
        );
        let meta: serde_json::Value = serde_json::from_str(&row.1).unwrap();
        assert!(meta.get("ask").is_none(), "{meta}");
    }

    /// An ordinary paste is untouched by any of this.
    #[tokio::test]
    async fn an_ordinary_capture_still_records_itself_as_one() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let res = app
            .clone()
            .oneshot(form("/ui/capture", &cookie, "text=typed+by+a+person"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let origin: String =
            sqlx::query_scalar("SELECT origin FROM corpora WHERE raw_text = 'typed by a person'")
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(origin, "web");
    }

    #[tokio::test]
    async fn the_answer_page_offers_a_verdict_when_the_question_was_recorded() {
        let (_app, _cookie, _core, html, id) = ask_recorded().await;
        assert!(html.contains(&format!("/ui/ask/{id}/verdict")), "{html}");
        assert!(html.contains("Nothing here"), "{html}");
        assert!(html.contains(&format!("/ui/ask/{id}/carried")), "{html}");
    }

    #[tokio::test]
    async fn the_answer_page_offers_no_verdict_when_feedback_is_off() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert!(!html.contains("/verdict"), "{html}");
    }

    #[tokio::test]
    async fn a_verdict_is_recorded_and_can_be_undone() {
        let (app, cookie, core, _, id) = ask_recorded().await;
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ask/{id}/verdict"),
                &cookie,
                "verdict=wrong",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bar = body_of(res).await;
        assert!(bar.contains("wrong") && bar.contains("undo"), "{bar}");
        assert_eq!(
            core.store.ask_event(&id).await.unwrap().unwrap().verdict,
            Some(crate::store::asks::AskVerdict::Wrong)
        );

        let bar = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/ask/{id}/verdict"),
                    &cookie,
                    "verdict=none",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(bar.contains("Nothing here"), "the buttons are back: {bar}");
        assert!(
            core.store
                .ask_event(&id)
                .await
                .unwrap()
                .unwrap()
                .verdict
                .is_none()
        );
    }

    #[tokio::test]
    async fn marking_a_carrier_marks_the_answer_right_and_updates_the_bar_out_of_band() {
        let (app, cookie, core, _, id) = ask_recorded().await;
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/ask/{id}/carried"), &cookie, "n=1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("hx-swap-oob"),
            "the verdict bar must follow the toggle: {html}"
        );
        assert!(html.contains("right"), "{html}");
        // One `#ask-verdict` in the response, not a wrapper repeating the id of
        // the bar inside it: two would nest after the first click, and the
        // click after that would match both.
        assert_eq!(
            html.matches(r#"id="ask-verdict""#).count(),
            1,
            "the swapped-in bar carries the id twice: {html}"
        );
        let ev = core.store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(crate::store::asks::AskVerdict::Right));
        assert!(ev.citations[0].carried);
    }

    #[tokio::test]
    async fn judging_an_unknown_question_is_not_found() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form("/ui/ask/nope/verdict", &cookie, "verdict=right"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_insights_page_lists_a_gap_group_as_one_row_and_forgets_it_whole() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        // Two, because one gap is not a group: the sweep leaves a lone question
        // ungrouped rather than buying a name that restates it.
        let mut ids = Vec::new();
        for q in ["how do I mount an E01", "mounting E01 images read only"] {
            let id = core
                .store
                .record_ask(crate::store::asks::NewAsk {
                    question: q.into(),
                    filters: "{}".into(),
                    query_vec: vec![1.0; 8],
                    embed_model: core.embedder.model().to_string(),
                    answer: "Not in the knowledge base.".into(),
                    abstained: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            core.store
                .judge_ask(&id, crate::store::asks::AskVerdict::NothingHere)
                .await
                .unwrap();
            ids.push(id);
        }
        // Before the sweep: each under itself, with a box to fill it.
        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(page.contains("Knowledge gaps"), "{page}");
        assert!(page.contains("mount an E01"), "{page}");
        assert!(page.contains(r#"hx-post="/ui/capture""#), "{page}");

        // After: one row under the sweep's name, and forget names both.
        crate::jobs::gaps::sweep(&core).await.unwrap();
        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(page.contains("Fake topic"), "{page}");
        assert!(
            !page.contains("mount an E01"),
            "a group is its name: {page}"
        );
        let members = format!("ask:{},ask:{}", ids[1], ids[0]);
        assert!(
            page.contains(&members) || page.contains(&format!("ask:{},ask:{}", ids[0], ids[1])),
            "{page}"
        );

        let res = app
            .clone()
            .oneshot(form(
                "/ui/gaps/forget",
                &cookie,
                &format!("members={members}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            !page.contains("Knowledge gaps"),
            "a forgotten group must leave the page: {page}"
        );
    }

    /// The members of a group are resolved when the page is rendered, and
    /// retention expires the very rows they name. A member that has since gone
    /// is already forgotten; stopping on it dismissed the earlier members,
    /// left the later ones, and answered 404 — so htmx swapped nothing and the
    /// row came back on reload under the same label, half forgotten.
    #[tokio::test]
    async fn forgetting_a_group_whose_member_has_since_gone_still_forgets_the_rest() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let mut ids = Vec::new();
        for q in ["how do I mount an E01", "mounting E01 images read only"] {
            let id = core
                .store
                .record_ask(crate::store::asks::NewAsk {
                    question: q.into(),
                    filters: "{}".into(),
                    query_vec: vec![1.0; 8],
                    embed_model: core.embedder.model().to_string(),
                    answer: "Not in the knowledge base.".into(),
                    abstained: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            core.store
                .judge_ask(&id, crate::store::asks::AskVerdict::NothingHere)
                .await
                .unwrap();
            ids.push(id);
        }
        crate::jobs::gaps::sweep(&core).await.unwrap();

        // A member named by the rendered row, gone before the press.
        let res = app
            .clone()
            .oneshot(form(
                "/ui/gaps/forget",
                &cookie,
                &format!("members=ask:no-such-ask,ask:{},ask:{}", ids[0], ids[1]),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            !page.contains("Knowledge gaps"),
            "the members that were still there are forgotten: {page}"
        );
    }

    #[tokio::test]
    async fn forgetting_a_malformed_member_is_refused_rather_than_skipped() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form("/ui/gaps/forget", &cookie, "members=ask:g1,nonsense"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_capture_that_answered_something_says_so_on_its_row() {
        // Coverage is closed silently — nothing asked the operator to confirm
        // it — so the queue row is the only place it is said.
        let mut c = crate::core::test_support::test_core().await;
        c.learn.enabled = true;
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "mounting an E01".into(),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        let gap = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "how do I mount an E01".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; crate::core::test_support::TEST_DIM],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(
                &gap,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();
        core.store
            .cover_gap(
                crate::store::gaps::GapKind::Search,
                &gap,
                &src.id,
                &a,
                0.71,
                crate::store::gaps::CoveredBy::Distance,
            )
            .await
            .unwrap();

        // The queue is its own fragment: the capture page fetches it on load.
        let queue = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            queue.contains("how do I mount an E01"),
            "the row does not say what this capture answered: {queue}"
        );
        // And the gap itself is gone from the list it was on.
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            !page.contains(&format!("gap-search-{gap}")),
            "a covered gap is still open: {page}"
        );
    }

    #[tokio::test]
    async fn a_question_the_operator_arrived_with_is_never_overwritten() {
        // A question carried in the URL is one they chose. The sitting fills
        // an empty box and nothing else.
        let (app, cookie, core) = app_session_and_core().await;
        hold_something(&core).await;
        get_body(&app, &cookie, "/ui/search/results?q=something%20else").await;

        let ask = get_body(&app, &cookie, "/ui/ask?q=the%20one%20I%20clicked").await;
        assert!(ask.contains("the one I clicked"), "{ask}");
        assert!(!ask.contains("something else"), "{ask}");
    }

    #[tokio::test]
    async fn a_cold_sitting_renders_no_rail_at_all() {
        // Absent, not empty: a box saying "nothing yet" is worse than no box.
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/search").await;
        assert!(!page.contains("Read just now"), "{page}");
    }

    #[tokio::test]
    async fn every_kind_of_gap_says_which_kind_it_is() {
        // Four ways of saying the base did not answer, on one list. They are
        // not the same claim, and an operator reading the list can tell them
        // apart.
        let mut c = crate::core::test_support::test_core().await;
        c.learn.enabled = true;
        // The fake embedder's vectors are not a semantic space, so the shipped
        // threshold would call everything weak. A line above what the
        // candidate below scores and below nothing else.
        c.set_weak_below(0.5);
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        // Judged a gap.
        let judged = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "judged one".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; crate::core::test_support::TEST_DIM],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(
                &judged,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();
        // Nothing came close.
        core.store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "nothing near one".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; crate::core::test_support::TEST_DIM],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![crate::store::feedback::NewCandidate {
                        artifact_id: "a-1".into(),
                        score: 0.01,
                        similarity: Some(0.01),
                        shown: true,
                        ..Default::default()
                    }],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        // A run of searches that ended unanswered.
        let p = core
            .store
            .insert_pursuit(
                1,
                &["pursued one".into()],
                &[],
                Some((
                    &[1.0; crate::core::test_support::TEST_DIM],
                    core.embedder.model(),
                )),
            )
            .await
            .unwrap();
        core.store
            .close_pursuit(&p, "unsatisfied", "nothing strong was engaged", 2)
            .await
            .unwrap();

        let html = get_body(&app, &cookie, "/ui/insights").await;
        for badge in ["judged", "nothing near", "pursued"] {
            assert!(
                html.contains(badge),
                "no `{badge}` badge on the list: {html}"
            );
        }
    }

    #[tokio::test]
    async fn housekeeping_counts_only_the_pursuits_the_gap_list_still_holds() {
        // `unsatisfied` is how the run ended, and a capture answering it later
        // leaves that word alone on purpose. The gap list drops it all the
        // same, so counting the state pointed the operator at entries that
        // were not there.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let core = handle;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let art = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "how to mount an E01".into(),
                    title: Some("Mounting an E01".into()),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        let mut ids = Vec::new();
        for q in ["pursued one", "pursued two"] {
            let p = core
                .store
                .insert_pursuit(
                    1,
                    &[q.to_string()],
                    &[],
                    Some((
                        &[1.0; crate::core::test_support::TEST_DIM],
                        core.embedder.model(),
                    )),
                )
                .await
                .unwrap();
            core.store
                .close_pursuit(&p, "unsatisfied", "nothing strong was engaged", 2)
                .await
                .unwrap();
            ids.push(p);
        }

        let both = get_body(&app, &cookie, "/ui/insights").await;
        assert!(both.contains("2 went unanswered"), "{both}");

        // A later capture answers one of them.
        core.store
            .cover_gap(
                crate::store::gaps::GapKind::Pursuit,
                &ids[0],
                &src.id,
                &art,
                0.8,
                crate::store::gaps::CoveredBy::Distance,
            )
            .await
            .unwrap();

        let one = get_body(&app, &cookie, "/ui/insights").await;
        assert!(one.contains("1 went unanswered"), "{one}");
        assert!(one.contains("is\n  <a href=\"#gaps\""), "{one}");
    }

    #[tokio::test]
    async fn the_capture_page_shows_no_gaps_block_when_feedback_is_off() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(!page.contains("Knowledge gaps"), "{page}");
    }

    #[tokio::test]
    async fn the_capture_button_comes_after_every_field_it_submits() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;

        // There is one field now. A staged file makes the box that file's
        // note, which is what took the second one away: two boxes on screen
        // and no rule saying which one the words in front of you belong to.
        assert!(
            !page.contains(r#"name="note""#),
            "the note is the box, not a field of its own: {page}"
        );
        let box_at = page
            .find(r#"name="q""#)
            .expect("the one box is on the page");
        let staged = page
            .find(r#"id="staged""#)
            .expect("a file waits to be sent rather than going on arrival");
        // The verb, not the nav link of the same name above it.
        let button = page
            .find(r#"data-verb="capture""#)
            .expect("the capture verb is there");
        assert!(
            box_at < button && staged < button,
            "the button must come after everything it sends: {page}"
        );

        // The box's own form is a GET that searches on every keystroke. The
        // staged file sits inside it, so the serialisation is pinned to the
        // fields the search actually takes — without this, every keystroke
        // carries a filename into the query string. `rerank` is on the list
        // for the refining pass, whose own flag rides this form's GET,
        // `explain` for the same reason, and `tz` because the echo under the
        // box reads a date out of what is being typed: a name missing here is
        // a flag the fragment is never asked with, however carefully the rest
        // is wired.
        assert!(
            page.contains(r#"hx-params="q,category,rerank,explain,fold,tz""#),
            "{page}"
        );
    }

    #[tokio::test]
    async fn the_nav_is_the_same_width_on_every_page() {
        let (app, cookie) = app_with_session().await;
        for uri in ["/ui/capture", "/ui/search", "/ui/insights"] {
            let page = get_body(&app, &cookie, uri).await;
            let bar = page.find(r#"class="topbar""#).expect("a top bar");
            let shell = page.find(r#"class="shell"#).expect("a shell");
            assert!(
                bar < shell,
                "the nav must sit outside the shell, or it inherits that page's \
                 measure and moves as you navigate: {uri}"
            );
        }
    }

    #[tokio::test]
    async fn the_ask_page_prefills_a_question_from_the_query_string() {
        let (app, cookie) = app_holding_something().await;
        let page = get_body(&app, &cookie, "/ui/ask?q=mount+an+E01").await;
        // A textarea carries its value as content rather than as an
        // attribute, which is the one visible consequence of the box being a
        // textarea from the first keystroke to the last.
        assert!(page.contains(">mount an E01</textarea>"), "{page}");
    }

    #[tokio::test]
    async fn ask_renders_an_answer_with_citations() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert!(html.contains("Answer"), "{html}");
    }

    #[tokio::test]
    async fn the_answer_page_badges_and_marks_a_literal_no_excerpt_carries() {
        let mut core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Swapped in after indexing, so only the answer comes from it.
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("First run `wipefs --all /dev/sdX`, then read alpha.".into()),
        }));
        let (app, cookie) = app_with_cookie(core).await;
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert!(
            html.contains("literal no excerpt supports"),
            "no badge: {html}"
        );
        assert!(
            html.contains(r#"<mark class="unsupported">wipefs --all /dev/sdX</mark>"#),
            "the invented command is not marked in the prose: {html}"
        );
    }

    /// A corpus that has been through synthesis and embedding, under a
    /// feedback-enabled core, which is the only state in which an ask both
    /// retrieves something and records a row.
    async fn app_session_and_core_with_an_embedded_base()
    -> (axum::Router, String, crate::core::Core) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        (app, cookie, handle)
    }

    /// `EventSource` is GET-only, and a GET that runs a model call and writes a
    /// row is the kind history and prefetchers replay. The id is the guard, and
    /// it is one-shot.
    #[tokio::test]
    async fn an_ask_handoff_id_cannot_be_used_twice() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let first = get_stream(&app, &cookie, &id).await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = get_stream(&app, &cookie, &id).await;
        assert_eq!(second.status(), StatusCode::NOT_FOUND);
    }

    /// An unknown id is a 404, never a fresh ask against an empty question.
    #[tokio::test]
    async fn an_unknown_handoff_id_is_not_found() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let res = get_stream(&app, &cookie, "nope").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// The stream runs a model call, so it takes the same identity as every
    /// other `/ui` route. Unauthenticated, it would be free inference.
    #[tokio::test]
    async fn the_stream_route_refuses_a_visitor_without_a_session() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let res = get_stream(&app, "", &id).await;
        assert_ne!(
            res.status(),
            StatusCode::OK,
            "an unsigned-in visitor streamed"
        );
    }

    /// The stream is SSE and terminates with the done event carrying the
    /// rendered answer, which is what the page swaps in.
    #[tokio::test]
    async fn the_stream_ends_with_a_done_event_carrying_rendered_html() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let res = get_stream(&app, &cookie, &id).await;
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let body = body_of(res).await;
        assert!(body.contains("event: done"), "{body}");
        assert!(
            done_html(&body).contains("<div class=\"md\">"),
            "the done event carries the rendered fragment: {body}"
        );
    }

    /// The script the page cannot work without is stamped, and the stamp is
    /// derived from the script.
    ///
    /// `/assets/app.js` is served with a year-long `max-age`, so a browser that
    /// has been here before keeps its copy across an upgrade — and since this
    /// page stopped working without JavaScript, an old copy is not a stale
    /// stylesheet but an ask form that submits nothing, silently and per
    /// browser. The query stamp is what moves the URL when the bytes move.
    ///
    /// Recomputed here from the files on disk rather than compared to a
    /// constant, because the property is *content-derived*: a build stamp that
    /// was a version string, a timestamp or a fixed value would satisfy a test
    /// that only looked for `?v=` and would still ship the bug.
    #[tokio::test]
    async fn the_page_stamps_its_script_with_a_hash_of_that_script() {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for name in ["assets/app.js", "assets/app.css", "assets/htmx.min.js"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
            for b in std::fs::read(&path).unwrap() {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        let want = format!("{h:x}");
        assert_eq!(
            crate::web::assets::stamp(),
            want,
            "the stamp is not a hash of the assets it stamps"
        );

        let (app, cookie) = app_holding_something().await;
        let page = get_body(&app, &cookie, "/ui/ask").await;
        assert!(
            page.contains(&format!("/assets/app.js?v={want}")),
            "the page does not stamp its script: {page}"
        );
        // htmx too. It is vendored and changes only on a deliberate bump — which
        // is exactly the moment a year-old cached copy would bite, on every page
        // that still drives its interactions through it.
        assert!(
            page.contains(&format!("/assets/htmx.min.js?v={want}")),
            "the page does not stamp htmx: {page}"
        );

        // The stamped URL still serves the file: the query is not part of the
        // route, and a stamp that 404s would be worse than no stamp at all.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/assets/app.js?v={want}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("askDriver"), "not the driver");
    }

    /// Both ways out of a stream close it.
    ///
    /// The consequence of losing one of these calls is invisible: the answer
    /// still renders, and about three seconds later the browser reconnects to
    /// the stream that ended and asks the question again — a model call nobody
    /// requested, and a doubled bill on a metered endpoint. Nothing else in
    /// this suite can see a browser, so this reads the shipped `app.js` and
    /// insists the calls are there.
    ///
    /// A text assertion, and honestly a weak one: it pins that the lines exist,
    /// not that they run. `tests/browser_ask.rs` is the other half, and it
    /// counts the requests a real browser makes — but it needs node and a
    /// headless Chrome, so it cannot be what guards this on every `cargo test`.
    /// The three regions move with the act, and an ask is an act.
    ///
    /// `hideIdle` was bound to the box's `input` event alone, so the ask door —
    /// `/ui/ask?q=…`, which renders `idle_state` true and pre-fills the box
    /// server-side, so no keystroke is coming — streamed its answer into a
    /// `#pane` still carrying `hidden`. The reader saw a spinner and then
    /// nothing.
    #[test]
    fn pressing_ask_reveals_the_regions_the_answer_is_written_into() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let handler = js
            .split_once(r#"closest('[data-verb="ask"]')"#)
            .expect("the ask click handler is gone")
            .1;
        let handler = &handler[..handler.find("addEventListener").unwrap_or(handler.len())];
        assert!(
            handler.contains("hideIdle();"),
            "the ask handler does not reveal the rail and the pane: {handler}"
        );
        assert!(
            handler.find("hideIdle();") < handler.find("live.hidden"),
            "the regions must be revealed before the answer is written into them"
        );
    }

    /// `dueTick` works from `Date.now() / 1000`, which is fractional. Every
    /// branch of `spanWords` but the first floored its unit, so the last minute
    /// before a reminder read `in 45.372s` and re-jittered on every tick — a
    /// glitch that appeared exactly when the client timer took the row over
    /// from the server's integer `due_words`.
    #[test]
    fn the_countdown_is_whole_seconds() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let f = js
            .split_once("function spanWords(secs) {")
            .expect("spanWords is gone")
            .1;
        let f = &f[..f.find("\n  }").expect("spanWords does not end")];
        assert!(
            f.contains("Math.floor(secs)"),
            "the seconds branch renders a fraction: {f}"
        );
    }

    #[test]
    fn the_stream_driver_closes_the_event_source_on_every_exit() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        // The one place the close actually happens.
        let stop = js
            .split_once("function stop() {")
            .expect("the driver has no stop()")
            .1;
        assert!(
            stop[..stop.find('}').unwrap()].contains("source.close()"),
            "stop() no longer closes the EventSource: {stop}"
        );

        // The answer arrived, so nothing more is coming: close before the
        // payload is touched, or a malformed one leaves the stream open.
        let done = js
            .split_once("addEventListener('done'")
            .expect("the driver does not handle done")
            .1;
        let done = &done[..done.find("addEventListener").unwrap_or(done.len())];
        assert!(
            done.contains("stop();"),
            "the done handler does not close the stream: {done}"
        );
        assert!(
            done.find("stop();") < done.find("JSON.parse"),
            "the stream must be closed before the payload is parsed: {done}"
        );
        // The fragment is set through `innerHTML`, which htmx does not watch:
        // its `hx-post` controls (the verdict bar) are inert until htmx is
        // told about them.
        assert!(
            done.contains("htmx.process(result)"),
            "the done handler no longer hands the answer to htmx: {done}"
        );

        // The failure path, which is also the path a stream that simply ended
        // arrives on: the browser is already queuing its reconnect when this
        // fires.
        let error = js
            .split_once("addEventListener('error'")
            .expect("the driver does not handle error")
            .1;
        assert!(
            error[..error.find("});").unwrap()].contains("fail("),
            "the error handler does not reach the failure path: {error}"
        );
        let fail = js
            .split_once("function fail(message) {")
            .expect("the driver has no fail()")
            .1;
        assert!(
            fail[..fail.find("\n    }").unwrap()].contains("stop();"),
            "fail() no longer closes the stream, so the browser will reconnect: {fail}"
        );
    }

    /// Every `from:` in a template names one element, in one word.
    ///
    /// htmx reads a `from:` selector up to the first space or comma. A
    /// descendant selector therefore binds to its first word and the remainder
    /// is thrown away as an `htmx:syntax:error` that nothing on the page
    /// listens for — so the trigger silently listens to far more than it says.
    /// Search is where that cost showed: `change from:#filters input[type=radio]`
    /// bound to the whole form, the search box fires `change` on blur, and the
    /// blur is caused by the very click that opens a result — so the first
    /// click on a result re-ran the search, swapped the list out between the
    /// press and the release, and opened nothing.
    #[test]
    fn no_trigger_scopes_itself_to_a_selector_htmx_will_cut_in_half() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/web/templates");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).expect("the template directory is there") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let html = std::fs::read_to_string(&path).unwrap();
            // Only inside an attribute: the comments above these triggers
            // quote the broken form on purpose.
            for attr in html.split("hx-trigger=\"").skip(1) {
                let attr = &attr[..attr.find('"').unwrap_or(attr.len())];
                for spec in attr.split(',') {
                    let Some(rest) = spec.split_once("from:") else {
                        continue;
                    };
                    checked += 1;
                    let selector = rest.1.trim();
                    // `closest`, `find`, `next` and `previous` are the one
                    // shape htmx does read a second word for.
                    let two_word = ["closest ", "find ", "next ", "previous "]
                        .iter()
                        .any(|p| selector.starts_with(p));
                    assert!(
                        two_word || !selector.contains(char::is_whitespace),
                        "{}: `from:{selector}` binds to `{}` and htmx drops the rest — \
                         give the element an id and name it in one word",
                        path.display(),
                        selector.split_whitespace().next().unwrap_or("")
                    );
                }
            }
        }
        assert!(checked >= 3, "no `from:` triggers were found to check");
    }

    /// The page and the driver agree about what is on it.
    ///
    /// The stream driver in `app.js` reaches for its regions by id, and a
    /// renamed or dropped element does not fail loudly in a browser — it leaves
    /// an ask page that posts nothing and answers nothing, which is exactly the
    /// state this task found the page in. Read out of the shipped `app.js`
    /// rather than listed here, so the two cannot drift apart; scoped to the
    /// `ask-` prefix, because only the ask driver's ids are this page's problem.
    #[tokio::test]
    async fn the_ask_page_carries_every_region_the_stream_driver_looks_up() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let wanted = pulled(&js, "getElementById('ask-", '\'');
        assert!(
            wanted.len() >= 4,
            "the driver looks up almost nothing, so this test checks almost nothing: {wanted:?}"
        );

        let (app, cookie) = app_holding_something().await;
        let page = get_body(&app, &cookie, "/ui/ask").await;
        for id in wanted {
            assert!(
                page.contains(&format!(r#"id="ask-{id}""#)),
                "app.js drives #ask-{id} and the page has no such element: {page}"
            );
        }
        // The old path is gone rather than sitting beside the new one: two
        // submitters on one form would park the question twice and spend a
        // model call on the copy nobody reads.
        //
        // Scoped to the box's own form. The page has other `hx-post`s on it
        // now — the context offer under the box is one — and asserting over
        // the whole document would pass or fail on things that have nothing to
        // do with how a question is parked.
        let form = page
            .split(r#"<form id="box-form""#)
            .nth(1)
            .and_then(|f| f.split("</form>").next())
            .expect("the workspace has a box form");
        assert!(
            !form.contains("hx-post"),
            "the box form still posts through htmx: {form}"
        );
    }

    /// The rail has to be readable while the answer is still being written, so
    /// the excerpts go out as their own event before the first token.
    #[tokio::test]
    async fn the_citations_event_precedes_the_first_token() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let body = ask_over_sse(&app, &cookie, "what+is+alpha").await;
        let cites = body.find("event: citations").expect(&body);
        let token = body.find("event: token").expect(&body);
        assert!(
            cites < token,
            "citations must precede the first token: {body}"
        );
    }

    /// A reader who leaves before `done` records nothing: the recorded id only
    /// reaches the page in `done`, so an abandoned ask has no verdict bar and
    /// nothing anyone could judge, and retention deletes an unjudged row anyway.
    ///
    /// The ask is genuinely under way when the reader leaves — the stream is
    /// read past its excerpts and dropped between them and the answer. Dropping
    /// the response unread would prove nothing: an `async_stream` that is never
    /// polled never runs, so `ask_events` would not have been called at all.
    #[tokio::test]
    async fn an_ask_abandoned_mid_answer_is_not_recorded() {
        let (app, cookie, core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let res = get_stream(&app, &cookie, &id).await;
        assert_eq!(res.status(), StatusCode::OK);

        // Past the excerpts and into the answer: the generator is suspended
        // inside the token loop, with the completion still to be awaited and
        // recorded. Stopping at `citations` would leave it suspended one line
        // earlier and prove nothing about what happens after.
        let seen = read_until(res, "event: token").await;
        assert!(seen.contains("event: retrieved"), "{seen}");
        assert!(seen.contains("event: citations"), "{seen}");
        // Without this the test passes when the answer *failed*: `read_until`
        // also stops at end of stream, and an ask that errored after its
        // excerpts records nothing either — for the wrong reason.
        assert!(
            seen.contains("event: token"),
            "the answer never started: {seen}"
        );
        assert!(
            !seen.contains("event: done"),
            "the reader has to leave before done for this to be about anything: {seen}"
        );

        assert_eq!(
            core.store.ask_stats().await.unwrap().asked,
            0,
            "an unjudgeable row was written for a reader who left"
        );
    }

    /// Reads SSE frames until `marker` has arrived, then drops the body — which
    /// is what a closed tab does. Returns what was read.
    async fn read_until(res: Response, marker: &str) -> String {
        use tokio_stream::StreamExt as _;
        let mut frames = res.into_body().into_data_stream();
        let mut seen = String::new();
        while let Some(chunk) = frames.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if seen.contains(marker) {
                break;
            }
        }
        // Explicit, because this is the whole point of the test: the generator
        // is suspended at the frame just read and is never polled again.
        drop(frames);
        seen
    }

    /// An empty box is refused before anything is parked: it costs no entry in
    /// the map, and no round trip to a stream to find out.
    #[tokio::test]
    async fn an_empty_question_is_refused_without_being_parked() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let st = ask_state_over(core).await;
        let (app, cookie) = app_over(&st).await;
        let res = app
            .oneshot(form("/ui/ask", &cookie, "q=+++"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            st.ask_handoff.lock().unwrap().is_empty(),
            "a question nobody can answer took a slot in the map"
        );
    }

    /// Parking is the only thing that grows the map, so it is where the sweep
    /// has to run: a page opened and never streamed would otherwise leave its
    /// question behind forever.
    #[tokio::test]
    async fn parking_a_question_sweeps_out_one_that_expired() {
        let st = ask_state().await;
        let stale = st.ask_handoff_park(a_question(), "me");
        age_out(&st, &stale);
        assert_eq!(st.ask_handoff.lock().unwrap().len(), 1);

        // The next ask is what collects it.
        let fresh = st.ask_handoff_park(a_question(), "me");
        let held = st.ask_handoff.lock().unwrap();
        assert_eq!(held.len(), 1, "the expired entry survived the sweep");
        assert!(held.contains_key(&fresh), "the sweep took the live entry");
    }

    /// An id that outlived its window is as good as unknown: the tab it belongs
    /// to is gone, and honouring it would spend a model call on nobody.
    #[tokio::test]
    async fn an_expired_handoff_id_is_refused_and_taken_out_of_the_map() {
        let st = ask_state().await;
        let id = st.ask_handoff_park(a_question(), "me");
        age_out(&st, &id);
        assert!(
            st.ask_handoff_take(&id, "me").is_none(),
            "an expired id was honoured"
        );
        assert!(
            st.ask_handoff.lock().unwrap().is_empty(),
            "a refused id must not stay in the map"
        );
    }

    /// A question is answered to the person who asked it. The id is not
    /// guessable, but a URL travels — into a log, another tab, a referer — and
    /// a second subject who arrives with it within the window must get the
    /// same nothing an unknown id gets, while the asker's own stream still can.
    #[tokio::test]
    async fn a_parked_question_is_spent_only_by_the_subject_who_parked_it() {
        let st = ask_state().await;
        let id = st.ask_handoff_park(a_question(), "alice");
        assert!(
            st.ask_handoff_take(&id, "bob").is_none(),
            "somebody else's question was handed over"
        );
        assert!(
            st.ask_handoff_take(&id, "alice").is_some(),
            "a stranger's attempt spent the asker's own stream"
        );
    }

    fn a_question() -> crate::core::ask::AskRequest {
        crate::core::ask::AskRequest {
            q: "what is alpha".into(),
            limit: None,
            tags: vec![],
            category: None,
        }
    }

    /// Backdates a parked entry past its TTL. Reaching into the map rather than
    /// sleeping a minute: the clock is the thing under test, not the wait.
    fn age_out(st: &AppState, id: &str) {
        let mut m = st.ask_handoff.lock().unwrap();
        let p = m.get_mut(id).expect("parked");
        p.at -= crate::web::state::ASK_HANDOFF_TTL * 2;
    }

    async fn ask_state() -> AppState {
        ask_state_over(crate::core::test_support::test_core().await).await
    }

    async fn ask_state_over(core: crate::core::Core) -> AppState {
        crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await
    }

    /// A router and a session over a state the caller still holds, so a test
    /// can look at the same handoff map the routes write to. `app_with_cookie`
    /// builds its own state and cannot be asked what is in it.
    async fn app_over(st: &AppState) -> (axum::Router, String) {
        let cid = crate::store::new_id();
        st.tenants
            .control()
            .insert_session(&cid, "user-1", None, 3600)
            .await
            .unwrap();
        (
            crate::web::router(st.clone()),
            format!("engram_session={cid}"),
        )
    }

    /// The same markup, marked rather than linked: marking subtracts trust, and
    /// a fabricated command is precisely what hides in a code span.
    #[test]
    fn marking_an_unsupported_literal_still_reaches_inside_a_code_span() {
        let out = crate::core::ask::check::mark_unsupported(
            "<pre><code>wipefs --all</code></pre>",
            &["wipefs --all".to_string()],
        );
        assert!(
            out.contains(r#"<mark class="unsupported">wipefs --all</mark>"#),
            "{out}"
        );
    }

    #[tokio::test]
    async fn without_an_ask_model_there_is_no_ask_page_and_no_ask_link() {
        let mut core = crate::core::test_support::test_core().await;
        core.completer = None;
        let (app, cookie) = app_with_cookie(core).await;
        let page = get_body(&app, &cookie, "/ui/search").await;
        assert!(!page.contains("href=\"/ui/ask\""), "{page}");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/ask")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .oneshot(form("/ui/ask", &cookie, "q=anything"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn with_an_ask_model_the_verb_is_there() {
        let (app, cookie, core) = app_session_and_core().await;
        // Something held, because there are two conditions on the door now and
        // this test is about the other one. An empty base hides Ask whatever
        // the configuration says — it can only abstain — so without a capture
        // here the assertion below would pass or fail for the wrong reason.
        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let page = get_body(&app, &cookie, "/ui").await;
        // A button on the box, not a link in the nav. Ask stopped being a
        // place to go the moment the box learned to do it — but the rule the
        // sibling test pins is unchanged: where there is no model there is no
        // door, and this is the other half of it.
        assert!(page.contains(r#"data-verb="ask""#), "{page}");
    }

    #[tokio::test]
    async fn the_pursuit_section_is_not_there_when_pursuits_are_off() {
        let (app, cookie) = app_with_session().await;
        let ops = get_body(&app, &cookie, "/ui/insights").await;
        assert!(!ops.contains("<h3>Pursuits</h3>"), "{ops}");
    }

    // ── Judging at the moment of search ──────────────────────────────────────

    #[tokio::test]
    async fn a_verdict_on_the_bar_past_the_floor_pays_for_a_sweep() {
        // The loop the whole feature is: a verdict is what buys the next
        // measurement, so the check rides on the verdict rather than a timer.
        // It used to ride the deck's verdicts; the bar is the labeller now.
        let (app, cookie, handle, a, event) = searched_app_tuned(Some(1)).await;
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{event}/verdict"),
                &cookie,
                &format!("verdict=hit&artifact_id={a}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        handle.background.wait_idle().await;

        let run = handle.store.latest_eval_run().await.unwrap();
        assert!(run.is_some(), "the floor was crossed and no sweep ran");
        assert_eq!(run.unwrap().pairs_used, 1);
    }

    #[tokio::test]
    async fn under_the_floor_a_verdict_buys_nothing() {
        // Below it a sweep would recommend the quirks of a handful of queries
        // as confidently as a real improvement.
        let (app, cookie, handle, a, event) = searched_app_tuned(Some(50)).await;
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{event}/verdict"),
                &cookie,
                &format!("verdict=hit&artifact_id={a}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        handle.background.wait_idle().await;

        assert!(handle.store.latest_eval_run().await.unwrap().is_none());
    }

    /// The search just recorded, read off the table rather than off the deck.
    /// A search that returned nothing is not a card the deck deals — see
    /// `dealable!` — and the rail asks about one of those.
    async fn newest_event(handle: &crate::core::Core) -> String {
        sqlx::query_scalar("SELECT id FROM search_events ORDER BY created_at DESC, id DESC LIMIT 1")
            .fetch_one(&handle.store.pool)
            .await
            .expect("the search the rail was filled by")
    }

    #[tokio::test]
    async fn the_rail_offers_a_gap_where_nothing_matches() {
        // The deck's `N` key, moved to where the person is when they know.
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=nothing+here").await;
        assert!(rail.contains("No matches."), "{rail}");
        assert!(rail.contains("Nothing here has it"), "{rail}");
        // The button names the search it is a verdict on. The query never
        // reaches the client as data, so no wording can break the request: a
        // `"` in `hx-vals` JSON used to make htmx throw and the button do
        // nothing at all, silently.
        let event = newest_event(&handle).await;
        assert!(
            rail.contains(&format!(
                r#"hx-post="/ui/search/{event}/gap?q=nothing%20here""#
            )),
            "{rail}"
        );
        assert!(!rail.contains("hx-vals='{\"q\""), "{rail}");

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{event}/gap?q=nothing%20here"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.gaps, s.pending), (1, 0), "{s:?}");
        assert!(
            body_of(res).await.contains("recorded as a gap"),
            "the button did not say what it did"
        );
    }

    #[tokio::test]
    async fn the_rail_hands_the_box_the_search_it_should_fold_into() {
        // The box types into one event by naming it, not by being the most
        // recent thing this operator wrote — which is what a second window
        // also is. The id goes back into the form out of band, and the next
        // keystroke carries it.
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=fat32").await;
        let first = newest_event(&handle).await;
        assert!(
            rail.contains(&format!(
                r#"<span hx-swap-oob="innerHTML:#fold-of"><input type="hidden" name="fold" value="{first}">"#
            )),
            "{rail}"
        );

        // The next keystroke, naming it: one search, still.
        get_body(
            &app,
            &cookie,
            &format!("/ui/search/results?q=fat32+mount&fold={first}"),
        )
        .await;
        assert_eq!(newest_event(&handle).await, first);
        assert_eq!(
            handle.store.feedback_stats(0.0).await.unwrap().captured,
            1,
            "the burst folded into the event the page was holding"
        );

        // A second window, holding nothing yet, does not fold into it.
        get_body(&app, &cookie, "/ui/search/results?q=ntfs").await;
        assert_eq!(
            handle.store.feedback_stats(0.0).await.unwrap().captured,
            2,
            "the other window started its own search"
        );
    }

    #[tokio::test]
    async fn a_quotation_mark_in_the_query_does_not_break_the_gap_button() {
        // The capture is on the request path at this door, so the rail comes
        // back naming its own search whatever was typed — and the button
        // carries an id rather than the words.
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        let rail = get_body(
            &app,
            &cookie,
            "/ui/search/results?q=say%20%22hi%22%20%5Cnow",
        )
        .await;
        let event = newest_event(&handle).await;
        // The wording rides the URL, so a quote is percent-encoded rather than
        // spliced into an HTML attribute or a JSON blob.
        let q = "say%20%22hi%22%20%5Cnow";
        assert!(
            rail.contains(&format!(r#"hx-post="/ui/search/{event}/gap?q={q}""#)),
            "{rail}"
        );
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/search/{event}/gap?q={q}"), &cookie, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().gaps, 1);
    }

    #[tokio::test]
    async fn a_stale_bar_says_so_rather_than_writing_over_a_verdict() {
        // The bar is drawn against an unjudged search and the tab holding it
        // can be left open for as long as anyone likes, so both of its writing
        // answers can arrive after another tab has answered the same search.
        // Neither replaces what is there; both come back saying why.
        let (app, cookie, handle, a, event) = searched_app().await;
        handle
            .store
            .judge(
                &event,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();

        for body in [
            format!("verdict=hit&artifact_id={a}"),
            format!("verdict=none&artifact_id={a}"),
        ] {
            let res = app
                .clone()
                .oneshot(form(&format!("/ui/search/{event}/verdict"), &cookie, &body))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert!(
                body_of(res).await.contains("already judged"),
                "the bar wrote over the verdict instead of saying it could not"
            );
        }
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.gaps, s.hits, s.judged), (1, 0, 1), "{s:?}");
    }

}
