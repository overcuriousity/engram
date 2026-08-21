use crate::auth::Identity;
use crate::core::ingest::{ORIGIN_ASK, ORIGIN_WEB};
use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::store::corpora::CorpusStatus;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
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
    /// Past the point where this list's relevance falls off. Greyed, under a
    /// rule; the rank stays, because it did place — the claim withdrawn is
    /// "this is an answer", not "this is fifth". See `search::cliff`.
    pub past_cliff: bool,
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

/// A chunk beside the source lines it claims.
pub struct ArtifactDetail {
    pub id: String,
    pub title: String,
    /// Sanitized by `markdown::render`. Rendered with `|safe`.
    pub html: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    /// The artifact this one was hidden in favour of. Opening a hidden artifact
    /// by link has to say why it is not in results, or it reads as a bug.
    pub superseded_by: Option<String>,
    pub status: crate::store::artifacts::ArtifactStatus,
    pub last_verified_at: Option<i64>,
    /// Conditions the source stated under which this artifact does not apply.
    pub caveats: Vec<String>,
    /// `None` for a merged artifact, which belongs to no corpus. The pane shows
    /// what it was made of instead of corpus lines — see `build_artifact_detail`.
    pub corpus_id: Option<String>,
    /// The artifact's own text, for the edit box. `html` is what is read;
    /// this is what is edited, and rendering one back into the other is not
    /// something markdown round-trips.
    pub text: String,
    /// How this artifact came to exist: what it was written from, generation
    /// by generation, and what it replaced. Empty for a captured artifact that
    /// has replaced nothing — which is most of them, and which is why the
    /// template asks `is_empty` rather than `merged` before rendering it.
    pub lineage: crate::web::lineage_view::Lineage,
    /// Which of the two panes to render. A merged artifact belongs to no corpus
    /// and has no span, so the source pane has no document to link and no lines
    /// to list; it shows what the artifact was written from instead.
    ///
    /// The template used to branch on `sources` being empty, which is the same
    /// question only while a merge still has its sources. One that had lost them
    /// all fell through to the captured branch and rendered a "Source · …
    /// highlighted" label over an empty link and an empty line table — on
    /// exactly the artifact whose orphan notice matters most.
    pub merged: bool,
    /// Written from a pursuit: shows the questions it was written for.
    pub synthesized: bool,
    /// Those questions.
    pub cues: Vec<String>,
    /// True when one of those sources has since been deleted. The text still
    /// carries what it said, so this is a missing link rather than missing
    /// knowledge — and saying so beats listing one source fewer in silence.
    pub orphaned_source: bool,
    /// True when this artifact's source was never captured here — the artifact
    /// was restored from the vector store and its corpus row is a placeholder.
    /// The pane shows the source beside the artifact, so it has to say when what
    /// it is showing is the artifact's own text reflected back rather than the
    /// document it was drawn from.
    pub corpus_restored: bool,
    /// Link to the source, scrolled to and highlighting the exact lines this
    /// artifact was drawn from. Falls back to the plain source page for an
    /// artifact with no recorded span — a restored one, for instance.
    pub source_at_lines: String,
    /// The next passage of the same document, when this one stops in the
    /// middle of a sentence. A segmentation boundary landing mid-clause is not
    /// a thing the pane can prevent, but leaving the reader at "…Einsatz von"
    /// with the rest of the sentence visible in the column beside it and no
    /// way onward is.
    pub continues_at: Option<String>,
    pub segment_idx: Option<i64>,
    pub slice_label: String,
    pub slice_lines: Vec<crate::web::corpus_view::CorpusLine>,
    /// Query terms to highlight, space separated. Empty when the pane was
    /// opened outside a search.
    pub terms: String,
    /// The nearest artifacts to this one. Free in the sense that matters: the
    /// vector is already stored, so this costs no embedding call and no
    /// completion. Empty while the artifact is still waiting to be embedded.
    pub related: Vec<RelatedArtifact>,
    /// What this artifact has been needed alongside, learned from co-retrieval
    /// rather than resemblance. Beside `related`, not instead of it: one list
    /// is what this resembles, the other is what it has been reached for
    /// together with, and they answer different questions.
    pub seen_together: Vec<SeenTogether>,
}

/// A neighbour, as one line in the pane.
pub struct RelatedArtifact {
    pub id: String,
    pub title: String,
    pub snippet: String,
}

/// A link, as one line in the pane. Beside the nearest neighbours, not instead
/// of them: one list is what this artifact resembles, the other is what it has
/// been needed alongside, and they answer different questions.
pub struct SeenTogether {
    pub id: String,
    pub title: String,
    pub snippet: String,
    /// The judge's line, or the question that bound the pair. `None` only for a
    /// link with neither, which is a link nothing can explain yet.
    pub why: Option<String>,
    pub corpus_title: String,
    /// Rendered emphasised: two documents needing each other is the finding.
    /// Two passages of one document needing each other is not.
    pub cross_corpus: bool,
}

/// Work that hit something and is waiting to try again by itself.
pub struct RetryingRow {
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub due: String,
    pub last_error: String,
}

/// A parked capture, with enough of the corpus it resembles to decide without
/// opening both.
pub struct ParkedRow {
    pub id: String,
    pub title: String,
    pub bytes: usize,
    pub other_id: String,
    pub other_title: String,
    pub percent: i64,
}

/// An artifact the sweep hid, with the one it lost to.
pub struct SupersededRow {
    pub id: String,
    pub title: String,
    /// When it was written and how it opens. Two artifacts can carry the same
    /// title — a merge of two documents that named a section identically
    /// produces exactly that — and a table of them is unreadable without
    /// something that differs between the rows.
    pub subtitle: String,
    pub winner_id: String,
    pub winner_title: String,
}

/// A pair waiting on a person.
pub struct PairRow {
    pub id: i64,
    pub percent: i64,
    pub a_id: String,
    pub a_title: String,
    pub b_id: String,
    pub b_title: String,
    /// Each side's opening words, said beside its title only when that title
    /// is shared with another row on the page. Three artifacts genuinely
    /// titled "LevelDB: Funktionsweise und forensische Analyse" turned one
    /// cluster of questions into what looked like one question asked three
    /// times. Same rule as `disambiguate_labels`, same reason.
    pub a_opening: String,
    pub b_opening: String,
    /// Enough of each side to decide by. The titles are links, but following
    /// one leaves the queue and comes back to a card whose other half you now
    /// have to remember — which is not a comparison, it is two readings with a
    /// navigation between them.
    pub a_excerpt: String,
    pub b_excerpt: String,
    pub detail: Option<String>,
    /// The stored `detail` is exactly `"link"` — the judge's duplicate
    /// hand-off (§7), a provenance marker, not prose. The row renders a
    /// sentence explaining that instead, and the percent is not shown as a
    /// measured similarity, because no cosine was ever computed for a pair
    /// found by co-retrieval.
    pub via_link: bool,
    pub contradiction: bool,
    /// Set when the judge named a direction with enough confidence to propose
    /// a supersede. A recommendation only: nothing here has hidden anything,
    /// and either side can still be kept.
    pub obsolete_title: Option<String>,
    /// Which side the judge's proposal amounts to keeping, so the row can
    /// accent that button. Both false when it made no proposal — every pair is
    /// still resolvable, just with nothing recommended.
    pub keeps_a: bool,
    pub keeps_b: bool,
}

/// An artifact flagged stale with no specific replacement.
pub struct DeprecatedRow {
    pub id: String,
    pub title: String,
}

/// An active artifact nobody has confirmed or retrieved in a while.
pub struct StaleRow {
    pub id: String,
    pub title: String,
    pub last_verified: String,
}

pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub created: String,
    pub last_used: String,
    /// What asked for the token, as it announced itself, or `—` for one minted
    /// before this was recorded. The extension names every token it mints the
    /// same thing, so this is what tells two of those rows apart.
    pub minted_by: String,
    pub revoked: bool,
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

/// A wait, coarsely. "in 4h" is the whole of what a reader needs from a backoff
/// — the exact second is noise, and the point of the line is that nobody has to
/// do anything about it.
pub fn fmt_duration(secs: i64) -> String {
    match secs {
        s if s <= 0 => "now".into(),
        s if s < 90 => format!("in {s}s"),
        s if s < 5400 => format!("in {}m", (s + 59) / 60),
        s => format!("in {}h", (s + 3599) / 3600),
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
fn sweep_label(stage: &str) -> &str {
    match stage {
        "synthesize" => "Writing artifacts",
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
        other => other,
    }
}

/// How long something took, past tense.
///
/// `fmt_duration` above answers a different question — when does this run next
/// — and says "now" for zero and "in 5m" for three hundred. Housekeeping spent
/// it on the TOOK column, so every sweep in the history claimed to have taken
/// "now", and a sweep that genuinely ran for five minutes would have claimed
/// to be about to happen.
pub fn fmt_elapsed(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// Unix seconds as an ISO-ish UTC stamp, computed directly so the project does
/// not pull in a date library for one display string.
pub fn fmt_time(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    // Civil-from-days (Howard Hinnant's algorithm), epoch shifted to 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
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
fn artifact_title(c: &crate::store::artifacts::Chunk) -> String {
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
fn artifact_html(c: &crate::store::artifacts::Chunk) -> String {
    if c.provenance == crate::store::artifacts::Provenance::Passage {
        markdown::render_verbatim(&c.text)
    } else {
        markdown::render(&c.text)
    }
}

fn artifact_view(c: &crate::store::artifacts::Chunk) -> ArtifactView {
    ArtifactView {
        id: c.id.clone(),
        title: artifact_title(c),
        html: artifact_html(c),
        text: c.text.clone(),
        tags: c.tags.clone(),
        embed_state: c.embed_state.as_str().to_string(),
        embed_badge: embed_badge(&c.embed_state),
    }
}

// ── Templates ───────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "capture.html")]
struct CaptureTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    /// Decisions waiting on a person, shown where the work arrives rather than
    /// on a page you have to remember to visit. Empty renders nothing at all.
    /// Grouped, because one artifact against three others is one decision and
    /// arrived as three — see `group_pairs`.
    pairs: Vec<PairCluster>,
    /// How many more are behind the ones shown. Said once under the list, so a
    /// short list does not read as an empty queue when it is a capped one.
    more_pairs: i64,
    /// Whether the image door is open, i.e. `[infer.vision]` is configured.
    /// Off, the page offers text only rather than a picker that fails.
    vision_enabled: bool,
    /// Whether capture spends a synthesis call per segment, i.e. `eager`.
    ///
    /// At `earned` and `off` it spends none: the text is embedded as written,
    /// and at `earned` a window is rewritten later only where reading has
    /// earned it. The page has to say which of those is happening — promising
    /// "16 model calls" on a base that will make none is the page lying about
    /// what the button costs.
    eager: bool,
    /// The holes, grouped and named by the sweep. Empty when feedback is off.
    gaps: Vec<GapGroup>,
    /// Open gaps the sweep has not grouped yet.
    loose: Vec<GapMember>,
    /// An answer the operator asked to keep, dropped into the box for them to
    /// edit. Empty on an ordinary visit.
    ///
    /// Prefilled and not saved: the save stays the operator's decision. That is
    /// the line the roadmap draws — this is a person keeping something the
    /// model wrote, recorded as such, and not the system writing memory to
    /// itself.
    prefill_text: String,
    /// The ask this text came from, carried through the form so the capture
    /// records where it came from. Empty when the box was not prefilled.
    ///
    /// The id rather than the prose: a note is a string someone can edit away,
    /// while this is the join back to the question and the artifacts the answer
    /// was built from, and `capture_submit` turns it into stored provenance.
    prefill_ask: String,
    /// The question this answer answered, in the operator's own words.
    ///
    /// The provenance already recorded it — `with_ask` carries the question and
    /// the citations into the corpus metadata. What was missing was saying so
    /// on the page: the box arrived holding an answer with no sign of what it
    /// was an answer to, and the operator deciding whether to keep it is the
    /// person who most needs to see the question. The line does not move; it is
    /// only better documented.
    prefill_question: String,
}

/// One artifact this sitting has been in, as the rail lists it.
pub struct SittingItem {
    pub id: String,
    pub title: String,
}

/// How many of the sitting's touched artifacts a page shows.
///
/// Six, against a carried twenty. The rail is a way back to what you were just
/// reading, not a history: a list long enough to need reading is a second set
/// of results beside the real ones.
const SITTING_RAIL: usize = 6;

/// What this sitting has touched, most recent first, ready to render.
///
/// Empty for a cold sitting and for every door that has no session — which is
/// the whole of what keeps this at the web door. An artifact deleted since is
/// simply absent: the sitting holds ids and the store is the truth.
async fn sitting_rail(st: &AppState, id: &Identity) -> Vec<SittingItem> {
    let Some(sess) = &id.session else {
        return Vec::new();
    };
    let carried =
        st.core
            .sittings
            .read(sess, crate::store::now(), st.core.pursuit.idle_secs as i64);
    let mut out = Vec::new();
    for aid in carried.touched.iter().take(SITTING_RAIL) {
        if let Ok(c) = st.core.store.get_artifact(aid).await
            && c.in_results()
        {
            out.push(SittingItem {
                title: title_of(&c),
                id: c.id,
            });
        }
    }
    out
}

/// One hole in the base, as the capture page lists it.
pub struct GapMember {
    /// The `GapKind`, for the dismiss route.
    pub kind: String,
    /// What asked it, in the operator's words: *judged*, *asked*, *nothing
    /// near*, *pursued*. Four ways of saying the base did not answer, on one
    /// list, each still able to say which one it was.
    pub badge: &'static str,
    pub id: String,
    pub text: String,
}

pub struct GapGroup {
    pub label: String,
    pub members: Vec<GapMember>,
}

fn gap_member(g: crate::store::gaps::Gap) -> GapMember {
    use crate::store::gaps::GapKind;
    GapMember {
        kind: g.kind.as_str().into(),
        badge: match g.kind {
            GapKind::Search => "judged",
            GapKind::Ask => "asked",
            GapKind::Unmatched => "nothing near",
            GapKind::Pursuit => "pursued",
        },
        id: g.id,
        text: g.text,
    }
}

#[derive(Template)]
#[template(path = "_captured.html")]
struct CapturedTemplate {
    id: String,
    duplicate: bool,
    /// Set when the capture was parked as a near-duplicate. Without it the page
    /// says "processing" for a capture that nothing will ever process, and the
    /// only hint is a queue on Ops the writer has no reason to open.
    near_dupe_of: Option<String>,
    near_dupe_percent: i64,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    /// Kept so a reload or a deep link restores the box with its results.
    q: String,
    /// What this collection can actually be narrowed by. Rendered as chips, so
    /// choosing a category never means knowing in advance that it exists.
    facets: crate::vector::Facets,
    /// The chip a deep link arrived with, so the form comes back selected
    /// rather than reset to "all".
    category: String,
    /// What this sitting has been in. Absent on a cold sitting — an empty box
    /// saying "nothing yet" is worse than no box.
    sitting: Vec<SittingItem>,
    /// Whether the area under the search box exists at all. See
    /// `Core::recommends`.
    recommend: bool,
}

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
}

#[derive(Template)]
#[template(path = "_queue.html")]
struct QueueTemplate {
    rows: Vec<QueueRow>,
    /// Whether anything is still moving. The fragment carries its own polling
    /// trigger only while this holds, so an idle page makes no requests.
    active: bool,
}

#[derive(Template)]
#[template(path = "corpus.html")]
struct CorpusTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    id: String,
    status: String,
    badge: &'static str,
    /// This row is a placeholder for a source that was never captured here, so
    /// `raw_text` is its restored artifacts joined rather than a document. The
    /// page has to say so: it otherwise presents reconstructed fragments under
    /// the same "Raw corpus" heading as a real capture, and offers to
    /// re-segment them.
    restored: bool,
    /// The page this was captured from, for the doors that know one. The last
    /// hop back to where the text came from, which is otherwise unrecoverable
    /// once the tab is closed.
    source_url: Option<String>,
    /// An image corpus: the page shows the photo, and the lines below are the
    /// model's reading of it rather than the source itself.
    image: bool,
    /// A PDF corpus: the lines below are docling's extraction of it rather
    /// than the document as it was laid out, and the original is one click
    /// away.
    pdf: bool,
    /// A capture whose reading has not landed — still `describing` or
    /// `extracting`, or parked before any text was read. Nothing to
    /// re-segment; only read it again.
    unread: bool,
    /// Rows of what the door recorded about the capture, already formatted.
    meta_rows: Vec<(String, String)>,
    /// Every other EXIF tag the file carried, by name. Folded away on the page:
    /// a phone emits dozens, and none of them is what someone came here to read
    /// — but the original is not stored, so this is the only place they exist.
    exif_rows: Vec<(String, String)>,
    note: Option<String>,
    /// The source cut where the artifacts claiming it change, each stretch
    /// beside what came of it. Empty for a source there is nothing to band —
    /// a restored placeholder, or a photo not read yet — which falls back to
    /// the flat rendering.
    bands: Vec<BandView>,
    /// Windows synthesis has read because their passages were read — each with
    /// an undo, which puts the verbatim text back in results.
    promoted: Vec<PromotedWindow>,
    /// The artifacts of this capture that name no lines of it, in a section of
    /// their own below the source. Every artifact of a restored placeholder is
    /// here, as is anything written before spans were recorded — and the page
    /// showed none of them once it rendered bands alone.
    unplaced: Vec<ArtifactView>,
    /// Merged and synthesized artifacts with a root in this capture. A merge
    /// belongs to every corpus it drew from, and this is where that shows.
    written_from: Vec<ArtifactView>,
    /// Nothing was captured here at all, so the flat fallback has nothing to
    /// show either.
    lines_empty: bool,
    /// The source as one block, for the fallback. Unnumbered on purpose: the
    /// only thing that reaches it is a restored placeholder, where a line
    /// number is a claim about a document that was never captured here.
    raw_text: String,
    /// How much of the wording survived, as the Recent list measures it.
    /// Stated whether or not a band is red, because the two measures answer
    /// different questions and can disagree.
    coverage: Option<String>,
}

/// A window a promotion has synthesized, for the corpus page's undo list.
pub struct PromotedWindow {
    pub idx: i64,
    pub from: i64,
    pub to: i64,
}

/// One stretch of the source on the corpus page, beside what came of it.
pub struct BandView {
    pub from: i64,
    pub to: i64,
    pub lines: Vec<crate::web::corpus_view::CorpusLine>,
    pub artifacts: Vec<ArtifactView>,
    /// `(id, title)` for the artifacts claiming this band whose card is in an
    /// earlier one — the overlaps. A line pointing up at the card, because the
    /// card itself can only exist once: two copies of it share their element
    /// ids, and edit and delete then reach the wrong one.
    pub echoes: Vec<(String, String)>,
    /// Nothing was written from these lines.
    pub gap: bool,
    /// For a gap band, the lines a re-read would actually cover: the whole
    /// window holding this passage, which is wider than the passage. `None`
    /// when no window holds it and there is nothing to offer.
    pub reread: Option<String>,
}

#[derive(Template)]
#[template(path = "_artifact.html")]
struct ArtifactFragment {
    c: ArtifactView,
}

#[derive(Template)]
#[template(path = "_artifact_detail.html")]
struct ArtifactDetailFragment {
    d: ArtifactDetail,
}

#[derive(Template)]
#[template(path = "artifact_detail.html")]
struct ArtifactDetailPage {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    d: ArtifactDetail,
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

/// One phrase of the last day: "412 links forgotten".
struct SweepCount {
    n: i64,
    what: String,
}

/// One recorded run, as the history renders it.
struct SweepRunRow {
    when: String,
    /// The stage in words. The identifier it was worded from is on the cell as
    /// a `title`, because the log and the config still call it that and a
    /// reader who greps for `arm_dedupe` should find it here too.
    stage: String,
    stage_id: String,
    /// Empty unless it failed, in which case it is why.
    error: String,
    took: String,
    /// The counts, already worded. Empty for a run that did nothing.
    counts: Vec<SweepCount>,
}

/// Add up one run's `detail` into `totals`, keyed by the words it earns.
fn tally_sweep(stage: &str, detail: &str, totals: &mut Vec<(String, i64)>) {
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

#[derive(Template)]
#[template(path = "ops.html")]
struct OpsTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    job_counts: Vec<(String, i64)>,
    oldest_pending_secs: Option<i64>,
    artifact_count: i64,
    vector_count: u64,
    retrying: Vec<RetryingRow>,
    parked: Vec<ParkedRow>,
    superseded: Vec<SupersededRow>,
    /// Artifacts the dedupe pass wrote out of several others, with what they
    /// were written from and an undo.
    merged: Vec<MergedRow>,
    /// The list is capped; there are rows this page is not showing. Said out
    /// loud, because a table that stops without saying so reads as a table of
    /// everything there is.
    more_merged: bool,
    more_superseded: bool,
    /// `TABLE_CAP`, so the line that says how many rows are showing says the
    /// number the code actually truncated to. Written out twice in the
    /// template, it drifted from the constant the first time either moved.
    table_cap: i64,
    deprecated: Vec<DeprecatedRow>,
    stale: Vec<StaleRow>,
    /// `None` when nothing is being learned, which renders nothing at all: a
    /// count of links on a base that records no searches is a line about a
    /// feature that is switched off.
    links: Option<crate::store::links::LinkCounts>,
    /// Artifacts written from pursuits, newest first, each one click from
    /// deprecated.
    generated: Vec<GeneratedRow>,
    /// Recent pursuits, only when the feature is on. A count and not a table:
    /// a pursuit that ended unsatisfied is a hole in the base and belongs on
    /// the one list of those, not on a second list of its own; one that ended
    /// satisfied needs nobody; and one that was written up is in `generated`
    /// above.
    pursuit_enabled: bool,
    pursuit_recent: usize,
    pursuit_unsatisfied: usize,
    /// What the sweeps did in the last twenty-four hours, added up. Not "last
    /// night": units that reschedule themselves on their own periods do not
    /// line up into one cycle, and there is no cycle identity to group them by.
    last_day: Vec<SweepCount>,
    /// Runs in the last day that failed. Said separately, because a summary of
    /// what got done cannot report what did not.
    last_day_failures: usize,
    /// The runs themselves, newest first. What a single overwritten summary
    /// could never give: whether this started yesterday or has been going
    /// wrong for a week.
    sweep_history: Vec<SweepRunRow>,
    /// Shown against clicked, by rung. Empty when the offer is switched off, or
    /// when it has been on and never had anything to say — either way there is
    /// no table, because a heading over no rows is a claim that something is
    /// being measured when nothing is.
    offer_rates: Vec<crate::store::pursuits::OfferRate>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    tokens: Vec<TokenRow>,
    /// `None` when capture is switched off, which renders nothing at all: a
    /// section about a log nobody is keeping is noise.
    feedback: Option<crate::store::feedback::Stats>,
    /// The questions, counted beside the searches. Set exactly when `feedback`
    /// is: one switch records both, one purge takes both, and a page that named
    /// only the searches let an operator clear their query log without knowing
    /// the judged questions went with it.
    asks: Option<crate::store::asks::AskStats>,
}

/// One generated artifact on Ops.
struct GeneratedRow {
    id: String,
    title: String,
    subtitle: String,
    cues: Vec<String>,
    sources: Vec<SourceRow>,
}

struct MergedRow {
    id: String,
    title: String,
    /// See `SupersededRow::subtitle`: what tells two rows with one title apart.
    subtitle: String,
    /// What it was written from, in the order the lineage stores them.
    sources: Vec<SourceRow>,
    /// True when a source has been deleted since, so the artifact claims less
    /// provenance than its text carries.
    orphaned: bool,
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
fn row_subtitle(c: &crate::store::artifacts::Chunk) -> String {
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
async fn source_rows(
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

#[derive(Template)]
#[template(path = "_token_created.html")]
struct TokenCreatedTemplate {
    token: String,
}

#[derive(Template)]
#[template(path = "ask.html")]
struct AskTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    /// A question to prefill the box with — a gap's "ask again", or the query
    /// this sitting was just searching for.
    q: String,
    /// What this sitting has been in. See `SearchTemplate::sitting`.
    sitting: Vec<SittingItem>,
}

#[derive(serde::Deserialize)]
struct AskPrefill {
    #[serde(default)]
    q: String,
}

#[derive(Template)]
#[template(path = "_answer.html")]
struct AnswerTemplate {
    answer: String,
    citations: Vec<RenderedResult>,
    dropped: usize,
    /// The answer stops where its ceiling did. Shown beside `dropped` for the
    /// same reason: a cut-off answer is otherwise indistinguishable from a
    /// finished one.
    truncated: bool,
    /// The answer said "not in the base"; badged so the operator sees what
    /// the harness will count.
    abstained: bool,
    /// Literals the answer carries that no cited excerpt does. Badged, and
    /// marked in `answer`, so a reader can tell what the base holds from what
    /// the model wrote.
    unsupported: Vec<String>,
    /// Set when the question was recorded; the verdict bar exists only then.
    event_id: Option<String>,
    /// The bar, rendered — empty when there is no event.
    verdict_bar: String,
}

#[derive(Template)]
#[template(path = "_ask_rail.html")]
struct AskRailTemplate {
    citations: Vec<RenderedResult>,
}

#[derive(Template)]
#[template(path = "_ask_verdict.html")]
struct AskVerdictTemplate {
    event_id: String,
    /// `right` / `wrong` / `nothing here` for display; `None` shows the buttons.
    verdict: Option<String>,
    /// Marks the bar to swap itself out-of-band. Set when it rides along with
    /// something else — the carrier toggle — and not when it is the response
    /// the click already targets.
    oob: bool,
}

/// What the keep button leaves behind: the outcome of storing the answer.
#[derive(Template)]
#[template(path = "_ask_kept.html")]
struct AskKeptTemplate {
    /// The corpus the answer is now — the new one, or the one that already
    /// held the same bytes.
    id: String,
    duplicate: bool,
    /// Stored but not processed: it resembles something already in the base
    /// closely enough that an operator decides on Ops first.
    parked: bool,
    near_dupe_percent: i64,
}

#[derive(Template)]
#[template(path = "_ask_carried.html")]
struct AskCarriedTemplate {
    event_id: String,
    n: i64,
    carried: bool,
    /// The bar, rendered, to swap out-of-band. Always `Some` from the route.
    bar: Option<String>,
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// What the capture page accepts in its query string.
///
/// `from_ask` rather than the answer itself: an answer runs to thousands of
/// characters and a URL does not, so passing the text would break on exactly
/// the long answers worth keeping. The id is short, and the page reads the
/// stored row.
#[derive(serde::Deserialize)]
struct CapturePrefill {
    #[serde(default)]
    from_ask: Option<String>,
}

async fn capture_page(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<CapturePrefill>,
) -> Result<Response> {
    let (pairs, more_pairs) = pair_rows(&st).await?;
    let pairs = group_pairs(pairs);
    // A prefill that names an ask nobody recorded is not an error worth a page
    // for: the box is simply empty, which is what an ordinary visit looks like.
    let prefilled = match &p.from_ask {
        Some(id) => st.core.store.ask_event(id).await?,
        None => None,
    };
    let (prefill_text, prefill_ask, prefill_question) = match prefilled {
        Some(ev) => (ev.answer, ev.id, ev.question),
        None => (String::new(), String::new(), String::new()),
    };
    // Read, never computed: the page shows what the sweep grouped and named,
    // and whatever has been judged since sits under itself until the next
    // pass. Nothing here embeds or calls a model.
    let (gaps, loose) = if st.core.feedback.enabled {
        let (rows, loose) = st
            .core
            .store
            .gap_rows(st.core.embedder.model(), st.core.weak_below)
            .await?;
        (
            rows.into_iter()
                .map(|r| GapGroup {
                    label: r.label,
                    members: r.members.into_iter().map(gap_member).collect(),
                })
                .collect(),
            loose.into_iter().map(gap_member).collect(),
        )
    } else {
        (vec![], vec![])
    };
    Ok(HtmlTemplate(CaptureTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        pairs,
        more_pairs,
        vision_enabled: st.core.describer.is_some(),
        eager: st.core.synthesis == crate::config::SynthesisMode::Eager,
        gaps,
        loose,
        prefill_text,
        prefill_ask,
        prefill_question,
    })
    .into_response())
}

async fn gap_dismiss(
    State(st): State<AppState>,
    _id: Identity,
    Path((kind, id)): Path<(String, String)>,
) -> Result<Response> {
    let kind = crate::store::gaps::GapKind::parse(&kind)
        .ok_or_else(|| Error::Validation(format!("unknown gap kind {kind}")))?;
    st.core.store.dismiss_gap(kind, &id).await?;
    Ok(axum::http::StatusCode::OK.into_response())
}

/// Text and nothing else. The label field is gone: a name arrives from
/// synthesis, which has read the document, rather than from someone who has
/// just pasted it and does not yet know what it says.
#[derive(serde::Deserialize)]
struct CaptureForm {
    text: String,
    /// Set when the box was prefilled from an answer. Carries the ask through
    /// the edit, so what is stored records that the text was model-written and
    /// what it was written from — even if the operator rewrote every word of it.
    #[serde(default)]
    from_ask: Option<String>,
}

async fn capture_submit(
    State(st): State<AppState>,
    _id: Identity,
    Form(f): Form<CaptureForm>,
) -> Result<Response> {
    // An answer the operator chose to keep is still a paste, and is stored as
    // one — the same pipeline, the same synthesis, no special case downstream.
    // What differs is only the trace: the origin says a model wrote it, and the
    // metadata says from which question and which artifacts. That is the whole
    // of the concession the roadmap makes here, and it is a record rather than
    // a mechanism.
    //
    // The two travel together or not at all. An ask can vanish between the page
    // load and the save — retention deletes unjudged questions — and storing
    // `origin = "ask"` with no `ask` metadata would leave a corpus asserting
    // model authorship while carrying none of the provenance that assertion is
    // supposed to buy. A claim that cannot be checked is worse than no claim, so
    // a lost row falls back to an ordinary paste, which is what it now is.
    let capture = match f.from_ask.as_deref().filter(|s| !s.is_empty()) {
        Some(ask_id) => match st.core.store.ask_event(ask_id).await? {
            Some(ev) => crate::core::ingest::Capture::new(&f.text, ORIGIN_ASK).with_ask(
                &ev.id,
                &ev.question,
                &ev.citations,
            ),
            None => {
                tracing::warn!(
                    ask_id,
                    "capture named an ask that is no longer stored; keeping it as an ordinary paste"
                );
                crate::core::ingest::Capture::new(&f.text, ORIGIN_WEB)
            }
        },
        None => crate::core::ingest::Capture::new(&f.text, ORIGIN_WEB),
    };
    let out = st.core.ingest_capture(capture).await?;
    Ok(HtmlTemplate(CapturedTemplate {
        id: out.id,
        duplicate: out.duplicate,
        near_dupe_percent: out
            .near_duplicate
            .as_ref()
            .map(|n| (n.similarity * 100.0).round() as i64)
            .unwrap_or(0),
        near_dupe_of: out.near_duplicate.map(|n| n.corpus_id),
    })
    .into_response())
}

/// Chips per row. Long enough to cover a real vocabulary, short enough that the
/// row stays a row.
const FACET_LIMIT: usize = 12;

async fn search_page(
    State(st): State<AppState>,
    id: Identity,
    Query(p): Query<UiSearchParams>,
) -> Result<Response> {
    // A vector store that cannot answer must not take the search page down with
    // it: without chips the page is what it was yesterday, with them it is
    // better, and neither is worth a 500.
    let mut facets = st
        .core
        .vectors
        .facets(FACET_LIMIT)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "facets unavailable; rendering search without chips");
            Default::default()
        });
    let category = p.category.unwrap_or_default();
    // A deep link can name a value that falls outside the top `FACET_LIMIT`, or
    // one nothing carries at all. The rail is narrowed by it either way, so the
    // chip row has to show it: otherwise the page reads as unfiltered while the
    // results are not, and there is no chip to click to get back out.
    ensure_facet(&mut facets.categories, &category);
    Ok(HtmlTemplate(SearchTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        q: p.q,
        facets,
        category,
        sitting: sitting_rail(&st, &id).await,
        recommend: st.core.recommends(),
    })
    .into_response())
}

/// One offer, flattened for the template. Every decision — which rung, which
/// blocks, how the stamp reads — is made here, so the template holds no logic
/// and a new block in the encoder changes no markup.
#[derive(Default)]
pub struct OfferView {
    pub id: String,
    pub title: String,
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
async fn context_offer(
    State(st): State<AppState>,
    id: Identity,
    Form(f): Form<ContextForm>,
) -> Result<Response> {
    if !st.core.recommends() {
        return Ok(HtmlTemplate(ContextTemplate::default()).into_response());
    }
    let bundle = crate::core::context::parse_bundle(&f.bundle);
    st.core
        .record_context_event(&f.bundle, &bundle, Some(&id.subject));

    // A recommendation that cannot be computed is not worth a 500: the area is
    // what it was yesterday, which is empty.
    let offer = st
        .core
        .offer(Some(&id.subject), &bundle)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not build a recommendation");
            None
        });

    if let Some(o) = &offer {
        st.core.record_recommendation(
            &o.artifact_id,
            "recommended_shown",
            o.rung.as_str(),
            o.slot,
            Some(&id.subject),
        );
    }
    Ok(HtmlTemplate(ContextTemplate {
        offer: offer.map(offer_view),
    })
    .into_response())
}

fn offer_view(o: crate::core::recommend::Offer) -> OfferView {
    use crate::core::recommend::Rung;
    OfferView {
        rung: match o.rung {
            Rung::Pattern => "Pattern".to_string(),
            Rung::Similar => "Similar to".to_string(),
            // The count, in words a person reads. `weight` is the decayed
            // number the ranking uses and nobody can read 1.9 and know it means
            // twice — so the undecayed count is stored alongside it and said
            // out loud here.
            Rung::Tentative => match o.events {
                0 | 1 => "Once before".to_string(),
                2 => "Twice before".to_string(),
                n => format!("{n} times before"),
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
        // from.
        rec: match o.slot {
            Some(s) => format!("?rec={s}&rung={}", o.rung.as_str()),
            None => String::new(),
        },
        id: o.artifact_id,
        title: o.title,
        detail: o.detail,
    }
}

/// Append `value` to a facet row if the store did not report it. `count` is 0
/// because the two reasons it is missing — nothing carries it, or it was
/// crowded out of the top `FACET_LIMIT` — are not distinguishable from here;
/// the template renders no number rather than a wrong one.
fn ensure_facet(row: &mut Vec<crate::vector::FacetCount>, value: &str) {
    if value.is_empty() || row.iter().any(|f| f.value == value) {
        return;
    }
    row.push(crate::vector::FacetCount {
        value: value.to_string(),
        count: 0,
    });
}

#[derive(serde::Deserialize)]
struct UiSearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    category: Option<String>,
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

async fn search_results(
    State(st): State<AppState>,
    id: Identity,
    Query(p): Query<UiSearchParams>,
) -> Result<Response> {
    // Clearing the box fires a request with an empty query. That is not an
    // error; it just means there is nothing to show.
    if p.q.trim().is_empty() {
        return Ok(HtmlTemplate(ResultsTemplate {
            results: vec![],
            associated: vec![],
            all_weak: false,
            terms: String::new(),
        })
        .into_response());
    }

    // The same terms the sparse branch derives, handed to the client so
    // highlighting never has to touch the sanitized HTML on this side.
    // Function words are dropped: a query phrased as a situation is mostly
    // stopwords, and highlighting every "to" marks the whole card.
    let terms = highlightable_terms(p.q.trim());
    // What this sitting is working on. A typing burst folds into one entry
    // here as it does in the log, so what is carried is the query that was
    // meant rather than every prefix of it.
    if let Some(sess) = &id.session {
        st.core.sittings.queried(
            sess,
            p.q.trim(),
            crate::store::now(),
            st.core.pursuit.idle_secs as i64,
        );
    }
    let (hits, t) = st
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
            },
            Some(crate::core::search::MAX_PER_CORPUS),
            // Scoped to the operator, because coalescing folds a keystroke into
            // the query it was an early spelling of, and two people typing at
            // once are not spelling the same thing.
            crate::store::feedback::Door::Ui
                .by(id.subject)
                // The live sitting, for priming. Off unless `sitting.prime` is
                // on, and impossible at any door with no session.
                .in_sitting(id.session.clone()),
        )
        .await?;

    // The ranked answer and what it recalled are two lists on the page, and one
    // list here: an associated hit carries the id of the hit that recalled it,
    // and the title is looked up among the ranked ones rather than fetched.
    let titles = ranked_titles(&hits);
    let (ranked, recalled): (Vec<_>, Vec<_>) = hits.into_iter().partition(|h| h.via.is_none());
    let results: Vec<RenderedResult> = ranked
        .into_iter()
        .enumerate()
        .map(|(i, h)| render_hit(i, h, &titles))
        .collect();
    let associated: Vec<RenderedResult> = recalled
        .into_iter()
        .map(|h| render_hit(0, h, &titles))
        .collect();
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
    })
    .into_response();
    // Measured as before, reported where a browser already knows to show it.
    // On the page it was a line of debug telemetry floated beside the results
    // — a number nobody searching has a use for, in a place the eye lands.
    if let Ok(v) = format!("embed;dur={}, total;dur={}", t.embed_ms, t.total_ms).parse() {
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

fn render_hit(
    position: usize,
    h: crate::core::search::SearchResult,
    titles: &std::collections::HashMap<String, String>,
) -> RenderedResult {
    RenderedResult {
        artifact_id: h.artifact_id,
        // Empty, never "Untitled": a verbatim passage has no title by design,
        // and a rail of "Untitled" headings is a column of a word that says
        // nothing where a name would say something. The row shows its snippet.
        title: h.title.unwrap_or_default(),
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
        past_cliff: h.past_cliff,
        via_title: h.via.as_ref().and_then(|v| titles.get(v).cloned()),
        reason: h.reason.clone(),
        model_written: h.model_written,
        origin_count: h.origin_count,
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

/// The same repair as `disambiguate_labels`, for the pair cards.
///
/// A pair names two artifacts, so a page of pairs has two columns of titles
/// that can collide, and on the deployment they did: three of five cards read
/// `… vs LevelDB: Funktionsweise und forensische Analyse` because three
/// distinct artifacts carried that one name. Each side keeps its opening words
/// only where its title is shared, for the reason the queue keeps them — a
/// suffix on a name that needs no suffix is noise.
fn disambiguate_pair_titles(rows: &mut [PairRow]) {
    // By distinct artifact, never by how often a title appears. One artifact
    // against three others — which is what a cluster looks like from here —
    // puts its name on three rows without anything colliding: it is the same
    // artifact each time, and a qualifier on it would say that three rows are
    // about different things when they are about one. A title collides when
    // two different ids carry it.
    let mut ids: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for r in rows.iter() {
        ids.entry(r.a_title.as_str())
            .or_default()
            .insert(r.a_id.as_str());
        ids.entry(r.b_title.as_str())
            .or_default()
            .insert(r.b_id.as_str());
    }
    let collides: std::collections::HashSet<String> = ids
        .into_iter()
        .filter(|(_, seen)| seen.len() > 1)
        .map(|(t, _)| t.to_string())
        .collect();
    for r in rows.iter_mut() {
        let a_collides = collides.contains(&r.a_title);
        let b_collides = collides.contains(&r.b_title);
        if !(a_collides && !r.a_opening.is_empty() && r.a_opening != r.a_title) {
            r.a_opening.clear();
        }
        if !(b_collides && !r.b_opening.is_empty() && r.b_opening != r.b_title) {
            r.b_opening.clear();
        }
    }
}

async fn queue_fragment(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let mut rows = Vec::new();
    let corpora = st.core.store.list_corpora(10, 0).await?;
    // Asked once for the page rather than once per row: this fragment is polled
    // while anything is in flight, and the coverage read is a three-way join.
    // Failure is the empty map for the reason a missing capture is: the line is
    // what a capture did beyond being stored, and a page that cannot say so
    // says nothing rather than failing to render the queue.
    let covered = st
        .core
        .store
        .gaps_covered_by_each(&corpora.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
        .await
        .unwrap_or_default();
    for s in corpora {
        let (resolved, total) = st.core.store.segment_progress(&s.id).await?;
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
            label: s.title_hint.clone().unwrap_or_else(|| {
                if s.raw_text.is_empty() && s.origin == crate::core::ingest::ORIGIN_IMAGE {
                    "photo".into()
                } else if s.raw_text.is_empty() && s.origin == crate::core::ingest::ORIGIN_PDF {
                    // A PDF has no opening words until the extraction lands.
                    // Without this the row renders an empty anchor: nothing to
                    // read and nothing to click through to the corpus.
                    "document".into()
                } else {
                    markdown::snippet(&s.raw_text, 60)
                }
            }),
            unnamed: s.title_hint.is_none() && in_flight,
            in_flight,
            settled: matches!(s.status, CorpusStatus::Ready),
            badge: status_badge(&s.status),
            status: s.status.as_str().to_string(),
            artifact_count: st.core.store.count_artifacts_for_corpus(&s.id).await?,
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

/// Which lines to highlight, when the page was opened from an artifact that
/// claims them. Absent for an ordinary visit, which highlights nothing.
#[derive(serde::Deserialize, Default)]
struct LineRange {
    from: Option<i64>,
    to: Option<i64>,
}

/// Whether a capture's coverage is final — whether what no artifact carried is
/// a loss rather than a window nobody has read yet.
///
/// `synthesize::plan` writes every window up front in state `pending`, so a
/// capture still being read has segment rows and no artifacts for most of them.
/// Measured then, every unread line looks uncovered, and the page said so: it
/// named lines that were about to arrive as never reached, and offered to pay
/// for reading them a second time.
///
/// These are the states synthesis sets once every window has resolved. `partial`
/// and `failed` are in the list on purpose — they are where a real loss lives,
/// and gating on `ready` alone would hide the section from exactly the captures
/// that have something to show it.
fn coverage_final(status: &CorpusStatus) -> bool {
    matches!(
        status,
        CorpusStatus::Ready | CorpusStatus::Partial | CorpusStatus::Failed
    )
}

#[derive(serde::Deserialize)]
struct RereadForm {
    /// The band the button sits in. Both ends, because a passage nothing was
    /// written from does not stop at a window boundary, and matching on the
    /// first line alone re-read the window the loss opened in and left the
    /// rest of it exactly as it was.
    from: i64,
    to: i64,
}

/// Read one passage again.
///
/// The window holding that line, not the line itself: a window is wider than
/// the passage, and that is what lets the model read it in its surroundings
/// rather than stripped of them. One model call.
///
/// Nothing already written from this capture is replaced. What comes back is
/// added, and anything it repeats is folded by the dedupe sweep like any other
/// near duplicate.
///
/// The range is what the band said, not what it is: the form carries no token,
/// and taking `from`/`to` at their word let one POST of `from=1&to=999999` —
/// hand-edited, replayed, or arriving from another page in the operator's
/// session — reset and re-enqueue every window of the capture, one paid model
/// call each. So the bands are cut again here, and only a window holding a
/// passage that really is a loss, and really is inside the band pressed, is
/// queued.
async fn reread_uncovered_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Form(f): Form<RereadForm>,
) -> Result<Response> {
    // Back to the band the button was in. On a nine-hundred-line document,
    // returning to the top after pressing something two thirds of the way down
    // loses the reader's place for no reason.
    let back = Redirect::to(&format!("/ui/corpora/{cid}#L{}", f.from)).into_response();

    // A page left open while the capture was still being read would otherwise
    // offer to re-read lines that are merely not written yet.
    let s = st.core.store.get_corpus(&cid).await?;
    if !coverage_final(&s.status) {
        return Ok(back);
    }

    // The same cut the page renders, from the same inputs — and the same two
    // reasons it renders nothing red: a restored placeholder's text is its own
    // artifacts, and an artifact naming no lines may have come from exactly the
    // lines about to be re-read.
    let chunks = st.core.store.artifacts_for_corpus(&cid).await?;
    if s.restored_at.is_some() || chunks.iter().any(|c| c.corpus_span.is_none()) {
        return Ok(back);
    }
    let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = chunks
        .iter()
        .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
        .collect();
    let lost: Vec<(i64, i64)> = crate::web::corpus_view::bands(&s.raw_text, &spans, None)
        .into_iter()
        .filter(|b| b.gap() && b.from <= f.to && f.from <= b.to)
        .map(|b| (b.from, b.to))
        .collect();
    if lost.is_empty() {
        return Ok(back);
    }

    let segments = st.core.store.segments_for_corpus(&cid).await?;
    for w in segments.iter().filter(|w| {
        lost.iter()
            .any(|(a, z)| w.start_line <= *z && *a <= w.end_line)
    }) {
        // A window something is already going to read is left alone. `enqueue`
        // re-arms a conflicting row whatever state it is in, running included,
        // so pressing this twice handed the same window to a second worker: two
        // paid model calls and two sets of artifacts for one passage, then the
        // dedupe sweep to clean up after them.
        if st
            .core
            .store
            .live_job(
                crate::store::jobs::Stage::SegmentWindow,
                &crate::jobs::window::unit_target(&cid, w.idx),
            )
            .await?
        {
            continue;
        }
        // `true`: this window was read correctly and missed lines, so it is
        // being added to rather than replaced. Deleting what it already wrote
        // would throw away artifacts that may have been edited, tagged or
        // verified since, for lines that were never the problem.
        st.core.store.reset_segment(&cid, w.idx, true).await?;
        st.core
            .store
            .enqueue(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(&cid, w.idx),
            )
            .await?;
    }
    Ok(back)
}

#[derive(serde::Deserialize)]
struct DwellForm {
    #[serde(default)]
    secs: i64,
}

/// The page saying how long an artifact was open, sent as the reader leaves
/// it. `sendBeacon` lands here; nothing is rendered back.
async fn artifact_dwell(
    State(st): State<AppState>,
    id: Identity,
    Path(aid): Path<String>,
    Form(f): Form<DwellForm>,
) -> Result<Response> {
    st.core.record_dwell(&aid, f.secs, Some(&id.subject));
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// Undo a promotion: the window's passages back in results, what the
/// promotion wrote retired, the window `verbatim` again.
async fn unpromote_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path((cid, idx)): Path<(String, i64)>,
) -> Result<Response> {
    st.core.undo_promotion(&cid, idx).await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}

async fn corpus_detail(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Query(range): Query<LineRange>,
) -> Result<Response> {
    let s = st.core.store.get_corpus(&cid).await?;
    let chunks = st.core.store.artifacts_for_corpus(&cid).await?;
    let restored = s.restored_at.is_some();

    // A restored placeholder's text is its own artifacts joined back together,
    // so a span into it points at an artifact rather than at a source. Banding
    // it would be a claim that arrangement cannot support; it keeps the flat
    // rendering, and the warning above it already says why.
    let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = if restored {
        Vec::new()
    } else {
        chunks
            .iter()
            .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
            .collect()
    };
    let by_id: std::collections::HashMap<&str, &crate::store::artifacts::Chunk> =
        chunks.iter().map(|c| (c.id.as_str(), c)).collect();

    // An artifact that names no lines was written from somewhere in this
    // capture without saying where: a row from before spans were recorded,
    // anything created outside `window::run`, and every artifact of a restored
    // placeholder. Banding cannot place it, and rendering only bands dropped it
    // off the page altogether — off the only page that can edit or delete it.
    // It gets a section of its own below the source instead.
    let unplaced: Vec<ArtifactView> = chunks
        .iter()
        .filter(|c| restored || c.corpus_span.is_none())
        .map(artifact_view)
        .collect();

    let segments = st.core.store.segments_for_corpus(&cid).await?;
    // Until the capture has finished being read, a passage nothing claims is
    // a passage nothing has got to yet. Banded, still — the arrangement is how
    // the page reads — but not red, and not offering to re-read what is
    // already on its way.
    //
    // And nothing is a loss while an artifact of this capture names no lines:
    // it may well have been written from exactly the lines about to be painted
    // red, and the page would be offering to pay to read them again on the
    // strength of a claim it cannot make. `unplaced` says so in words instead.
    let losses_are_final = coverage_final(&s.status) && !segments.is_empty() && unplaced.is_empty();

    // Every row still carries its `L<n>` anchor, inside its band: an artifact's
    // "open at these lines" and the `?from=&to=` highlight both address lines
    // by that id, and banding must not cost the page either of them.
    //
    // An artifact whose span overlaps another's claims every band the overlap
    // cuts, and its card belongs to the first of them. Rendered in each, the
    // page carried the same artifact three times under one set of element ids:
    // "edit" on the second copy opened the editor attached to the first, and
    // delete swapped the first away and left the others behind pointing at a
    // row that no longer exists.
    let mut carded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bands: Vec<BandView> = if restored {
        Vec::new()
    } else {
        crate::web::corpus_view::bands(
            &s.raw_text,
            &spans,
            range.from.map(|f| (f, range.to.unwrap_or(f))),
        )
        .into_iter()
        .map(|b| {
            // Split before the band is built: an artifact gets its card in the
            // first band that claims it, and a line pointing up at that card in
            // every later one.
            let (mut artifacts, mut echoes) = (Vec::new(), Vec::new());
            for c in b
                .artifact_ids
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
            {
                if carded.insert(c.id.clone()) {
                    artifacts.push(artifact_view(c));
                } else {
                    echoes.push((c.id.clone(), artifact_title(c)));
                }
            }
            BandView {
                // What pressing the button would actually read: the window holding
                // this passage, which is wider than it. Saying only "lines 51–53"
                // over a button that reads 1–120 is a promise it does not keep —
                // and a second red band inside the same window really is read too.
                reread: (b.gap() && losses_are_final)
                    .then(|| {
                        segments
                            .iter()
                            .filter(|w| w.start_line <= b.to && b.from <= w.end_line)
                            .fold(None::<(i64, i64)>, |acc, w| {
                                Some(match acc {
                                    Some((a, z)) => (a.min(w.start_line), z.max(w.end_line)),
                                    None => (w.start_line, w.end_line),
                                })
                            })
                            .map(|(a, z)| format!("reads lines {a}–{z}"))
                    })
                    .flatten(),
                gap: b.gap() && losses_are_final,
                from: b.from,
                to: b.to,
                artifacts,
                // The overlap is still on the page: both artifacts do claim these
                // lines, and a band that silently dropped one of them would read as
                // if only the other did.
                echoes,
                lines: b.lines,
            }
        })
        .collect()
    };

    // Stated whether or not anything is red, because the warning on Recent is
    // computed the other way round: a corpus can be 55% covered with every
    // line claimed, and following that warning has to land somewhere that
    // explains itself rather than on a page with nothing marked.
    let coverage = s.coverage.map(|c| format!("{:.0}%", c * 100.0));
    let image = s.origin == crate::core::ingest::ORIGIN_IMAGE;
    let pdf = s.origin == crate::core::ingest::ORIGIN_PDF;
    let unread = (image && (s.status == CorpusStatus::Describing || s.raw_text.trim().is_empty()))
        || (pdf && (s.status == CorpusStatus::Extracting || s.raw_text.trim().is_empty()));
    let note = s.metadata["note"].as_str().map(str::to_string);
    let meta_rows = metadata_rows(&s.metadata);
    let exif_rows = exif_tag_rows(&s.metadata);
    let written_from: Vec<ArtifactView> = st
        .core
        .store
        .artifacts_originating_in(&cid)
        .await?
        .iter()
        .filter(|c| c.in_results())
        .map(artifact_view)
        .collect();
    // A promoted window: `done`, and owning at least one superseded passage.
    let promoted: Vec<PromotedWindow> = segments
        .iter()
        .filter(|w| w.state == crate::store::segments::SegmentState::Done)
        .filter(|w| {
            chunks.iter().any(|c| {
                c.segment_idx == Some(w.idx)
                    && c.provenance == crate::store::artifacts::Provenance::Passage
                    && c.superseded_by.is_some()
            })
        })
        .map(|w| PromotedWindow {
            idx: w.idx,
            from: w.start_line,
            to: w.end_line,
        })
        .collect();
    Ok(HtmlTemplate(CorpusTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        id: s.id,
        badge: status_badge(&s.status),
        status: s.status.as_str().to_string(),
        restored,
        source_url: s.source_url.clone(),
        image,
        pdf,
        unread,
        meta_rows,
        exif_rows,
        note,
        bands,
        promoted,
        unplaced,
        written_from,
        lines_empty: s.raw_text.trim().is_empty(),
        raw_text: s.raw_text.clone(),
        coverage,
    })
    .into_response())
}

/// Everything under `exif.tags`, by name, sorted. The named facts above have
/// their own rows; this is the rest of what the camera wrote, in a block that
/// starts folded — the original file is not kept, so the page is the only place
/// left to read it, and it is still nothing anyone opened the page to see.
fn exif_tag_rows(m: &serde_json::Value) -> Vec<(String, String)> {
    let Some(tags) = m["exif"]["tags"].as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<(String, String)> = tags
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// The metadata worth a row on the corpus page, in reading order. Everything
/// else the file carried is under `exif.tags`, folded away below.
fn metadata_rows(m: &serde_json::Value) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let exif = &m["exif"];
    if let Some(t) = exif["taken_at"].as_str() {
        rows.push(("Taken".into(), t.into()));
    }
    if let Some(c) = exif["camera"].as_str() {
        rows.push(("Camera".into(), c.into()));
    }
    if let (Some(lat), Some(lon)) = (exif["gps"]["lat"].as_f64(), exif["gps"]["lon"].as_f64()) {
        rows.push(("Location".into(), format!("{lat}, {lon}")));
    }
    let f = &m["file"];
    if let Some(n) = f["name"].as_str() {
        rows.push(("File".into(), n.into()));
    }
    if let (Some(w), Some(h)) = (f["width"].as_u64(), f["height"].as_u64()) {
        rows.push(("Size".into(), format!("{w}×{h}")));
    }
    if let Some(e) = m["describe"]["error"].as_str() {
        rows.push(("Reading".into(), e.into()));
    }
    if let Some(e) = m["extract"]["error"].as_str() {
        rows.push(("Extraction".into(), e.into()));
    }
    rows
}

#[derive(serde::Deserialize)]
struct ArtifactEditForm {
    text: String,
    /// Which shape to answer with. Two screens edit an artifact and they are
    /// not the same size: the corpus page swaps one card in a list, the detail
    /// pane swaps the whole pane. Answering both with a card replaced the pane
    /// — source, lineage and neighbours included — with a list row.
    #[serde(default)]
    view: String,
    /// The search terms the pane was opened with, so the highlight survives a
    /// save. Empty everywhere else.
    #[serde(default)]
    terms: String,
}

async fn put_artifact(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Form(f): Form<ArtifactEditForm>,
) -> Result<Response> {
    if f.text.trim().is_empty() {
        return Err(Error::Validation("chunk text is empty".into()));
    }
    st.core.store.update_artifact_text(&cid, &f.text).await?;
    // The stored vector describes wording that no longer exists.
    st.core
        .store
        .enqueue(crate::store::jobs::Stage::Embed, "artifact", &cid)
        .await?;
    if f.view == "detail" {
        let d = build_artifact_detail(&st.core, &cid, &f.terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    let c = st.core.store.get_artifact(&cid).await?;
    Ok(HtmlTemplate(ArtifactFragment {
        c: artifact_view(&c),
    })
    .into_response())
}

async fn delete_corpus_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    st.core.delete_corpus(&cid).await?;
    Ok(Redirect::to("/ui/capture").into_response())
}

/// Remove an artifact from both stores, from the page that shows it.
///
/// The deliberate counterpart to what `Core::heal_store_drift` stopped doing on
/// its own. A background pass cannot tell an artifact deleted on purpose from
/// one whose row a crash lost, so it now restores both and this button is the
/// only thing that removes anything — a person who can see the artifact deciding
/// it should go.
///
/// Two callers, two right answers. Pressed in a list — a search result, a card
/// on the source page — the answer is nothing at all: htmx swaps the row that
/// was pressed out of the list, and the page the operator was reading stays
/// where it was. Pressed in the pane, where the whole view *is* the artifact,
/// there is nothing left to stay on, so it lands on the source.
///
/// An empty 200 rather than a 204: htmx treats no-content as "swap nothing",
/// which would leave the deleted artifact on screen until a reload.
async fn delete_artifact_ui(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
) -> Result<Response> {
    let corpus_id = st.core.store.get_artifact(&aid).await?.corpus_id;
    st.core.delete_artifact(&aid).await?;
    if headers.contains_key("hx-request") {
        return Ok(axum::response::Html(String::new()).into_response());
    }
    // A merged artifact has no document to return to, so the artifact list is
    // where deleting one leaves you.
    Ok(match corpus_id {
        Some(cid) => Redirect::to(&format!("/ui/corpora/{cid}")).into_response(),
        None => Redirect::to("/ui/ops").into_response(),
    })
}

#[derive(serde::Deserialize, Default)]
struct ReprocessForm {
    #[serde(default)]
    stage: Option<String>,
}

/// Re-segment by default; `stage=describe` re-reads a captured image and
/// `stage=extract` re-reads a captured PDF.
async fn reprocess_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Form(form): Form<ReprocessForm>,
) -> Result<Response> {
    let stage = match form.stage {
        None => crate::store::jobs::Stage::Synthesize,
        Some(s) => crate::store::jobs::Stage::parse(&s)
            .ok_or_else(|| Error::Validation(format!("unknown stage `{s}`")))?,
    };
    st.core.reprocess(&cid, stage).await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}

/// What to call an artifact in a place that must call it something.
///
/// A title is what makes two near-identical artifacts tellable apart at a
/// glance; falling back to the opening of the body beats an id. Sixty
/// characters of raw body was that fallback, and it is where the sitting's
/// "…darin vo" and the dedupe queue's `Keep "- schneller Schreibzugriff …"`
/// both came from. The rule lives in one place now — see
/// `markdown::stand_in_title` — so the sitting, the pair cards and the judge
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

/// How many decisions Capture offers at once, and the order it looks for them
/// in. Confirmed contradictions and judge-proposed supersedes lead: they are
/// the ones that mean something in the base is wrong or stale, rather than
/// merely repeated.
///
/// A rolling window rather than the whole backlog. This is now the app's start
/// page, so every open paid for three fifty-row queries and two point lookups
/// per pair, and a base with real overlap in it rendered a screen of warning
/// boxes above the captures. Deciding one of these makes the next appear, so
/// the cap strands nothing — there is no second page to go and find the rest
/// on, which is the point: Housekeeping is reference, not work.
const PAIR_LIMIT: usize = 5;
const PAIR_STATES: [crate::store::pairs::PairState; 3] = [
    crate::store::pairs::PairState::Contradiction,
    crate::store::pairs::PairState::Superseded,
    crate::store::pairs::PairState::Pending,
];

/// The first `PAIR_LIMIT` pairs still waiting on a judgement, and how many more
/// there are behind them.
///
/// Used by Capture, which shows them because that is where the work arrives,
/// and by nothing else: Housekeeping is what is left over once the only part of
/// Ops that needs a person has moved to the page people actually open.
async fn pair_rows(st: &AppState) -> Result<(Vec<PairRow>, i64)> {
    let mut waiting = 0i64;
    for state in PAIR_STATES {
        waiting += st.core.store.count_pairs_by_state(state).await?;
    }

    let mut pairs = Vec::new();
    'fill: for state in PAIR_STATES {
        for p in st
            .core
            .store
            .pairs_by_state(state, PAIR_LIMIT as i64)
            .await?
        {
            let (Ok(a), Ok(b)) = (
                st.core.store.get_artifact(&p.a_id).await,
                st.core.store.get_artifact(&p.b_id).await,
            ) else {
                continue;
            };
            let obsolete_title = p.obsolete_id.as_deref().map(|id| {
                if id == a.id {
                    title_of(&a)
                } else {
                    title_of(&b)
                }
            });
            // Keeping one side is superseding the other, so the judge naming
            // `a` obsolete is a recommendation to keep `b`.
            let keeps_a = p.obsolete_id.as_deref() == Some(b.id.as_str());
            let keeps_b = p.obsolete_id.as_deref() == Some(a.id.as_str());
            // A score of exactly zero is what "no cosine was ever measured"
            // looks like in the row: the link judge's `duplicate` verdict files
            // the pair with one (`src/jobs/associate.rs`), and the similarity
            // sweep — the only other producer of a pending pair — files the
            // cosine it found, which cleared `consolidate.review_min` to get
            // there (`src/jobs/relate.rs:68`).
            //
            // That gate is `>=` and `review_min` has no lower bound of its own
            // — only `auto_supersede > review_min` is enforced — so an operator
            // who sets it to zero could in principle file a pair measured at
            // exactly 0.0, and this would call it unmeasured. It takes an exact
            // float zero out of a real embedding to get there, which is why the
            // marker is left implicit; if that ever stops being true the fix is
            // an explicit `origin` column, not a smaller epsilon.
            //
            // Not `detail == "link"`, which is only the *initial* detail: the
            // dedupe judge's `set_pair_state` and `set_pair_superseded`
            // (`src/store/pairs.rs`) both write their own prose over that
            // field, so a marker read out of it survives only while the pair is
            // pending. The score is never rewritten.
            let via_link = p.score == 0.0;
            // The bare marker, on the other hand, *is* read out of `detail` —
            // it is the whole of that field only while the pair is pending, and
            // that is exactly when there is no judge's line to lose. Once one
            // has been written the prose is what the reader needs; the score
            // above still keeps the page from calling it a measurement.
            let detail = if p.detail.as_deref() == Some("link") {
                Some(
                    "Not found by similarity: these two kept being retrieved together, \
                     and the judge then found they say the same thing."
                        .to_string(),
                )
            } else {
                p.detail
            };
            pairs.push(PairRow {
                id: p.id,
                percent: (p.score * 100.0).round() as i64,
                a_title: title_of(&a),
                b_title: title_of(&b),
                // Kept whether or not it is shown; `disambiguate_pair_titles`
                // clears the ones the page does not need.
                a_opening: crate::web::markdown::stand_in_title(&a.text, 40),
                b_opening: crate::web::markdown::stand_in_title(&b.text, 40),
                a_excerpt: crate::web::markdown::snippet(&a.text, 400),
                b_excerpt: crate::web::markdown::snippet(&b.text, 400),
                a_id: p.a_id,
                b_id: p.b_id,
                detail,
                via_link,
                contradiction: state == crate::store::pairs::PairState::Contradiction,
                obsolete_title,
                keeps_a,
                keeps_b,
            });
            if pairs.len() == PAIR_LIMIT {
                break 'fill;
            }
        }
    }

    // Counted from the states rather than from the rows, so a pair whose
    // artifacts have since gone missing — skipped above — is not announced as
    // something waiting that never appears.
    let more = (waiting - pairs.len() as i64).max(0);
    disambiguate_pair_titles(&mut pairs);
    Ok((pairs, more))
}

/// One decision, however many pairs it takes to state it.
pub struct PairCluster {
    pub pairs: Vec<PairRow>,
    /// How many distinct artifacts the cluster names, so the card can say what
    /// it is asking about before the rows do.
    pub members: usize,
}

/// Group the open pairs into the clusters they actually describe.
///
/// The same disjoint-set `jobs::consolidate` runs before it settles anything,
/// and for the same reason it gives: resolving pairs one at a time does not
/// work, and the way it fails is quiet. Here the failure is the operator's
/// rather than the base's — one artifact against three others arrived as three
/// separate questions, 90%, 90% and 88% alike, and answering one of them left
/// the other two on the page looking identical to the one just answered.
///
/// Order is the incoming order, which is `PAIR_STATES`' priority: the cluster
/// containing the most urgent pair leads, and within a cluster the rows keep
/// the order they were read in.
fn group_pairs(pairs: Vec<PairRow>) -> Vec<PairCluster> {
    let mut parent: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    fn find(parent: &mut std::collections::HashMap<String, String>, x: &str) -> String {
        let p = parent.get(x).cloned().unwrap_or_else(|| x.to_string());
        if p == x {
            return p;
        }
        let root = find(parent, &p);
        parent.insert(x.to_string(), root.clone());
        root
    }
    for r in &pairs {
        let (ra, rb) = (find(&mut parent, &r.a_id), find(&mut parent, &r.b_id));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    let mut order: Vec<String> = Vec::new();
    let mut by_root: std::collections::HashMap<String, Vec<PairRow>> =
        std::collections::HashMap::new();
    for r in pairs {
        let root = find(&mut parent, &r.a_id);
        if !by_root.contains_key(&root) {
            order.push(root.clone());
        }
        by_root.entry(root).or_default().push(r);
    }

    order
        .into_iter()
        .filter_map(|root| {
            let pairs = by_root.remove(&root)?;
            let members: std::collections::HashSet<&str> = pairs
                .iter()
                .flat_map(|r| [r.a_id.as_str(), r.b_id.as_str()])
                .collect();
            Some(PairCluster {
                members: members.len(),
                pairs,
            })
        })
        .collect()
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
fn ends_mid_sentence(text: &str) -> bool {
    let t = text.trim_end();
    let t = t.trim_end_matches([')', ']', '"', '»', '\'', '“', '”']);
    match t.chars().last() {
        None => false,
        // A table row or a fence closes on its own punctuation.
        Some('|') | Some('`') => false,
        Some(c) => !matches!(c, '.' | '!' | '?' | ':' | ';' | '…'),
    }
}

/// The API tokens, formatted for a table.
async fn token_rows(st: &AppState) -> Result<Vec<TokenRow>> {
    Ok(st
        .core
        .store
        .list_tokens()
        .await?
        .into_iter()
        .map(|t| TokenRow {
            id: t.id,
            name: t.name,
            created: fmt_time(t.created_at),
            last_used: t
                .last_used_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".into()),
            // What asked for it. Two tokens can carry one name — the extension
            // gives every token it mints the same one — and when neither has
            // been used yet, this is the only thing that differs.
            minted_by: t.user_agent.clone().unwrap_or_else(|| "—".into()),
            revoked: t.revoked_at.is_some(),
        })
        .collect())
}

/// What is true about this installation, as opposed to what is in it.
///
/// Split off Housekeeping, which had grown to hold six tables about the corpus
/// plus the extension, the tokens and the feedback purge — so revoking a token
/// meant scrolling past every merge and every hidden artifact first. Reached
/// from the same quiet line under Capture, and no more advertised than
/// Housekeeping is: neither belongs in a top row that is three destinations
/// wide on purpose.
async fn settings(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    Ok(HtmlTemplate(SettingsTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        tokens: token_rows(&st).await?,
        feedback: match st.core.feedback.enabled {
            true => Some(st.core.store.feedback_stats().await?),
            false => None,
        },
        asks: match st.core.feedback.enabled {
            true => Some(st.core.store.ask_stats().await?),
            false => None,
        },
    })
    .into_response())
}

/// Rows of one housekeeping table before it says there are more.
///
/// These tables are read to answer "what happened to X", and the answer to
/// that is a search for X rather than a scroll — so the cap is stated and the
/// rest arrive as these are cleared, instead of growing a pager nobody would
/// page through.
const TABLE_CAP: i64 = 25;

async fn ops(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use sqlx::Row;

    let artifact_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&st.core.store.pool)
        .await?
        .get("n");

    // Not a queue of chores: work that hit something and is waiting to try
    // again on its own. Nothing here needs a person.
    let retrying: Vec<RetryingRow> = st
        .core
        .store
        .retrying_jobs(50)
        .await?
        .into_iter()
        .map(|j| RetryingRow {
            stage: j.stage,
            target_id: j.target_id,
            attempts: j.attempts,
            due: fmt_duration(j.next_attempt_secs),
            last_error: j.last_error.unwrap_or_else(|| "—".into()),
        })
        .collect();

    // A parked capture is the one corpus state no worker advances. It has to be
    // shown here or it sits unprocessed with nothing saying why.
    let mut parked = Vec::new();
    for c in st.core.store.parked_corpora(50).await? {
        let other_id = c.near_dupe_of.clone().unwrap_or_default();
        let other_title = match st.core.store.get_corpus(&other_id).await {
            Ok(o) => o.title_hint.unwrap_or_else(|| "untitled".into()),
            Err(_) => "(deleted)".into(),
        };
        parked.push(ParkedRow {
            percent: (c.near_dupe_score.unwrap_or(0.0) * 100.0).round() as i64,
            bytes: c.raw_text.len(),
            title: c.title_hint.clone().unwrap_or_else(|| "untitled".into()),
            id: c.id,
            other_id,
            other_title,
        });
    }

    let mut superseded = Vec::new();
    // One past the cap, so the page can say it is capped rather than truncate
    // in silence — a table that stops at 25 with nothing said reads as a table
    // of everything there is.
    for c in st.core.store.superseded_artifacts(TABLE_CAP + 1).await? {
        let winner_id = c.superseded_by.clone().unwrap_or_default();
        let winner_title = match st.core.store.get_artifact(&winner_id).await {
            Ok(w) => title_of(&w),
            Err(_) => "(deleted)".to_string(),
        };
        superseded.push(SupersededRow {
            title: title_of(&c),
            subtitle: row_subtitle(&c),
            id: c.id,
            winner_id,
            winner_title,
        });
    }

    let mut merged = Vec::new();
    let merged_chunks = st.core.store.merged_artifacts(TABLE_CAP + 1).await?;
    // One lineage call per page, not one per row: `roots_of` takes the batch.
    let merged_ids: Vec<String> = merged_chunks.iter().map(|c| c.id.clone()).collect();
    let roots = st
        .core
        .store
        .roots_of(&merged_ids)
        .await
        .unwrap_or_default();
    for c in merged_chunks {
        let sources = source_rows(
            &st.core.store,
            &c.id,
            roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        merged.push(MergedRow {
            orphaned: c.flags.iter().any(|f| f == "orphaned_source"),
            title: title_of(&c),
            subtitle: row_subtitle(&c),
            id: c.id,
            sources,
        });
    }

    let more_merged = merged.len() > TABLE_CAP as usize;
    merged.truncate(TABLE_CAP as usize);

    let mut generated = Vec::new();
    let gen_chunks = st.core.store.synthesized_artifacts(TABLE_CAP).await?;
    let gen_ids: Vec<String> = gen_chunks.iter().map(|c| c.id.clone()).collect();
    let gen_roots = st.core.store.roots_of(&gen_ids).await.unwrap_or_default();
    for c in gen_chunks {
        let sources = source_rows(
            &st.core.store,
            &c.id,
            gen_roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        generated.push(GeneratedRow {
            title: title_of(&c),
            subtitle: row_subtitle(&c),
            cues: c.cues.clone(),
            id: c.id,
            sources,
        });
    }
    let pursuit_enabled = st.core.pursuit.enabled;
    let recent = match pursuit_enabled {
        true => st.core.store.recent_pursuits(50).await?,
        false => Vec::new(),
    };
    let pursuit_recent = recent.len();
    // The ones the sentence below can honestly point at. `unsatisfied` is how a
    // run of searches *ended*, and a capture that answers one afterwards leaves
    // that word alone deliberately — coverage never rewrites what happened — so
    // counting the state sent the operator to a gap list that had already
    // dropped half of them.
    let on_the_gap_list = match pursuit_enabled {
        true => st
            .core
            .store
            .open_pursuit_gap_ids(st.core.embedder.model())
            .await
            .unwrap_or_default(),
        false => Default::default(),
    };
    let pursuit_unsatisfied = recent
        .iter()
        .filter(|p| p.state == "unsatisfied" && on_the_gap_list.contains(&p.id))
        .count();
    // What the memory did while nobody was looking. The last day as one
    // sentence, and under it the runs themselves — which is the half a single
    // overwritten summary could never give.
    let day = st
        .core
        .store
        .sweep_runs_since(crate::store::now() - 86_400, 500)
        .await
        .unwrap_or_default();
    let last_day_failures = day.iter().filter(|r| r.outcome == "failed").count();
    let mut totals: Vec<(String, i64)> = Vec::new();
    for r in &day {
        tally_sweep(&r.stage, &r.detail, &mut totals);
    }
    let last_day: Vec<SweepCount> = totals
        .into_iter()
        .map(|(what, n)| SweepCount { n, what })
        .collect();
    let sweep_history: Vec<SweepRunRow> = st
        .core
        .store
        .sweep_history(TABLE_CAP)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let mut counts = Vec::new();
            tally_sweep(&r.stage, &r.detail, &mut counts);
            SweepRunRow {
                when: fmt_time(r.started_at),
                error: match r.outcome == "failed" {
                    true => serde_json::from_str::<serde_json::Value>(&r.detail)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
                        .unwrap_or_else(|| "it failed".into()),
                    false => String::new(),
                },
                took: fmt_elapsed(r.ended_at - r.started_at),
                stage: sweep_label(&r.stage).to_string(),
                stage_id: r.stage,
                counts: counts
                    .into_iter()
                    .map(|(what, n)| SweepCount { n, what })
                    .collect(),
            }
        })
        .collect();

    let more_superseded = superseded.len() > TABLE_CAP as usize;
    superseded.truncate(TABLE_CAP as usize);

    let deprecated = st
        .core
        .store
        .artifacts_by_status(crate::store::artifacts::ArtifactStatus::Deprecated, 50)
        .await?
        .into_iter()
        .map(|c| DeprecatedRow {
            title: title_of(&c),
            id: c.id,
        })
        .collect();

    // Read-only candidates: nothing here has been changed, only listed.
    let stale = st
        .core
        .stale_candidates(50)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "no stale candidates for ops");
            vec![]
        })
        .into_iter()
        .map(|r| StaleRow {
            title: r.title.unwrap_or_else(|| markdown::snippet(&r.text, 60)),
            id: r.artifact_id,
            last_verified: r
                .last_verified_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".to_string()),
        })
        .collect();

    Ok(HtmlTemplate(OpsTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        retrying,
        parked,
        superseded,
        merged,
        more_merged,
        more_superseded,
        table_cap: TABLE_CAP,
        deprecated,
        stale,
        job_counts: st.core.store.job_counts().await?,
        oldest_pending_secs: st.core.store.oldest_pending_age().await?,
        artifact_count,
        // Qdrant being briefly unreachable must not blank the ops page, which
        // is exactly where you look when something is wrong.
        vector_count: st.core.vectors.count().await.unwrap_or(0),
        links: match st.core.associating() {
            true => Some(st.core.store.link_counts().await?),
            false => None,
        },
        generated,
        pursuit_enabled,
        pursuit_recent,
        pursuit_unsatisfied,
        last_day,
        last_day_failures,
        sweep_history,
        // The last month rather than the last day: a weekly pattern needs
        // weeks, so a hit rate measured over a day would be a number nobody
        // could act on. Read like `vector_count` — a failure here must not
        // blank the page you open when something is wrong.
        offer_rates: match st.core.recommends() {
            true => st
                .core
                .store
                .offer_rates(crate::store::now() - 30 * 86_400)
                .await
                .unwrap_or_default(),
            false => Vec::new(),
        },
    })
    .into_response())
}

/// Take a merge back: what it replaced returns, the merge is retired, and the
/// pairs behind it are dismissed so the sweep does not simply redo it.
async fn undo_merge_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(aid): Path<String>,
) -> Result<Response> {
    crate::jobs::merge::undo(&st.core, &aid).await?;
    Ok(Redirect::to("/ui/ops").into_response())
}

/// Forget every captured search and every recorded question.
///
/// Judgements go with them: a verdict is a statement about a query, and one
/// whose query no longer exists records nothing. Accepted settings and their
/// history stay, because they describe how the application is configured now.
///
/// Both tables, because one switch records both and `expire_feedback` ages both
/// under one window — but the questions are the harder loss, being the only
/// source `--export-eval` has for `questions.json`, so the button and its
/// confirmation name them rather than leaving them to the word "searches".
async fn purge_feedback_ui(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let n = st.core.store.purge_feedback().await?;
    tracing::info!(
        dropped = n,
        "captured searches and questions deleted by the operator"
    );
    // Back to the page the button is on. The route keeps its /ui/ops prefix —
    // the two pages split, the endpoints did not.
    Ok(Redirect::to("/ui/settings").into_response())
}

#[derive(serde::Deserialize)]
struct MintForm {
    name: String,
}

async fn mint_token(
    State(st): State<AppState>,
    id: Identity,
    headers: axum::http::HeaderMap,
    Form(f): Form<MintForm>,
) -> Result<Response> {
    let name = if f.name.trim().is_empty() {
        "unnamed"
    } else {
        f.name.trim()
    };
    let (_, plaintext) = crate::auth::tokens::mint(
        &st.core.store,
        name,
        &id.subject,
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
    )
    .await?;
    // Shown once, here, and never stored in plaintext anywhere.
    Ok(HtmlTemplate(TokenCreatedTemplate { token: plaintext }).into_response())
}

async fn revoke_token_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(tid): Path<String>,
) -> Result<Response> {
    crate::auth::tokens::revoke(&st.core.store, &tid).await?;
    Ok(Redirect::to("/ui/settings").into_response())
}

#[derive(serde::Deserialize)]
struct ResolveForm {
    action: crate::core::ingest::NearDupeAction,
}

async fn resolve_near_dupe_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Form(form): Form<ResolveForm>,
) -> Result<Response> {
    st.core.resolve_near_duplicate(&cid, form.action).await?;
    Ok(Redirect::to("/ui/ops").into_response())
}

/// Where a lifecycle button should land afterwards.
///
/// The same four actions are offered from two places: the Ops review lists,
/// where the queue is the thing being worked through, and an artifact's own
/// page, where being thrown onto Ops for pressing "Confirm still accurate"
/// loses the reader's place. The page that rendered the button says where it
/// leads; Ops sends nothing and keeps the default.
#[derive(serde::Deserialize, Default)]
struct ReturnTo {
    to: Option<String>,
}

impl ReturnTo {
    /// Only a path inside this UI. A form field is user input, and a redirect
    /// that will follow anything it is handed is an open redirect — worth
    /// nothing to the operator and a phishing hop to everyone else.
    fn path(&self) -> &str {
        match self.to.as_deref() {
            Some(p) if p.starts_with("/ui/") && !p.starts_with("/ui//") => p,
            _ => "/ui/ops",
        }
    }
}

/// What a lifecycle button answers with: the artifact it just changed.
///
/// These four buttons say something about an artifact, not about the page it is
/// on — so the answer is that artifact, re-rendered where it already was, and
/// nothing navigates. `_artifact_detail.html` is rendered in two places and the
/// hidden `to` beside each button can only name one of them: it named the
/// standalone artifact page, so pressing "Confirm still accurate" on a search
/// result took the whole window there and the results the operator was working
/// through were gone. That is the reason `ReturnTo` exists, arrived at from the
/// other side.
///
/// The redirect is still what a browser without htmx gets, and `to` is still
/// what it follows. Nothing here is the only way any of these buttons work.
async fn artifact_changed(
    st: &AppState,
    headers: &axum::http::HeaderMap,
    aid: &str,
    terms: &str,
    back: &ReturnTo,
) -> Result<Response> {
    if headers.contains_key("hx-request") {
        let d = build_artifact_detail(&st.core, aid, terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(Redirect::to(back.path()).into_response())
}

async fn unsupersede_ui(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.unsupersede(&aid).await?;
    artifact_changed(&st, &headers, &aid, &p.terms, &back).await
}

async fn dismiss_pair_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(pid): Path<i64>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core
        .store
        .set_pair_state(pid, crate::store::pairs::PairState::Dismissed, None)
        .await?;
    Ok(Redirect::to(back.path()).into_response())
}

/// Which artifact of a pair the operator is keeping. Absent means "whichever
/// the judge proposed", which is what the confirmation button on a proposed
/// supersede sends.
#[derive(serde::Deserialize, Default)]
struct KeepForm {
    keep: Option<String>,
    /// Pressed from Capture, these come back to Capture. Same reasoning as
    /// `ReturnTo`, which validates the path.
    #[serde(flatten)]
    back: ReturnTo,
}

/// Resolve a pair by naming the artifact that survives; the other is superseded
/// by it.
///
/// Two callers, one action. The judge's proposal is a suggestion an operator
/// confirms, and a contradiction the judge could not call is the same decision
/// with nobody suggesting anything — so both are "keep this one", and only the
/// default differs. Before this, a pair the judge flagged as disagreeing but
/// could not rule on offered nothing except Dismiss: the operator could see two
/// artifacts stating different things and had no way to say which was right,
/// so the only way out of the queue was to declare the disagreement uninteresting
/// and leave both in results.
///
/// Nothing before this press hides anything — see `jobs::consolidate::judge_pending`.
async fn apply_pair_supersede_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(pid): Path<i64>,
    Form(f): Form<KeepForm>,
) -> Result<Response> {
    let pair = st.core.store.get_pair(pid).await?;
    // The winner has to be one of this pair's own artifacts. A form field is
    // user input, and superseding an arbitrary id because it arrived in a POST
    // would hide an artifact that has nothing to do with the row that was
    // pressed.
    let obsolete_id = match f.keep {
        Some(keep) if keep == pair.a_id => pair.b_id.clone(),
        Some(keep) if keep == pair.b_id => pair.a_id.clone(),
        Some(_) => {
            return Err(crate::error::Error::Validation(
                "the artifact to keep is not part of this pair".into(),
            ));
        }
        None => pair
            .obsolete_id
            .clone()
            .ok_or(crate::error::Error::NotFound)?,
    };
    let winner_id = if obsolete_id == pair.a_id {
        pair.b_id
    } else {
        pair.a_id
    };
    st.core.supersede(&obsolete_id, &winner_id).await?;
    // The judge's explanation is carried through rather than dropped: it is the
    // only record of why this supersede was applied, and `set_pair_state`
    // writes `detail` unconditionally, so passing `None` would null it.
    st.core
        .store
        .set_pair_state(
            pid,
            crate::store::pairs::PairState::Dismissed,
            pair.detail.as_deref(),
        )
        .await?;
    Ok(Redirect::to(f.back.path()).into_response())
}

async fn deprecate_ui(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.deprecate(&aid).await?;
    artifact_changed(&st, &headers, &aid, &p.terms, &back).await
}

async fn reactivate_ui(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.reactivate(&aid).await?;
    artifact_changed(&st, &headers, &aid, &p.terms, &back).await
}

async fn verify_ui(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.verify(&aid).await?;
    artifact_changed(&st, &headers, &aid, &p.terms, &back).await
}

async fn ask_page(
    State(st): State<AppState>,
    id: Identity,
    Query(p): Query<AskPrefill>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !st.core.asks() {
        return Err(Error::NotFound);
    }
    // A query typed on the rail and then retyped into ask is the cost of two
    // pages with nothing carried between them. Only when the box is empty: a
    // question the operator arrived with — a gap's "ask again" — is never
    // overwritten by what they searched for a minute ago.
    let q = match p.q.trim().is_empty() {
        true => match &id.session {
            Some(sess) => st
                .core
                .sittings
                .read(sess, crate::store::now(), st.core.pursuit.idle_secs as i64)
                .queries
                .first()
                .cloned()
                .unwrap_or_default(),
            None => String::new(),
        },
        false => p.q,
    };
    Ok(HtmlTemplate(AskTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        q,
        sitting: sitting_rail(&st, &id).await,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
struct AskForm {
    q: String,
}

/// Parks the question and hands back the id that streams it.
///
/// The model call belongs to the GET that follows, not here: `EventSource` is
/// GET-only, so the alternative is a GET that runs inference and writes a row —
/// exactly what history, prefetchers and link scanners replay. The id is the
/// guard, and it is spent on first use.
async fn ask_submit(
    State(st): State<AppState>,
    id: Identity,
    Form(f): Form<AskForm>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !st.core.asks() {
        return Err(Error::NotFound);
    }
    // Refused before anything is parked, so an empty box costs no entry in the
    // map and no second round trip to find out.
    if f.q.trim().is_empty() {
        return Err(Error::Validation("question is empty".into()));
    }
    let handoff = st.ask_handoff_park(
        crate::core::ask::AskRequest {
            q: f.q,
            limit: None,
            tags: vec![],
            category: None,
        },
        &id.subject,
    );
    Ok(axum::Json(serde_json::json!({ "id": handoff })).into_response())
}

/// One ask, as it happens.
///
/// Takes an `Identity` like every other `/ui` route: this one runs a model
/// call, and an endpoint that runs inference for whoever guesses a URL is a
/// free-inference hole rather than a page.
///
/// A reader who leaves before `Done` records nothing. That is not an oversight:
/// the recorded id reaches the page only in `Done`, so an abandoned ask has no
/// verdict bar, nothing to judge, and retention deletes an unjudged row anyway.
async fn ask_stream(
    State(st): State<AppState>,
    id: Identity,
    Path(handoff): Path<String>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !st.core.asks() {
        return Err(Error::NotFound);
    }
    use tokio_stream::StreamExt as _;

    // Unknown, already spent, expired, or somebody else's — all one answer.
    // Never a fresh ask against an empty question, which would spend a model
    // call on a replay; never another subject's question, which would be
    // answered to the wrong person and recorded under their name.
    let req = st
        .ask_handoff_take(&handoff, &id.subject)
        .ok_or(Error::NotFound)?;
    let core = st.core.clone();
    let origin = crate::store::feedback::Door::Ui.by(id.subject);
    let events = async_stream::stream! {
        let s = core.ask_events(&req, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            yield match ev {
                Ok(e) => sse_event(e),
                // Terminal by construction: the producer is a `try_stream!` and
                // ends at its first error, so the page sees one `error` event
                // and nothing after it.
                Err(e) => Ok(SseEvent::default().event("error").data(e.to_string())),
            };
        }
    };
    // Kept alive because a slow model thinks for longer than a proxy's idle
    // timeout, and a connection closed mid-answer looks to the page exactly
    // like an answer that ended.
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// One `AskEvent` as one named SSE event carrying JSON.
///
/// JSON rather than bare text for every payload, because SSE frames data by
/// line: a token that ends in a newline, or an answer whose markdown carries
/// blank lines, does not survive the wire as itself.
fn sse_event(ev: crate::core::ask::stream::AskEvent) -> Result<SseEvent> {
    use crate::core::ask::stream::AskEvent::*;
    let (name, data) = match ev {
        Retrieved {
            round,
            retrieved,
            shown,
            dropped,
            cliff_at,
        } => (
            "retrieved",
            serde_json::json!({
                "round": round,
                "retrieved": retrieved,
                "shown": shown,
                "dropped": dropped,
                "cliff_at": cliff_at,
            }),
        ),
        // A list rather than a string: the page joins it, so the separator is
        // one decision made where the sentence is written rather than here.
        Needs(what) => ("needs", serde_json::json!({ "queries": what })),
        Citations(hits) => (
            "citations",
            serde_json::json!({ "rail": rail_fragment(hits)? }),
        ),
        Reasoning(t) => ("reasoning", serde_json::json!({ "text": t })),
        Token(t) => ("token", serde_json::json!({ "text": t })),
        Done(d) => (
            "done",
            serde_json::json!({
                "event_id": d.event_id,
                "html": answer_fragment(*d)?,
            }),
        ),
    };
    Ok(SseEvent::default().event(name).data(data.to_string()))
}

/// The rail, rendered here rather than in the browser.
///
/// One fragment rather than a list of fields, because the ids in it are the
/// other end of the links `link_citations` writes into the answer, and both
/// ends are then numbered by the same server-side pass. Rendering the rail in
/// the browser would put the two halves of a citation in two languages, where
/// only a person clicking could tell they still agree.
///
/// Each excerpt's markdown has already been through the sanitizing renderer, so
/// the page inserts HTML it was handed and never renders markdown itself.
fn rail_fragment(hits: Vec<crate::core::search::SearchResult>) -> Result<String> {
    AskRailTemplate {
        citations: hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| render_hit(i, h, &Default::default()))
            .collect(),
    }
    .render()
    .map_err(|e| Error::Internal(e.to_string()))
}

/// The finished answer, as the page swaps it in.
///
/// The same template the blocking render used, for the same reason it existed:
/// one account of what an answer looks like. Only its delivery moved.
fn answer_fragment(out: crate::core::ask::AskResponse) -> Result<String> {
    // The answer is model output too, so it goes through the same sanitizing
    // renderer as chunk text. Marking comes after sanitizing: it works on the
    // escaped text a reader sees, and nothing it inserts needs cleaning.
    // Linking comes last, so a `[1]` that marking has just wrapped is still
    // found and neither pass has to know about the other's markup.
    let answer = link_citations(
        &crate::core::ask::check::mark_unsupported(
            &markdown::render(&out.answer),
            &out.unsupported,
        ),
        out.citations.len(),
    );
    AnswerTemplate {
        answer,
        citations: out
            .citations
            .into_iter()
            .enumerate()
            .map(|(i, h)| render_hit(i, h, &Default::default()))
            .collect(),
        dropped: out.dropped,
        truncated: out.truncated,
        abstained: out.abstained,
        unsupported: out.unsupported,
        verdict_bar: match &out.event_id {
            Some(id) => AskVerdictTemplate {
                event_id: id.clone(),
                verdict: None,
                oob: false,
            }
            .render()
            .map_err(|e| Error::Internal(e.to_string()))?,
            None => String::new(),
        },
        event_id: out.event_id,
    }
    .render()
    .map_err(|e| Error::Internal(e.to_string()))
}

/// Turns each `[n]` the answer cites into a link to that excerpt's rail item.
///
/// Bounded by `n`, the number of excerpts actually shown: a model writes `[7]`
/// over four excerpts often enough, and a link to a rail item that does not
/// exist scrolls nowhere while reading as a citation that is there. An
/// out-of-range bracket is left as the plain text it is.
///
/// Tag interiors are skipped because an attribute value is not prose, and code
/// spans are skipped because `argv[1]` is not a citation.
///
/// That second exclusion is the opposite of what `mark_unsupported` does over
/// the same markup, and deliberately so. Marking *subtracts* trust, and inside
/// code is where a fabricated command hides, so marking there is the feature.
/// Linking *adds* it: `<a href="#cite-1">[1]</a>` asserts that excerpt 1
/// supports this token, and a reader cannot tell an authored citation from a
/// coincidence. `arr[0]`, `argv[1]`, `results[2]` are exactly the shapes that
/// collide, because excerpt counts are single-digit and so are array indices —
/// on a base whose answers are full of code. Fabricated provenance is the one
/// failure this codebase exists to prevent, and a wrong link is worse than no
/// link.
fn link_citations(html: &str, n: usize) -> String {
    if n == 0 {
        return html.to_string();
    }
    crate::core::ask::check::for_text_between_tags(html, |t, in_code| match in_code {
        true => std::borrow::Cow::Borrowed(t),
        false => std::borrow::Cow::Owned(link_text(t, n)),
    })
}

/// The bracket scan, over one run of prose between tags.
fn link_text(text: &str, n: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        // The parsed number, never the digits as written: `[01]` cites excerpt
        // one, and an anchor of `#cite-01` points at nothing the rail emits.
        let cited = digits.parse::<usize>().ok().filter(|i| (1..=n).contains(i));
        match (after[digits.len()..].strip_prefix(']'), cited) {
            (Some(tail), Some(i)) => {
                out.push_str(&format!(
                    r##"<a class="cite" href="#cite-{i}">[{digits}]</a>"##
                ));
                rest = tail;
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(serde::Deserialize)]
struct VerdictForm {
    verdict: String,
}

fn verdict_label(v: crate::store::asks::AskVerdict) -> String {
    use crate::store::asks::AskVerdict::*;
    match v {
        Right => "right",
        Wrong => "wrong",
        NothingHere => "nothing here",
    }
    .into()
}

async fn ask_verdict_bar(st: &AppState, id: &str, oob: bool) -> Result<String> {
    let ev = st.core.store.ask_event(id).await?.ok_or(Error::NotFound)?;
    AskVerdictTemplate {
        event_id: ev.id,
        verdict: ev.verdict.map(verdict_label),
        oob,
    }
    .render()
    .map_err(|e| Error::Internal(e.to_string()))
}

async fn ask_verdict(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Form(f): Form<VerdictForm>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !st.core.asks() {
        return Err(Error::NotFound);
    }
    match f.verdict.as_str() {
        "none" => st.core.store.unjudge_ask(&id).await?,
        v => {
            let verdict = crate::store::asks::AskVerdict::parse(v)
                .ok_or_else(|| Error::Validation(format!("unknown verdict {v}")))?;
            st.core.store.judge_ask(&id, verdict).await?;
        }
    }
    Ok(axum::response::Html(ask_verdict_bar(&st, &id, false).await?).into_response())
}

#[derive(serde::Deserialize)]
struct CarriedForm {
    n: i64,
}

async fn ask_carried(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
    Form(f): Form<CarriedForm>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !st.core.asks() {
        return Err(Error::NotFound);
    }
    let carried = st.core.store.toggle_carried(&id, f.n).await?;
    let bar = ask_verdict_bar(&st, &id, true).await?;
    Ok(HtmlTemplate(AskCarriedTemplate {
        event_id: id,
        n: f.n,
        carried,
        bar: Some(bar),
    })
    .into_response())
}

/// Keep an answer: store it as a source, here, without a detour through the
/// capture box.
///
/// The same pipeline as any paste — one corpus, segmented, embedded, searchable
/// — and the same concession the capture door already made: `origin = "ask"`
/// and the `ask` metadata, so what the base holds says a model wrote it, from
/// which question, and from which artifacts. Nothing about it is special
/// downstream, which is why this works whatever `synthesis` is set to: at
/// `eager` the windows go to the synthesiser, at `off` and `earned` they are
/// captured verbatim, and both end in artifacts with vectors.
///
/// The answer as the model wrote it, not as the operator retyped it: an
/// operator who wants to edit first has `edit first` beside this, which is the
/// old path unchanged.
async fn ask_keep(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !st.core.asks() {
        return Err(Error::NotFound);
    }
    // Unlike the capture door, there is no text to fall back to here: the row
    // is where the answer lives. A question that retention has already taken
    // has nothing left to keep, and saying so is better than storing an empty
    // source or an unprovenanced one.
    let ev = st.core.store.ask_event(&id).await?.ok_or(Error::NotFound)?;
    let out = st
        .core
        .ingest_capture(
            crate::core::ingest::Capture::new(&ev.answer, ORIGIN_ASK).with_ask(
                &ev.id,
                &ev.question,
                &ev.citations,
            ),
        )
        .await?;
    Ok(HtmlTemplate(AskKeptTemplate {
        id: out.id,
        duplicate: out.duplicate,
        parked: out.near_duplicate.is_some(),
        near_dupe_percent: out
            .near_duplicate
            .as_ref()
            .map(|n| (n.similarity * 100.0).round() as i64)
            .unwrap_or(0),
    })
    .into_response())
}

/// Neighbours shown beside an artifact. A short list, because this is a way
/// out of the pane rather than a second result rail.
const RELATED_LIMIT: usize = 5;

/// Everything the pane needs, in one place, so the handler is only routing.
pub(crate) async fn build_artifact_detail(
    core: &crate::core::Core,
    artifact_id: &str,
    terms: &str,
) -> Result<ArtifactDetail> {
    let c = core.store.get_artifact(artifact_id).await?;
    let html = artifact_html(&c);
    // A merged artifact belongs to no corpus, so there are no lines to show
    // beside it and no span to highlight. Task 15 fills that half of the pane
    // with the artifacts it was written from; until then it renders without a
    // source block rather than claiming a document it did not come from.
    let src = match &c.corpus_id {
        Some(id) => Some(core.store.get_corpus(id).await?),
        None => None,
    };
    let slice = match &src {
        Some(s) => crate::web::corpus_view::slice(s, c.corpus_span.as_ref(), 3),
        None => crate::web::corpus_view::CorpusSlice::default(),
    };
    // A missing lineage is not a missing pane, for the same reason a missing
    // neighbour list is not: it is a layer over the artifact, and the artifact
    // beside its source is what the page is for.
    let lineage = crate::web::lineage_view::build(&core.store, artifact_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no lineage for this pane");
            Default::default()
        });
    // A missing neighbour list is not a missing pane. The vector store may be
    // down, or this artifact may simply not be embedded yet, and neither is a
    // reason to refuse to show the artifact beside its source.
    let related = core
        .vectors
        .neighbours(artifact_id, RELATED_LIMIT)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(artifact_id, error = %e, "no related artifacts for this pane");
            vec![]
        })
        .into_iter()
        .map(|h| RelatedArtifact {
            title: h
                .payload
                .title
                .unwrap_or_else(|| markdown::snippet(&h.payload.text, 40)),
            snippet: markdown::snippet(&h.payload.text, 90),
            id: h.payload.artifact_id,
        })
        .collect();
    // Unreadable links are not a missing pane, for the same reason a missing
    // neighbour list is not: this layer can only ever add. And gated on
    // `associating()`, not just `associate.enabled`: a base that learned
    // links and then had the feature switched off must stop rendering them,
    // the same as every other associative surface.
    let anchor = vec![c.id.clone()];
    let seen_together_links = if core.associating() {
        match core
            .store
            .links_from(
                &anchor,
                &[
                    crate::store::links::LinkState::Learning,
                    crate::store::links::LinkState::Related,
                ],
                core.associate.half_life_days,
                crate::store::now(),
                core.associate.show_min,
                RELATED_LIMIT as i64,
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(artifact_id, error = %e, "no links for this pane");
                vec![]
            }
        }
    } else {
        vec![]
    };
    let mut seen_together = Vec::new();
    for l in seen_together_links.into_iter().take(RELATED_LIMIT) {
        let Ok(other) = core.store.get_artifact(&l.other).await else {
            continue;
        };
        let corpus_title = match &other.corpus_id {
            Some(id) => core
                .store
                .get_corpus(id)
                .await
                .ok()
                .and_then(|s| s.title_hint)
                .unwrap_or_else(|| "untitled".into()),
            // A merged artifact belongs to no document, which is worth saying
            // rather than leaving blank.
            None => "merged".to_string(),
        };
        seen_together.push(SeenTogether {
            title: title_of(&other),
            snippet: markdown::snippet(&other.text, 90),
            // The judge's line where there is one; otherwise the question that
            // bound them, which is the link's own explanation and free.
            why: l
                .reason
                .clone()
                .or_else(|| l.cues.first().map(|c| format!("when asking: {}", c.q))),
            corpus_title,
            cross_corpus: l.cross_corpus,
            id: other.id,
        });
    }
    // Built before the struct consumes `c`. The fragment is what makes the
    // browser scroll to the span; the query parameters are what make the page
    // highlight it.
    // Empty for a merged artifact: there is no document to link to, and the
    // template hides the whole source block rather than offering a dead link.
    let source_at_lines = match (&c.corpus_id, c.corpus_span.as_ref()) {
        (Some(cid), Some(sp)) => format!(
            "/ui/corpora/{cid}?from={}&to={}#L{}",
            sp.start_line, sp.end_line, sp.start_line
        ),
        (Some(cid), None) => format!("/ui/corpora/{cid}"),
        (None, _) => String::new(),
    };
    let orphaned_source = c.flags.iter().any(|f| f == "orphaned_source");
    // Only asked when the passage actually stops mid-sentence: the query is a
    // second lookup per pane, and most passages end where a sentence does.
    let continues_at = match (&c.corpus_id, ends_mid_sentence(&c.text)) {
        (Some(cid), true) => core
            .store
            .adjacent_artifacts(cid, c.ordinal)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|n| n.ordinal > c.ordinal)
            .map(|n| n.id),
        _ => None,
    };
    // The same rule as `artifact_title`, and for the same reason: an ordinal in
    // the ingest is not a name. Taken before the struct, which moves `c`.
    let title = artifact_title(&c);
    Ok(ArtifactDetail {
        continues_at,
        related,
        seen_together,
        orphaned_source,
        source_at_lines,
        lineage,
        id: c.id,
        title,
        html,
        text: c.text,
        category: c.category,
        tags: c.tags,
        flags: c.flags,
        flag_detail: c.flag_detail,
        superseded_by: c.superseded_by,
        status: c.status,
        last_verified_at: c.last_verified_at,
        caveats: c.caveats,
        merged: c.provenance.is_model_written(),
        synthesized: c.provenance == crate::store::artifacts::Provenance::Synthesized,
        cues: c.cues,
        corpus_id: c.corpus_id,
        // A merged artifact has no corpus and so cannot have a restored one.
        corpus_restored: src.as_ref().is_some_and(|s| s.restored_at.is_some()),
        segment_idx: c.segment_idx,
        slice_label: slice.label,
        slice_lines: slice.lines,
        terms: terms.to_string(),
    })
}

#[derive(serde::Deserialize)]
struct ArtifactViewParams {
    #[serde(default)]
    terms: String,
    /// The artifact this one was reached from — a neighbour, an association,
    /// a continuation — when the link came from another artifact's page.
    #[serde(default)]
    via: Option<String>,
    /// The cluster slot this was offered under, when the link came from the
    /// area under the search box.
    #[serde(default)]
    rec: Option<i64>,
    /// And the rung it was offered on. Carried on the link because the offer
    /// was computed on a previous request and nothing server-side still holds
    /// it — without it, every click lands in one bucket on Ops.
    #[serde(default)]
    rung: Option<String>,
}

/// One route, two shapes. An htmx swap wants the pane's body; a pasted link
/// wants a page with navigation around it.
async fn artifact_detail(
    State(st): State<AppState>,
    id: Identity,
    headers: axum::http::HeaderMap,
    Path(cid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
) -> Result<Response> {
    let d = build_artifact_detail(&st.core, &cid, &p.terms).await?;
    // Opening a chunk is the deliberate act that counts as remembering it.
    st.core.mark_artifact_seen(&cid);
    // And the act the pursuit sweep reads: opened, or pivoted through — unless
    // this came from the area under the search box, in which case it is written
    // under its own kind and *not* as an ordinary open. A `recommended_open`
    // counted as an open is the first lucky guess growing into a habit the
    // system taught itself.
    match p.rec {
        Some(slot) => st.core.record_recommendation(
            &cid,
            "recommended_open",
            p.rung.as_deref().unwrap_or("unknown"),
            Some(slot),
            Some(&id.subject),
        ),
        None => st
            .core
            .record_interaction(&cid, p.via.as_deref(), Some(&id.subject)),
    }
    // The live half of the same act. Written here rather than inside
    // `record_interaction` because this is where the session is known — and
    // that is the whole of what keeps the sitting at the web door.
    if let Some(sess) = &id.session {
        st.core.sittings.touched(
            sess,
            &cid,
            crate::store::now(),
            st.core.pursuit.idle_secs as i64,
        );
    }
    if headers.contains_key("hx-request") {
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(HtmlTemplate(ArtifactDetailPage {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        d,
    })
    .into_response())
}

/// The operator saying this pair does not belong together.
///
/// Final for that pair: never shown, never judged, never pruned. The weight is
/// left exactly as it is, so the decision stays auditable against the evidence
/// that produced it — undoing one is out of scope, and Ops is where it would go.
async fn dismiss_link(
    State(st): State<AppState>,
    _id: Identity,
    Path((artifact_id, other_id)): Path<(String, String)>,
) -> Result<Response> {
    st.core.store.dismiss_link(&artifact_id, &other_id).await?;
    // The row swaps itself out and leaves the pane alone, so the artifact you
    // were reading is still on screen afterwards.
    Ok(axum::response::Html(String::new()).into_response())
}

/// Clearing a flag is a judgement, not a fix: the operator looked at the chunk
/// beside its source lines and decided the warning was noise.
async fn mark_artifact_reviewed(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    // For an orphaned merge, "reviewed" means accepted as a merge of what
    // remains — recorded on source_count, or the next sweep re-flags it and
    // the operator's judgement lasts one tick.
    let c = st.core.store.get_artifact(&cid).await?;
    if c.flags.iter().any(|f| f == "orphaned_source") {
        st.core.store.accept_source_loss(&cid).await?;
    }
    st.core.store.clear_artifact_flags(&cid).await?;
    Ok(axum::response::Html(String::new()).into_response())
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
}

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
/// one path nobody routed was the one path that rendered the whole nav,
/// `judge_pending` — a live count out of the base — included.
pub async fn not_found(
    State(st): State<AppState>,
    id: Option<Identity>,
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
    if id.is_none() {
        return crate::error::Error::Unauthorized.into_response();
    }
    let page = NotFoundTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
    };
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
        .route("/", get(|| async { Redirect::to("/ui/search") }))
        .route("/ui", get(|| async { Redirect::to("/ui/search") }))
        .route("/ui/capture", get(capture_page).post(capture_submit))
        .route("/ui/search", get(search_page))
        .route("/ui/search/results", get(search_results))
        .route("/ui/context", post(context_offer))
        .route("/ui/queue", get(queue_fragment))
        // An installed PWA may still hold /ui/browse as its start URL, and a
        // bookmark outlives the page it pointed at.
        // Takes an `Identity` like every other page: a gone page must still send
        // a signed-out visitor to sign in rather than bouncing them onward.
        .route(
            "/ui/browse",
            get(|_id: Identity| async { Redirect::to("/ui/capture") }),
        )
        .route("/ui/corpora/{id}", get(corpus_detail))
        .route("/ui/corpora/{id}/delete", post(delete_corpus_ui))
        .route("/ui/corpora/{id}/reprocess", post(reprocess_ui))
        .route("/ui/corpora/{id}/reread", post(reread_uncovered_ui))
        .route(
            "/ui/corpora/{id}/segments/{idx}/unpromote",
            post(unpromote_ui),
        )
        .route("/ui/artifacts/{id}", get(artifact_detail).put(put_artifact))
        .route("/ui/artifacts/{cid}/reviewed", post(mark_artifact_reviewed))
        .route(
            "/ui/artifacts/{id}/links/{other}/dismiss",
            post(dismiss_link),
        )
        .route("/ui/artifacts/{id}/delete", post(delete_artifact_ui))
        .route("/ui/artifacts/{id}/dwell", post(artifact_dwell))
        .route("/ui/ask", get(ask_page).post(ask_submit))
        .route("/ui/ask/{id}/stream", get(ask_stream))
        .route("/ui/ask/{id}/verdict", post(ask_verdict))
        .route("/ui/ask/{id}/carried", post(ask_carried))
        .route("/ui/ask/{id}/keep", post(ask_keep))
        .route("/ui/gaps/{kind}/{id}/dismiss", post(gap_dismiss))
        .route("/ui/ops", get(ops))
        // One name for the page. The nav word is Housekeeping and the route is
        // `/ui/ops`; a reader who types the word they were shown lands here.
        .route(
            "/ui/housekeeping",
            get(|| async { Redirect::permanent("/ui/ops") }),
        )
        .route("/ui/settings", get(settings))
        .route("/ui/ops/tokens", post(mint_token))
        .route("/ui/ops/feedback/purge", post(purge_feedback_ui))
        .route("/ui/ops/tokens/{id}/revoke", post(revoke_token_ui))
        .route("/ui/ops/corpora/{id}/resolve", post(resolve_near_dupe_ui))
        .route("/ui/ops/artifacts/{id}/unsupersede", post(unsupersede_ui))
        .route("/ui/ops/artifacts/{id}/deprecate", post(deprecate_ui))
        .route("/ui/ops/artifacts/{id}/reactivate", post(reactivate_ui))
        .route("/ui/ops/merges/{id}/undo", post(undo_merge_ui))
        .route("/ui/ops/artifacts/{id}/verify", post(verify_ui))
        .route("/ui/ops/pairs/{id}/dismiss", post(dismiss_pair_ui))
        .route(
            "/ui/ops/pairs/{id}/supersede",
            post(apply_pair_supersede_ui),
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

    fn pair_row_fixture(a_id: &str, a_title: &str, a_opening: &str) -> PairRow {
        PairRow {
            id: 1,
            percent: 90,
            a_id: a_id.into(),
            a_title: a_title.into(),
            b_id: "b".into(),
            b_title: "SQLite-Datenbankeinstellungen und WAL".into(),
            a_opening: a_opening.into(),
            b_opening: "Einstellungen der SQLite-Datenbank".into(),
            a_excerpt: "Auto Vacuum werden freie Pages in der Free Page List verwaltet".into(),
            b_excerpt: "Einstellungen der SQLite-Datenbank koennen ueber Pragma".into(),
            detail: None,
            via_link: false,
            contradiction: true,
            obsolete_title: None,
            keeps_a: false,
            keeps_b: false,
        }
    }

    fn ask_page_fixture() -> String {
        askama::Template::render(&AskTemplate {
            judge_pending: None,
            ask_enabled: true,
            q: String::new(),
            sitting: vec![],
        })
        .unwrap()
    }

    fn answer_fixture(dropped: usize) -> String {
        askama::Template::render(&AnswerTemplate {
            answer: "<p>An answer.</p>".into(),
            citations: vec![],
            dropped,
            truncated: false,
            abstained: false,
            unsupported: vec![],
            event_id: None,
            verdict_bar: String::new(),
        })
        .unwrap()
    }

    fn settings_fixture(tokens: Vec<TokenRow>) -> String {
        askama::Template::render(&SettingsTemplate {
            judge_pending: None,
            ask_enabled: true,
            tokens,
            feedback: None,
            asks: None,
        })
        .unwrap()
    }

    #[test]
    fn the_ungrouped_gaps_say_what_being_ungrouped_means() {
        // "not yet grouped (1)" over a question, with no indication that the
        // grouping is a sweep that has not run rather than a state of the
        // question itself.
        // `_gaps.html` is only ever included, so it has no template struct of
        // its own; this is one, standing in for the page that includes it.
        #[derive(Template)]
        #[template(path = "_gaps.html")]
        struct Gaps {
            gaps: Vec<GapGroup>,
            loose: Vec<GapMember>,
            ask_enabled: bool,
        }
        let html = askama::Template::render(&Gaps {
            gaps: vec![],
            loose: vec![GapMember {
                kind: "ask".into(),
                badge: "asked",
                id: "g1".into(),
                text: "wie werden bei chipkarten die private keys geschützt?".into(),
            }],
            ask_enabled: true,
        })
        .unwrap();
        assert!(
            html.contains("has not run yet"),
            "nothing says why these are ungrouped: {html}"
        );
    }

    #[test]
    fn a_token_table_with_no_tokens_says_so_instead_of_showing_its_headings() {
        // Five column headings over nothing is a table pretending to have
        // rows — the same thing `_decide.html` names at its top: the old Ops
        // page answered five headings with "None." and made an empty base look
        // like a backlog.
        let html = settings_fixture(vec![]);
        assert!(!html.contains("Minted by"), "{html}");
        assert!(html.contains("No tokens yet"), "{html}");
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
    fn a_sweep_that_took_no_time_does_not_say_it_happens_now() {
        // Every row of Housekeeping's TOOK column read "now", because the
        // column spends `fmt_duration` — which answers when something runs
        // next, not how long it took.
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(3), "3s");
        assert_eq!(fmt_elapsed(75), "1m 15s");
        assert_eq!(fmt_elapsed(3600), "1h 0m");
        assert_eq!(fmt_elapsed(-5), "0s", "a clock that went backwards");
        // And the future-tense helper keeps its own meaning.
        assert_eq!(fmt_duration(0), "now");
        assert_eq!(fmt_duration(300), "in 5m");
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

    #[test]
    fn the_judges_own_answers_stay_with_the_question() {
        // The three ways out sat below every candidate — twenty at
        // `feedback.candidates`' default — so answering "None of these" meant
        // scrolling past all of them first. The bar is sticky in CSS; what
        // this holds is that it is one element, in one place, for the rule to
        // key on.
        let css = include_str!("../../assets/css/42-judge.css");
        assert!(
            css.contains(".judge-outs {") && css.contains("position: sticky"),
            "the action bar is not sticky"
        );
        assert!(
            css.contains("background: var(--color-bg-base)"),
            "a sticky bar with no background is one the cards scroll through"
        );
    }

    #[tokio::test]
    async fn a_search_with_nothing_open_leaves_the_grid_free_to_widen_the_rail() {
        // 22rem of rail beside a thousand pixels holding one line of
        // placeholder is the whole complaint. `has-selection` is what the pane
        // gains when something is opened into it, so its absence on first
        // paint is what the wide-rail rule keys on — see `40-search.css`.
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/search?q=write+blocker").await;
        assert!(page.contains("regions-rail-focus-source"), "{page}");
        assert!(
            !page.contains("has-selection"),
            "a fresh search already claims something is open: {page}"
        );
    }

    #[test]
    fn the_answer_says_what_was_dropped_in_words_a_person_uses() {
        // "18 excerpt(s) omitted for context budget" is the accounting, and
        // the "(s)" is the plural nobody wrote out.
        let html = answer_fixture(18);
        assert!(!html.contains("excerpt(s)"), "{html}");
        assert!(!html.contains("context budget"), "{html}");
        assert!(html.contains("18 more excerpts did not fit"), "{html}");
        let one = answer_fixture(1);
        assert!(one.contains("1 more excerpt did not fit"), "{one}");
    }

    #[test]
    fn an_ask_in_flight_offers_a_way_to_stop_it() {
        // Fifty seconds signalled by a small grey "thinking…" beside the
        // button, and nothing on the page to end it with.
        let html = ask_page_fixture();
        assert!(html.contains(r#"id="ask-stop""#), "{html}");
    }

    #[test]
    fn an_ask_page_does_not_open_with_the_models_reasoning_showing() {
        // The deployment streamed the chain of thought into the page for fifty
        // seconds, restating the prompt's own constraints verbatim — "Answer
        // *only* using the provided knowledge-base excerpts" — above the empty
        // space where the answer was going to be.
        let html = ask_page_fixture();
        assert!(html.contains("ask-reasoning-box"), "{html}");
        assert!(
            !html.contains("<details open")
                && !html.contains("<details id=\"ask-reasoning-box\" open"),
            "reasoning must start closed: {html}"
        );
    }

    #[test]
    fn a_pair_card_carries_both_texts_to_read_in_place() {
        // The titles were links, so reading either side meant leaving the
        // queue and coming back to a card whose other half you now have to
        // remember.
        // `_decide.html` is only ever included, so it has no template struct
        // of its own; this is one, standing in for the page that includes it.
        #[derive(Template)]
        #[template(path = "_decide.html")]
        struct Decide {
            pairs: Vec<PairCluster>,
        }
        let html = askama::Template::render(&Decide {
            pairs: group_pairs(vec![pair_row_fixture("a1", "Auto Vacuum", "")]),
        })
        .unwrap();
        assert!(html.contains("<details"), "{html}");
        assert!(
            html.contains("Auto Vacuum werden freie Pages"),
            "the A side's text is not on the card: {html}"
        );
        assert!(
            html.contains("Einstellungen der SQLite-Datenbank koennen"),
            "the B side's text is not on the card: {html}"
        );
    }

    #[test]
    fn pairs_that_share_an_artifact_are_one_card() {
        // The deployment showed one artifact against three others as three
        // separate questions, 90%, 90% and 88% alike — the same decision
        // asked three times, where answering one did not retire the others.
        let p = |id: i64, a: &str, b: &str| PairRow {
            id,
            a_id: a.into(),
            b_id: b.into(),
            ..pair_row_fixture(a, "t", "")
        };
        let grouped = group_pairs(vec![
            p(1, "a", "b"),
            p(2, "a", "c"),
            p(3, "a", "d"),
            p(4, "x", "y"),
        ]);
        assert_eq!(grouped.len(), 2, "{} groups", grouped.len());
        assert_eq!(grouped[0].pairs.len(), 3);
        assert_eq!(grouped[0].members, 4, "one artifact against three others");
        assert_eq!(grouped[1].pairs.len(), 1);
        assert_eq!(grouped[1].members, 2);
    }

    #[test]
    fn a_chain_of_pairs_is_one_cluster_even_without_a_shared_artifact() {
        // a–b and b–c name no artifact in common, but resolving them
        // separately is what leaves A pointing at an artifact that is itself
        // hidden — the dead end `jobs::consolidate` documents.
        let p = |id: i64, a: &str, b: &str| PairRow {
            id,
            a_id: a.into(),
            b_id: b.into(),
            ..pair_row_fixture(a, "t", "")
        };
        let grouped = group_pairs(vec![p(1, "a", "b"), p(2, "b", "c")]);
        assert_eq!(grouped.len(), 1, "the chain was split");
        assert_eq!(grouped[0].members, 3);
    }

    #[test]
    fn pair_rows_sharing_a_title_are_disambiguated_too() {
        // Three artifacts on the deployment were titled "LevelDB:
        // Funktionsweise und forensische Analyse", so one cluster of
        // questions read as the same question asked three times.
        let mut rows = vec![
            // Two different artifacts that synthesis gave one name.
            pair_row_fixture(
                "a1",
                "LevelDB: Funktionsweise",
                "Der Aufbau der Datenlagerung",
            ),
            pair_row_fixture("a2", "LevelDB: Funktionsweise", "Die Extraktion der Keys"),
            pair_row_fixture(
                "a3",
                "Auto Vacuum und die Free Page List",
                "Freie Pages werden",
            ),
        ];
        disambiguate_pair_titles(&mut rows);
        assert!(!rows[0].a_opening.is_empty(), "nothing tells row 0 apart");
        assert_ne!(
            (&rows[0].a_title, &rows[0].a_opening),
            (&rows[1].a_title, &rows[1].a_opening),
            "still identical"
        );
        assert!(
            rows[2].a_opening.is_empty(),
            "a unique title needs no opening beside it: {:?}",
            rows[2].a_opening
        );
        assert!(
            rows[0].b_opening.is_empty(),
            "every row's B side is one artifact under one name — appearing three \
             times is a cluster, not a collision"
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
    use crate::web::test_support::{a_png, app_with_cookie, body_of};
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
                },
                crate::store::feedback::Door::Ui,
            )
            .await
            .unwrap();
        let r = super::render_hit(0, hits[0].clone(), &Default::default());

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
    async fn a_merged_artifact_shows_its_sources_instead_of_corpus_lines() {
        // A captured artifact renders the corpus lines its span claims. A merged
        // one has neither corpus nor span, so the pane shows what it was written
        // from — each source still stored, each still naming its own document.
        // Rendering a corpus it did not come from would put the wrong lines
        // beside it forever, which is the one dishonesty merging must not
        // commit.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m = crate::jobs::merge::write(
            &core,
            &crate::infer::prompt::MergedDraft {
                title: Some("a and b".into()),
                text: "a text and b text".into(),
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &ids,
        )
        .await
        .unwrap();

        let d = build_artifact_detail(&core, &m.id, "").await.unwrap();

        assert_eq!(d.corpus_id, None, "a merged artifact claimed a corpus");
        assert!(
            d.slice_lines.is_empty(),
            "a merged artifact rendered lines from a document it did not come from"
        );
        assert_eq!(d.lineage.leaves(), 2);
        let listed: Vec<&str> = d.lineage.roots.iter().map(|s| s.id.as_str()).collect();
        for id in &ids {
            assert!(listed.contains(&id.as_str()), "source {id} is not listed");
        }
        // And each source still points at the document it was captured from.
        assert!(
            d.lineage
                .roots
                .iter()
                .all(|s| s.source_href.starts_with("/ui/corpora/"))
        );
        assert!(!d.orphaned_source);
    }

    #[tokio::test]
    async fn a_captured_artifact_lists_no_sources() {
        // The template branches on provenance, and a captured artifact filling
        // this list would put a provenance list it does not have where its
        // corpus lines belong.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(&core, &[("a text", [1.0, 0.0])]).await;

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();

        assert!(d.lineage.is_empty());
        assert!(!d.merged);
        assert!(d.corpus_id.is_some());
    }

    #[tokio::test]
    async fn a_merge_that_lost_every_source_still_renders_as_a_merge() {
        // An empty source list is not the same question as "was this captured".
        // The template branched on the list, so a merge whose sources had all
        // been deleted fell through to the captured branch and rendered a
        // "Source · … highlighted" label over an empty link and an empty line
        // table — on exactly the artifact whose orphan notice matters most.
        let core = crate::core::test_support::test_core().await;
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])],
        )
        .await;
        let m = crate::jobs::merge::write(
            &core,
            &crate::infer::prompt::MergedDraft {
                title: Some("a and b".into()),
                text: "a text and b text".into(),
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &ids,
        )
        .await
        .unwrap();
        for id in &ids {
            core.store.delete_artifact(id).await.unwrap();
        }

        let d = build_artifact_detail(&core, &m.id, "").await.unwrap();

        assert!(
            d.lineage.roots.is_empty(),
            "the fixture did not lose the sources"
        );
        assert!(d.merged, "a merge was rendered as a captured artifact");
        assert!(
            d.source_at_lines.is_empty() && d.slice_lines.is_empty(),
            "there is no document to link and no lines to show"
        );
    }

    #[tokio::test]
    async fn the_detail_view_pairs_a_chunk_with_the_lines_it_claims() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);

        let d = match super::build_artifact_detail(&core, &c.id, "").await {
            Ok(d) => d,
            Err(e) => panic!("detail view failed: {e}"),
        };

        assert_eq!(d.corpus_id.as_deref(), Some(out.id.as_str()));
        assert!(d.html.contains("alpha"), "the chunk body must be rendered");
        assert!(
            !d.slice_lines.is_empty(),
            "the source slice must not be empty"
        );
        assert!(
            d.slice_lines.iter().any(|l| l.in_span),
            "at least one line must be marked as the span"
        );
        // Either form: this artifact's span may be one line or several, and
        // the label says which rather than always saying "lines".
        assert!(
            d.slice_label.starts_with("line ") || d.slice_label.starts_with("lines "),
            "{}",
            d.slice_label
        );
    }

    #[tokio::test]
    async fn a_passage_cut_mid_sentence_points_at_the_one_that_carries_the_rest() {
        // The pane ended "…der bereits vorgestellte Einsatz von" and offered
        // nothing onward, while the source column beside it showed the rest of
        // the sentence.
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest(
                "Die erste Vorkehrung ist der bereits vorgestellte Einsatz von\n\n\
                 Hardware-Schreibschutzadaptern, wo immer es möglich ist.",
                "web",
                None,
            )
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(all.len() > 1, "the fixture produced one passage, not two");

        let first = all.iter().min_by_key(|c| c.ordinal).unwrap();
        let d = super::build_artifact_detail(&core, &first.id, "")
            .await
            .unwrap();
        assert!(
            d.continues_at.is_some(),
            "no way onward from a cut sentence: {:?}",
            first.text
        );

        let last = all.iter().max_by_key(|c| c.ordinal).unwrap();
        let d = super::build_artifact_detail(&core, &last.id, "")
            .await
            .unwrap();
        assert!(
            d.continues_at.is_none(),
            "the last passage ends on a period and has nothing after it"
        );
    }

    #[tokio::test]
    async fn a_chunk_whose_source_vanished_is_not_a_500() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbravo", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);
        core.delete_corpus(&out.id).await.unwrap();

        match super::build_artifact_detail(&core, &c.id, "").await {
            Err(crate::error::Error::NotFound) => {}
            Err(e) => panic!("expected a not-found, got {e}"),
            Ok(_) => panic!("a chunk whose source was deleted must not resolve"),
        }
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

    async fn app_with_session() -> (axum::Router, String) {
        let (app, cookie, _core) = app_session_and_core().await;
        (app, cookie)
    }

    async fn app_session_and_core() -> (axum::Router, String, crate::core::Core) {
        let core = crate::core::test_support::test_core().await;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        (app, cookie, handle)
    }

    /// A session whose core records searches, which is what the association
    /// features are gated on. `app_session_and_core` cannot be reused: the
    /// router owns its own clone of the core, so flipping a flag afterwards
    /// changes the handle and not the app.
    async fn app_session_and_core_with_feedback() -> (axum::Router, String, crate::core::Core) {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        (app, cookie, handle)
    }

    async fn get_body(app: &axum::Router, cookie: &str, uri: &str) -> String {
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
        assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
        body_of(res).await
    }

    /// The same, for the one route that takes a `PUT`: editing an artifact.
    fn put_form(uri: &str, cookie: &str, body: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("PUT")
            .header("cookie", cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn form(uri: &str, cookie: &str, body: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("POST")
            .header("cookie", cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// The first half of the two-request ask: park the question, take the id.
    /// `q` is form-encoded, as it is in the body it goes into.
    async fn post_ask(app: &axum::Router, cookie: &str, q: &str) -> String {
        let res = app
            .clone()
            .oneshot(form("/ui/ask", cookie, &format!("q={q}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "the question was not parked");
        crate::web::test_support::json_of(res).await["id"]
            .as_str()
            .expect("parking hands back an id")
            .to_string()
    }

    /// The second half: spend the id and stream.
    async fn get_stream(app: &axum::Router, cookie: &str, id: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/ask/{id}/stream"))
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// One whole ask over the wire, as the page performs it.
    async fn ask_over_sse(app: &axum::Router, cookie: &str, q: &str) -> String {
        let id = post_ask(app, cookie, q).await;
        let res = get_stream(app, cookie, &id).await;
        assert_eq!(res.status(), StatusCode::OK);
        body_of(res).await
    }

    /// The HTML the page swaps in, pulled out of the `done` frame the way the
    /// browser reads it: the payload is JSON, so the fragment survives the
    /// blank lines its markdown carries.
    fn done_html(body: &str) -> String {
        let data = body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
            .find(|v| v.get("html").is_some())
            .unwrap_or_else(|| panic!("no done event in {body}"));
        data["html"].as_str().unwrap().to_string()
    }

    /// A session plus a corpus that has been through synthesis and embedding,
    /// which is the only state in which there is anything to facet or to find a
    /// neighbour among.
    async fn app_with_embedded_corpus() -> (axum::Router, String) {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        app_with_cookie(core).await
    }

    /// Markup with every run of whitespace collapsed, so an assertion about an
    /// attribute pair does not also assert where the template wrapped a line.
    fn flat(html: &str) -> String {
        html.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// A session on the given core, for pages that need a core built a
    /// particular way.
    async fn app_for(core: crate::core::Core) -> (axum::Router, String) {
        app_with_cookie(core).await
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
                        ordinal: 0,
                        text: "the first one".into(),
                        corpus_span: None,
                        title: Some("first".into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "the second one".into(),
                        corpus_span: None,
                        title: Some("second".into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
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
        assert!(html.contains("name=\"note\""), "the context field is there");

        let (app, cookie) =
            app_for(crate::core::test_support::test_core_without_vision().await).await;
        let html = get(&app, "/ui/capture", &cookie).await;
        assert!(!html.contains("image/*"));
        assert!(html.contains("accept=\".txt,text/plain,.pdf,application/pdf\""));
    }

    #[tokio::test]
    async fn a_pdf_corpus_page_offers_re_extract_and_names_the_failure() {
        let core = crate::core::test_support::test_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
            })
            .await
            .unwrap()
            .id;
        crate::jobs::extract::park_failed(&core, &id, "that PDF holds no extractable text")
            .await
            .unwrap();

        let (app, cookie) = app_for(core).await;
        let html = get(&app, &format!("/ui/corpora/{id}"), &cookie).await;
        assert!(
            html.contains("no extractable text"),
            "the reason is what the page is for: {html}"
        );
        assert!(
            html.contains(r#"value="extract""#),
            "no Re-extract on a PDF that failed: {html}"
        );
        assert!(
            html.contains(&format!("/api/v1/corpora/{id}/file")),
            "the original is not reachable: {html}"
        );
        assert!(
            !html.contains("Re-segment"),
            "nothing was extracted; there is nothing to re-segment: {html}"
        );
    }

    #[tokio::test]
    async fn the_capture_page_prices_a_capture_by_the_mode_it_will_run_in() {
        // At `earned` — the default — capture synthesizes nothing: the text is
        // embedded as written and a window is rewritten later only where
        // reading earns it. Promising "16 model calls" there is the page
        // lying about what the button costs.
        let mut core = crate::core::test_support::test_core().await;
        core.synthesis = crate::config::SynthesisMode::Earned;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui/capture", &cookie).await;
        assert!(
            html.contains("kept as you wrote it"),
            "the standing line still prices a call: {html}"
        );
        assert!(html.contains("var EAGER = false"));
        assert!(
            !html.contains("one model call each"),
            "the standing line still prices a call: {html}"
        );

        let mut core = crate::core::test_support::test_core().await;
        core.synthesis = crate::config::SynthesisMode::Eager;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui/capture", &cookie).await;
        assert!(html.contains("one model call each"));
        assert!(html.contains("var EAGER = true"));
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

    #[tokio::test]
    async fn an_image_corpus_page_shows_the_photo_its_facts_and_the_reading_as_derived() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .store
            .insert_attached_corpus(
                "h",
                "image",
                Some("IMG.png"),
                &serde_json::json!({
                    "note": "front porch",
                    "file": {"name": "IMG.png", "width": 4, "height": 2},
                    "exif": {"taken_at": "2026-08-09T14:12:03", "camera": "Pixel",
                             "gps": {"lat": 1.5, "lon": 2.5},
                             "tags": {"LensModel": "24mm f/1.8", "ExposureTime": "1/120"}}
                }),
                crate::store::corpora::Reading::VISION,
                &crate::store::attachments::NewFile {
                    kind: "image",
                    mime: "image/png",
                    filename: Some("IMG.png"),
                    bytes: b"orig",
                    preview: b"prev",
                    width: Some(4),
                    height: Some(2),
                },
            )
            .await
            .unwrap()
            .into_corpus();
        core.store
            .set_read_text(&src.id, "# Porch\n\nblue door", vec![])
            .await
            .unwrap();
        let (app, cookie) = app_for(core).await;
        let html = get(&app, &format!("/ui/corpora/{}", src.id), &cookie).await;
        assert!(
            html.contains(&format!("/api/v1/corpora/{}/image", src.id)),
            "img src"
        );
        assert_eq!(
            html.matches("front porch").count(),
            1,
            "the note belongs to the photo card and is printed there once: {html}"
        );
        assert!(html.contains("2026-08-09T14:12:03"));
        assert!(html.contains("1.5"));
        // Everything else the camera wrote is on the page too, folded away and
        // in tag order: this preview is the only copy of it that survives.
        assert!(html.contains("All 2 EXIF tags"));
        let (exposure, lens) = (
            html.find("ExposureTime").expect("exposure tag"),
            html.find("LensModel").expect("lens tag"),
        );
        assert!(exposure < lens, "the tags are listed by name");
        assert!(html.contains("24mm f/1.8"));
        assert!(
            html.contains("Transcription"),
            "the text is labelled as derived, not 'Raw corpus'"
        );
        assert!(html.contains("blue door"));
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

    async fn an_unread_image(core: &crate::core::Core) -> String {
        core.ingest_image(crate::core::ingest::ImageCapture {
            bytes: a_png(),
            filename: Some("p.png".into()),
            title_hint: None,
            note: None,
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn an_unread_image_page_offers_re_read_and_not_re_segment() {
        let core = crate::core::test_support::test_core().await;
        let id = an_unread_image(&core).await;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, &format!("/ui/corpora/{id}"), &cookie).await;
        assert!(html.contains("Re-read"));
        assert!(!html.contains("Re-segment"));
    }

    #[tokio::test]
    async fn the_re_read_button_queues_describe() {
        let core = crate::core::test_support::test_core().await;
        let id = an_unread_image(&core).await;
        crate::jobs::describe::park_failed(&core, &id, "HTTP 400")
            .await
            .unwrap();
        let (app, cookie) = app_for(core.clone()).await;
        let res = app
            .oneshot(form(
                &format!("/ui/corpora/{id}/reprocess"),
                &cookie,
                "stage=describe",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().status,
            CorpusStatus::Describing
        );
    }

    /// A session with the recommender on, plus one artifact old enough and
    /// unseen enough that `resurface` returns it.
    async fn app_recommending() -> (axum::Router, String, crate::store::Store, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let store = core.store.clone();
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "when the recycling centre is open".into(),
                    corpus_span: None,
                    title: Some("recycling centre".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()
            .remove(0);
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: vec![1.0; 8],
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: a.id.clone(),
                    corpus_id: src.id.clone(),
                    text: a.text.clone(),
                    title: Some("recycling centre".into()),
                    category: None,
                    tags: vec![],
                    created_at: 0,
                    last_seen_at: None,
                    hit_count: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                    origin_corpora: vec![],
                    provenance: None,
                },
            }])
            .await
            .unwrap();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        // Held so a test can drain the recording writes rather than sleep.
        BACKGROUND.with(|b| *b.borrow_mut() = Some(background));
        (app, cookie, store, a.id)
    }

    thread_local! {
        static BACKGROUND: std::cell::RefCell<Option<std::sync::Arc<crate::core::background::Background>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The recording writes run off the request path. Drain them rather than
    /// sleeping and hoping.
    async fn drain() {
        let b = BACKGROUND.with(|b| b.borrow().clone());
        if let Some(b) = b {
            b.wait_idle().await;
        }
    }

    /// The same base, plus one established situation matching the bundle the
    /// tests post — so the reason line actually renders.
    async fn app_with_a_learned_situation() -> (axum::Router, String, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "when the recycling centre is open".into(),
                    corpus_span: None,
                    title: Some("recycling centre".into()),
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
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: vec![1.0; 8],
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: aid.clone(),
                    corpus_id: src.id.clone(),
                    text: "when the recycling centre is open".into(),
                    title: Some("recycling centre".into()),
                    category: None,
                    tags: vec![],
                    created_at: 0,
                    last_seen_at: None,
                    hit_count: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                    origin_corpora: vec![],
                    provenance: None,
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
        let v = crate::core::context::encode(at, Some("user-1"), &bundle, &core.recommend.weights);
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
                    encoder_version: crate::core::context::ENCODER_VERSION,
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
        assert!(body.contains("Pattern"), "no reason line at all: {body}");
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
        assert!(!body.contains("Pattern"), "{body}");
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
    async fn taking_an_offer_is_not_an_ordinary_open() {
        // Without this the profile reinforces itself. The row is written, and
        // it is written under its own kind so the sweep can weigh it at
        // `self_weight` — which is zero.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        // Both on, so an ordinary open *would* be recorded — otherwise this
        // test would pass on a base that records nothing at all.
        core.pursuit.enabled = true;
        core.feedback.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "opening hours".into(),
                    corpus_span: None,
                    title: Some("hours".into()),
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
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        get(
            &app,
            &format!("/ui/artifacts/{aid}?rec=0&rung=pattern"),
            &cookie,
        )
        .await;
        background.wait_idle().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["recommended_open"], "not an ordinary open");
        assert!(
            rows[0].detail.as_deref().unwrap().contains("pattern"),
            "and it remembers which rung it was offered on: {:?}",
            rows[0].detail
        );

        // And the ordinary path still records an ordinary open, so the branch
        // above is a branch rather than a hole.
        get(&app, &format!("/ui/artifacts/{aid}"), &cookie).await;
        background.wait_idle().await;
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(rows.iter().any(|r| r.kind == "opened"));
    }

    #[tokio::test]
    async fn ops_shows_shown_against_clicked_by_rung() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let store = core.store.clone();
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "opening hours".into(),
                    corpus_span: None,
                    title: Some("hours".into()),
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
        let page = get(&app, "/ui/ops", &cookie).await;
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
        let page = get(&app, "/ui/ops", &cookie).await;
        assert!(!page.contains("What was offered"));
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

    /// A session on an installation that is recording searches, with `pending`
    /// of them captured and waiting for a verdict.
    async fn app_recording_searches(pending: usize) -> (axum::Router, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        for i in 0..pending {
            core.store
                .record_search(
                    crate::store::feedback::NewEvent {
                        query: format!("search number {i}"),
                        door: crate::store::feedback::Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        candidates: vec![],
                        answered: false,
                    },
                    // No folding: these stand for separate searches, not one
                    // being typed.
                    0,
                )
                .await
                .unwrap();
        }
        app_with_cookie(core).await
    }

    #[tokio::test]
    async fn judging_is_a_destination_in_the_nav_with_what_is_waiting_on_it() {
        // It used to be reachable only from one conditional sentence on Ops —
        // the page you open when something is wrong, which is the wrong place
        // for the screen that has to be visited often for the dataset to grow.
        let (app, cookie) = app_recording_searches(3).await;
        for page in ["/ui/search", "/ui/capture", "/ui/ask", "/ui/ops"] {
            let html = flat(&get(&app, page, &cookie).await);
            assert!(
                html.contains(r#"<a href="/ui/judge">Judge"#),
                "{page} offers no way to judge"
            );
            assert!(
                html.contains(r#"<span class="badge badge-accent">3</span>"#),
                "{page} does not say how many are waiting"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_queue_asks_for_nothing() {
        // The entry stays — judging is where the metrics live, and they are
        // worth reading with nothing pending — but a badge reading zero is an
        // invitation to a screen that has no work on it.
        let (app, cookie) = app_recording_searches(0).await;
        let html = flat(&get(&app, "/ui/search", &cookie).await);
        assert!(html.contains(r#"<a href="/ui/judge">Judge"#));
        assert!(
            !html.contains(r#"badge-accent">0<"#),
            "an empty queue was badged"
        );
    }

    #[tokio::test]
    async fn nothing_about_judging_appears_where_nothing_is_captured() {
        // Capture is off by default. A door to a queue that can never fill is
        // an offer the installation cannot keep.
        let (app, cookie) = app_with_session().await;
        let html = flat(&get(&app, "/ui/search", &cookie).await);
        assert!(!html.contains("/ui/judge"), "judging was advertised anyway");
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

    /// One merge written from an earlier merge and one fresh capture. A flat
    /// list of roots reads as three equal siblings; the generation between them
    /// is the whole reason the tree exists.
    #[tokio::test]
    async fn the_pane_draws_the_generations_a_merge_came_through() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = crate::jobs::consolidate::tests::seed_titled(
            &core,
            &[
                ("first capture", "a text", [1.0, 0.0]),
                ("second capture", "b text", [0.93, 0.37]),
                ("third capture", "c text", [0.9, 0.4]),
            ],
        )
        .await;
        let draft = |t: &str| crate::infer::prompt::MergedDraft {
            title: Some(t.into()),
            text: format!("{t} text"),
            category: None,
            tags: vec![],
            caveats: vec![],
        };
        let m1 = crate::jobs::merge::write(&core, &draft("first pass"), &ids[0..2])
            .await
            .unwrap();
        let m2 = crate::jobs::merge::write(
            &core,
            &draft("second pass"),
            &[m1.id.clone(), ids[2].clone()],
        )
        .await
        .unwrap();

        // What `merge::finish` does once the merge is indexed: the sources it
        // was written from are hidden behind it. Set here because this test is
        // about how the pane draws that, not about the write path.
        for hidden in [&ids[2], &m1.id] {
            core.store
                .set_superseded_by(hidden, Some(&m2.id))
                .await
                .unwrap();
        }

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{}", m2.id)).await;

        assert!(page.contains(r#"class="lineage""#), "{page}");
        assert!(
            page.contains("first pass"),
            "the earlier merge is a node: {page}"
        );
        for t in ["first capture", "second capture", "third capture"] {
            assert!(page.contains(t), "{t} is missing from the lineage: {page}");
        }
        assert!(
            page.contains("--d:1"),
            "the earlier merge's own sources are drawn under it: {page}"
        );
        assert!(
            page.contains("Written from 3 artifacts"),
            "the count is of captures, not of the route they took: {page}"
        );
        // The roots this merge superseded say so where they sit.
        assert!(page.contains("replaced by this"), "{page}");
    }

    /// A captured artifact was written from a document, not from artifacts. Its
    /// column is the document, and a tree there would be an empty claim.
    #[tokio::test]
    async fn the_pane_of_a_capture_still_shows_its_lines() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;

        assert!(page.contains("Source"), "{page}");
        assert!(!page.contains(r#"class="lineage""#), "{page}");
    }

    /// The corpus page could edit an artifact and the pane could not, on the
    /// screen whose whole subject is one artifact.
    #[tokio::test]
    async fn the_pane_edits_the_artifact_and_comes_back_a_pane() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{c}")).await;
        assert!(page.contains(&format!(r#"id="edit-{c}""#)), "{page}");

        let res = app
            .clone()
            .oneshot(put_form(
                &format!("/ui/artifacts/{c}"),
                &cookie,
                "view=detail&terms=&text=rewritten+by+hand",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(
            body.contains("data-terms"),
            "the pane was replaced by a list card: {body}"
        );
        assert!(body.contains("rewritten by hand"), "{body}");
        assert_eq!(
            core.store.get_artifact(&c).await.unwrap().embed_state,
            crate::store::artifacts::EmbedState::Pending,
            "the stored vector describes wording that no longer exists"
        );
    }

    /// And the corpus page, which swaps one card in a list, still gets a card.
    #[tokio::test]
    async fn the_corpus_card_edit_still_answers_with_a_card() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();

        let res = app
            .clone()
            .oneshot(put_form(
                &format!("/ui/artifacts/{c}"),
                &cookie,
                "text=edited+from+the+corpus+page",
            ))
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(body.contains(&format!(r#"id="artifact-{c}""#)), "{body}");
        assert!(!body.contains("data-terms"), "{body}");
    }

    #[tokio::test]
    async fn the_pane_lists_the_nearest_other_artifacts() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let artifacts = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            artifacts.len() > 1,
            "a neighbour list needs something to be a neighbour of"
        );

        let d = super::build_artifact_detail(&core, &artifacts[0].id, "")
            .await
            .unwrap();
        assert!(!d.related.is_empty(), "the pane listed no neighbours");
        assert!(
            d.related.iter().all(|r| r.id != artifacts[0].id),
            "an artifact must not be listed as its own neighbour"
        );
        assert!(d.related.len() <= RELATED_LIMIT);
    }

    #[tokio::test]
    async fn the_pane_lists_what_this_artifact_is_seen_together_with() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(
                &ids[0],
                &ids[1],
                5.0,
                Some("mount forensic image"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert_eq!(d.seen_together.len(), 1);
        assert_eq!(d.seen_together[0].id, ids[1]);
        assert_eq!(
            d.seen_together[0].why.as_deref(),
            Some("when asking: mount forensic image"),
            "an unjudged link explains itself with the question that bound it"
        );
    }

    #[tokio::test]
    async fn a_judged_link_shows_the_judges_line_instead_of_the_query() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                crate::store::links::LinkState::Related,
                Some("the tool and the error it prints"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert_eq!(
            d.seen_together[0].why.as_deref(),
            Some("the tool and the error it prints")
        );
    }

    #[tokio::test]
    async fn dismissing_a_link_takes_it_out_for_good_without_losing_the_evidence() {
        // The weight stays, so the decision is auditable; the state is final,
        // so it is never shown, judged or pruned again.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        app.clone()
            .oneshot(form(
                &format!("/ui/artifacts/{}/links/{}/dismiss", ids[0], ids[1]),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.state, crate::store::links::LinkState::Dismissed);
        assert!(
            l.weight > 0.0,
            "the evidence was thrown away with the decision"
        );
        assert!(
            build_artifact_detail(&core, &ids[0], "")
                .await
                .unwrap()
                .seen_together
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_pane_still_renders_when_the_links_cannot_be_read() {
        // The associative layer can only add. It is not a reason to refuse to
        // show an artifact beside its source.
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let ids = artifacts(&core, &["alpha text"]).await;
        sqlx::query("DROP TABLE artifact_links")
            .execute(&core.store.pool)
            .await
            .unwrap();
        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        assert!(d.seen_together.is_empty());
    }

    #[tokio::test]
    async fn a_cross_corpus_pair_is_marked_and_a_same_corpus_pair_is_not() {
        // "Two documents needing each other is the finding; two passages of one
        // document needing each other is not" is the whole point of the flag —
        // pin it on the data the pane renders, not on a CSS class name.
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        let ids = artifacts(&core, &["alpha text", "same corpus neighbour"]).await;
        let other_corpus = core.store.insert_corpus("y", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &other_corpus.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "body of other document".to_string(),
                    corpus_span: None,
                    title: Some("other document".to_string()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        let cross_id = made[0].id.clone();

        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q1"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .bump_link(
                &ids[0],
                &cross_id,
                5.0,
                Some("q2"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();

        let d = build_artifact_detail(&core, &ids[0], "").await.unwrap();
        let same = d
            .seen_together
            .iter()
            .find(|r| r.id == ids[1])
            .expect("the same-corpus pair should still be listed");
        let cross = d
            .seen_together
            .iter()
            .find(|r| r.id == cross_id)
            .expect("the cross-corpus pair should be listed");
        assert!(
            !same.cross_corpus,
            "two passages of one document is not the finding"
        );
        assert!(
            cross.cross_corpus,
            "two documents needing each other is the finding"
        );
    }

    #[tokio::test]
    async fn a_related_link_works_on_the_standalone_artifact_page() {
        // The detail partial is both the search pane's content and the whole of
        // `/ui/artifacts/{id}`. A neighbour link that named `#pane` would be
        // dead on the standalone page, which is the one a shared link opens.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get(&app, "/ui/search/results?q=alpha", &cookie).await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        let page = flat(&get(&app, &format!("/ui/artifacts/{id}"), &cookie).await);
        assert!(
            page.contains("Related"),
            "the standalone page must list neighbours"
        );
        assert!(
            !page.contains(r##"hx-target="#pane""##),
            "no pane exists on this page, so nothing may target one"
        );
        assert!(
            page.contains(r#"hx-target="closest [data-terms]""#),
            "a neighbour must swap the detail it is listed under"
        );
    }

    #[tokio::test]
    async fn a_lifecycle_button_comes_back_to_the_page_that_offered_it() {
        // These four actions are rendered both on Ops and on an artifact's own
        // page. Always redirecting to Ops threw a reader who pressed "Confirm
        // still accurate" while reading an artifact onto a queue they were not
        // working through.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get(&app, "/ui/search/results?q=alpha", &cookie).await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{id}/verify"),
                &cookie,
                &format!("to=/ui/artifacts/{id}"),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.headers().get("location").unwrap(),
            format!("/ui/artifacts/{id}").as_str()
        );

        // Ops sends no `to` and keeps the default.
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{id}/deprecate"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.headers().get("location").unwrap(), "/ui/ops");
    }

    #[tokio::test]
    async fn a_lifecycle_button_pressed_in_the_pane_swaps_the_artifact_not_the_page() {
        // The same fragment is the standalone page and the pane beside the
        // search results, and the hidden `to` can only name one of them. It
        // named the page, so pressing "Confirm still accurate" on a result
        // navigated the whole window there and took the results with it.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get(&app, "/ui/search/results?q=alpha", &cookie).await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        let mut req = form(
            &format!("/ui/ops/artifacts/{id}/verify"),
            &cookie,
            &format!("to=/ui/artifacts/{id}"),
        );
        req.headers_mut()
            .insert("hx-request", "true".parse().unwrap());
        let res = app.clone().oneshot(req).await.unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().get("location").is_none(),
            "a swap must not navigate"
        );
        let body = crate::web::test_support::body_of(res).await;
        assert!(
            body.contains(&format!(r#"data-artifact="{id}""#)),
            "the answer is the artifact, re-rendered: {body}"
        );
        // A fragment, not a whole page: the pane is inside one already.
        assert!(!body.contains("<nav"), "{body}");
    }

    #[tokio::test]
    async fn a_return_path_pointing_off_this_ui_is_ignored() {
        // The field is user input, and a redirect that follows anything handed
        // to it is an open redirect: worth nothing here, a phishing hop
        // everywhere else.
        let (app, cookie) = app_with_embedded_corpus().await;
        let rail = get(&app, "/ui/search/results?q=alpha", &cookie).await;
        let id = rail
            .split(r#"hx-get="/ui/artifacts/"#)
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.split('?').next())
            .expect("no result to open")
            .to_string();

        for hostile in ["https://evil.example/x", "//evil.example/x", "/ui//evil"] {
            let res = app
                .clone()
                .oneshot(form(
                    &format!("/ui/ops/artifacts/{id}/verify"),
                    &cookie,
                    &format!("to={}", urlencoding_of(hostile)),
                ))
                .await
                .unwrap();
            assert_eq!(
                res.headers().get("location").unwrap(),
                "/ui/ops",
                "followed {hostile}"
            );
        }
    }

    /// Percent-encoding for the handful of characters these test bodies carry.
    fn urlencoding_of(s: &str) -> String {
        s.replace(':', "%3A").replace('/', "%2F")
    }

    #[tokio::test]
    async fn an_artifact_that_is_not_embedded_yet_still_opens() {
        // Synthesis without the embed job: the pane has to show the artifact
        // beside its source and simply offer no neighbours.
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbravo", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .remove(0);

        let d = super::build_artifact_detail(&core, &c.id, "")
            .await
            .unwrap();
        assert!(d.related.is_empty());
        assert!(!d.html.is_empty(), "the artifact itself must still render");
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
            category: None,
            tags: vec![],
            score: 0.5,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via: None,
            reason: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        };

        let loose = render_hit(0, result(true), &Default::default());
        assert!(loose.weak);
        assert!(loose.rank.is_empty(), "a loose result was presented as #1");
        assert_eq!(render_hit(0, result(false), &Default::default()).rank, "#1");

        let html = askama::Template::render(&ResultsTemplate {
            results: vec![loose],
            associated: vec![],
            all_weak: true,
            terms: String::new(),
        })
        .unwrap();
        assert!(html.contains("Nothing matches closely"), "{html}");
        assert!(!html.contains("#1"), "{html}");
    }

    #[tokio::test]
    async fn a_verbatim_passage_card_keeps_the_lines_markdown_would_flatten() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .ingest("Dateiattribute\n.........24", "web", None)
            .await
            .unwrap();
        let na = |t: &str| crate::store::artifacts::NewArtifact {
            ordinal: 0,
            text: t.into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
        };
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[na("Dateiattribute\n.........24")],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let card = artifact_view(&core.store.get_artifact(&p[0].id).await.unwrap());
        assert!(card.html.contains("<pre"), "{}", card.html);
        assert!(
            card.html.contains("Dateiattribute\n.........24"),
            "{}",
            card.html
        );

        // And a model-written artifact is still markdown: it was written as
        // markdown, and reading it as plain text would show the syntax.
        let a = core
            .store
            .insert_artifacts(&src.id, &[na("## Heading\n\n- one")])
            .await
            .unwrap();
        let written = artifact_view(&core.store.get_artifact(&a[0].id).await.unwrap());
        assert!(written.html.contains("<h2>"), "{}", written.html);

        // The detail pane renders the same artifact and has to say the same
        // thing about it: it is the half of the search page that shows a
        // passage in full.
        let d = super::build_artifact_detail(&core, &p[0].id, "")
            .await
            .unwrap();
        assert!(d.html.contains("<pre"), "{}", d.html);
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
            category: None,
            tags: vec![],
            score: 0.5,
            status: None,
            superseded_by: None,
            last_verified_at: None,
            weak: false,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via: via.map(str::to_string),
            reason: None,
            model_written: false,
            synthesized: false,
            origin_count: 0,
        };
        let titles = super::ranked_titles(&[hit(None, None)]);
        assert!(
            titles.is_empty(),
            "an untitled hit must not lend its name to what it recalled: {titles:?}"
        );
        let r = render_hit(0, hit(None, None), &titles);
        assert!(r.title.is_empty(), "{:?}", r.title);
        let html = askama::Template::render(&ResultsTemplate {
            results: vec![r],
            associated: vec![render_hit(0, hit(None, Some("a")), &titles)],
            all_weak: false,
            terms: String::new(),
        })
        .unwrap();
        assert!(!html.contains("Untitled"), "{html}");
        assert!(!html.contains("rail-title"), "{html}");
    }

    fn rendered(via: Option<&str>, reason: Option<&str>) -> RenderedResult {
        RenderedResult {
            artifact_id: "a1".into(),
            title: "The one that was recalled".into(),
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: String::new(),
            weak: false,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via_title: via.map(str::to_string),
            reason: reason.map(str::to_string),
            model_written: false,
            origin_count: 0,
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
            associated: vec![],
            all_weak: false,
            terms: String::new(),
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
            associated: vec![],
            all_weak: false,
            terms: String::new(),
        }
        .render()
        .unwrap();
        assert!(!flat.contains("Relevance falls off here"), "{flat}");
        assert!(!flat.contains("rail-past"), "{flat}");
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
            results: vec![],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            all_weak: false,
            terms: String::new(),
        };
        let body = template.render().unwrap();
        assert!(body.contains("Recalled by association"), "{body}");
        assert!(body.contains("seen together with"), "{body}");
        assert!(body.contains("Mounting E01 images"), "{body}");

        // A judged link says what the relation is instead of what was asked.
        let judged = ResultsTemplate {
            results: vec![],
            associated: vec![rendered(
                Some("Mounting E01 images"),
                Some("the tool and its errors"),
            )],
            all_weak: false,
            terms: String::new(),
        };
        let body = judged.render().unwrap();
        assert!(body.contains("the tool and its errors"), "{body}");
        assert!(!body.contains("seen together with"), "{body}");
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

    fn ranked(weak: bool) -> RenderedResult {
        RenderedResult {
            artifact_id: "r1".into(),
            title: "The ranked hit".into(),
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: if weak { String::new() } else { "#1".into() },
            weak,
            primed: false,
            in_sitting: false,
            past_cliff: false,
            via_title: None,
            reason: None,
            model_written: false,
            origin_count: 0,
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
            terms: String::new(),
        };
        let body = weak_with_association.render().unwrap();
        assert!(
            body.contains("Nothing matches closely"),
            "an association hid a real warning: {body}"
        );

        let good_with_association = ResultsTemplate {
            results: vec![ranked(false)],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            all_weak: false,
            terms: String::new(),
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
        let search = include_str!("templates/search.html");
        assert!(
            search.contains(r#"class="label facet-label""#),
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
        let tpl = include_str!("templates/_artifact_detail.html");
        for word in ["Verified", "Hide", "Delete"] {
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
            associated: vec![],
            all_weak: false,
            terms: String::new(),
        }
        .render()
        .unwrap();
        assert!(body.contains("rail-why"), "no provenance line: {body}");
        assert!(body.contains("you reach this one often"), "{body}");
    }

    #[test]
    fn an_ordinary_hit_explains_nothing() {
        // A line under every result saying "this matched your query" is noise
        // that makes the lines worth reading harder to see.
        let body = ResultsTemplate {
            results: vec![ranked(false)],
            associated: vec![],
            all_weak: false,
            terms: String::new(),
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
            associated: vec![],
            all_weak: false,
            terms: String::new(),
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

    #[test]
    fn timestamps_render_as_a_readable_date() {
        // 2026-08-09T07:00:00Z
        assert_eq!(fmt_time(1_775_631_600), "2026-04-08 07:00");
        assert_eq!(fmt_time(0), "1970-01-01 00:00");
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
            "/ui/ops",
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
    async fn capturing_text_stores_it_and_says_nothing() {
        // The confirmation is the row that appears under "Recent" — same link,
        // same progress badge. A card above the list saying it again was the
        // one capture reported twice.
        let (app, cookie, core) = app_session_and_core().await;
        let res = app
            .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            body_of(res).await.trim().is_empty(),
            "an ordinary capture repeats what the queue already shows"
        );
        assert_eq!(
            core.store.list_corpora(10, 0).await.unwrap().len(),
            1,
            "the capture itself still landed"
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
        assert!(
            !page("/ui/search").await.contains("load"),
            "an empty box has nothing to search for"
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
            body.contains("Fake title: alpha line"),
            "once synthesis names it, the row is called what it is"
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

        let body = get_body(&app, &cookie, "/ui/capture").await;
        assert_eq!(
            body.matches("/supersede").count(),
            super::PAIR_LIMIT * 2,
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
    async fn source_detail_shows_the_raw_text() {
        let (app, cookie, core) = app_session_and_core().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // An ordinary capture answers with nothing to read the id out of — the
        // queue fragment is what names it on the page.
        let id = core.store.list_corpora(10, 0).await.unwrap()[0].id.clone();

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/corpora/{id}"))
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("alpha para"));
    }

    #[tokio::test]
    async fn editing_a_missing_chunk_is_a_404() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/artifacts/missing")
                    .method("PUT")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("text=edited"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
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

        let html = get(&app, "/ui/ops", &cookie).await;
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
                    .uri("/ui/ops")
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

        let page = get_body(&app, &cookie, "/ui/ops").await;
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
                    .uri("/ui/ops")
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

    /// One corpus with `n` artifacts, titled so the ops page can be searched
    /// for them.
    async fn artifacts(core: &crate::core::Core, titles: &[&str]) -> Vec<String> {
        let src = core.store.insert_corpus("x", "web", None).await.unwrap();
        let new: Vec<crate::store::artifacts::NewArtifact> = titles
            .iter()
            .enumerate()
            .map(|(i, t)| crate::store::artifacts::NewArtifact {
                ordinal: i as i64,
                text: format!("body of {t}"),
                corpus_span: None,
                title: Some((*t).to_string()),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        core.store
            .insert_artifacts(&src.id, &new)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    #[tokio::test]
    async fn ops_lists_a_superseded_artifact_and_can_undo_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["the loser", "the keeper"]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

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
        let html = body_of(res).await;
        assert!(
            html.contains("the loser") && html.contains("the keeper"),
            "the superseded artifact is not listed"
        );

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{}/unsupersede", ids[0]),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "undo did not clear the flag"
        );
    }

    #[tokio::test]
    async fn a_contradiction_the_judge_could_not_call_is_still_resolvable() {
        // The dead end this fixes: the judge finds two artifacts stating a
        // detail differently but names no winner, so `obsolete_id` is NULL. The
        // row then offered nothing but Dismiss — an operator who could see which
        // one was right had no way to say so, and clearing the queue meant
        // declaring the disagreement uninteresting and leaving both in results.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Contradiction,
                Some("they disagree about the tag"),
            )
            .await
            .unwrap();
        assert!(
            core.store
                .get_pair(pair.id)
                .await
                .unwrap()
                .obsolete_id
                .is_none(),
            "this test is only meaningful with no judge proposal to fall back on"
        );

        // Keep the first; the second is the one that gets hidden.
        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/supersede", pair.id),
                &cookie,
                &format!("keep={}", pair.a_id),
            ))
            .await
            .unwrap();

        let kept = core.store.get_artifact(&pair.a_id).await.unwrap();
        let hidden = core.store.get_artifact(&pair.b_id).await.unwrap();
        assert_eq!(kept.status, crate::store::artifacts::ArtifactStatus::Active);
        assert_eq!(
            hidden.status,
            crate::store::artifacts::ArtifactStatus::Superseded
        );
        assert_eq!(hidden.superseded_by.as_deref(), Some(pair.a_id.as_str()));
    }

    #[tokio::test]
    async fn keeping_an_artifact_from_outside_the_pair_is_refused() {
        // `keep` is a form field, so it is user input. Superseding whatever id
        // arrives would hide an artifact that has nothing to do with the row
        // that was pressed.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one", "unrelated"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/supersede", pair.id),
                &cookie,
                &format!("keep={}", ids[2]),
            ))
            .await
            .unwrap();

        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                crate::store::artifacts::ArtifactStatus::Active,
                "an artifact outside the pair was touched"
            );
        }
    }

    #[tokio::test]
    async fn capture_lists_a_pending_pair_and_can_dismiss_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        // On Capture, not on Housekeeping: this is the one part of Ops that
        // needs a person, so it belongs where the work arrives.
        let html = get_body(&app, &cookie, "/ui/capture").await;
        assert!(html.contains("left one") && html.contains("right one"));
        assert!(
            html.contains("Keep “left one”"),
            "each button names the artifact it keeps"
        );

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/dismiss", pair.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_counts_say_what_they_count() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/ops").await;
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

        let page = get_body(&app, &cookie, "/ui/ops").await;
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

        let page = get_body(&app, &cookie, "/ui/ops").await;
        assert!(
            page.contains("body of Windows Update-Typen"),
            "a row has to say which artifact it is, and two can share a title: {page}"
        );
    }

    #[tokio::test]
    async fn two_tokens_with_one_name_are_still_tellable_apart() {
        // The extension mints every token under the same name, so two rows
        // called "browser extension" and neither used yet were the same row
        // twice — and one of them was the one currently working.
        let (app, cookie, core) = app_session_and_core().await;
        crate::auth::tokens::mint(
            &core.store,
            "browser extension",
            "user-1",
            Some("Firefox/141.0"),
        )
        .await
        .unwrap();
        crate::auth::tokens::mint(
            &core.store,
            "browser extension",
            "user-1",
            Some("Chrome/152.0"),
        )
        .await
        .unwrap();

        let page = get_body(&app, &cookie, "/ui/settings").await;
        assert!(page.contains("Firefox"), "{page}");
        assert!(page.contains("Chrome"), "{page}");
    }

    #[tokio::test]
    async fn the_installation_lives_on_its_own_page() {
        let (app, cookie) = app_with_session().await;

        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(settings.contains("API tokens"), "{settings}");
        assert!(settings.contains("Browser extension"), "{settings}");

        let ops = get_body(&app, &cookie, "/ui/ops").await;
        assert!(
            !ops.contains("API tokens"),
            "housekeeping is about the corpus: {ops}"
        );
        assert!(!ops.contains("Browser extension"), "{ops}");
    }

    #[tokio::test]
    async fn both_pages_are_reachable_from_capture() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("/ui/ops"), "{page}");
        assert!(page.contains("/ui/settings"), "{page}");
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
        assert!(
            !frag.contains("embed ") && !frag.contains("hx-swap-oob"),
            "timing is not operator-facing: {frag}"
        );
    }

    #[tokio::test]
    async fn every_result_carries_the_id_the_selection_handler_matches_on() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let frag = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(
            frag.contains(r#"role="option" aria-selected="false""#),
            "{frag}"
        );
        assert!(frag.contains("/ui/artifacts/"), "{frag}");
    }

    #[tokio::test]
    async fn tags_are_stored_and_filterable_but_never_rendered() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].clone();
        core.store
            .update_artifact_tags(&c.id, &["forensik".into()])
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{}", c.id)).await;
        assert!(
            !page.contains("forensik"),
            "no chips on the artifact: {page}"
        );

        let search = get_body(&app, &cookie, "/ui/search").await;
        assert!(
            !search.contains(r#"aria-label="Tag""#),
            "no tag facet row: {search}"
        );

        // Still true, still stored, still the channel pinning rides on.
        assert_eq!(
            core.store.get_artifact(&c.id).await.unwrap().tags,
            vec!["forensik".to_string()]
        );
    }

    #[tokio::test]
    async fn a_capture_still_being_read_names_no_loss_and_offers_no_re_read() {
        use crate::store::segments::NewSegment;
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha beta\ngamma delta", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[NewSegment {
                    start_line: 1,
                    end_line: 2,
                    text: "alpha beta\ngamma delta",
                    carry_lines: 0,
                }],
            )
            .await
            .unwrap();
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Segmenting)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            !page.contains(r#"id="uncovered""#),
            "an unread window was named as a loss: {page}"
        );
        assert!(!page.contains("Read these again"), "{page}");

        // And the form behind that button, reached directly, arms nothing.
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=6",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            !core
                .store
                .live_job(
                    crate::store::jobs::Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(&out.id, 0)
                )
                .await
                .unwrap(),
            "a window that had not been read yet was queued to be read again"
        );
    }

    /// `enqueue` re-arms a conflicting row whatever state it is in, running
    /// included. Pressing the button twice therefore handed one window to two
    /// workers: two paid model calls and two sets of artifacts for one loss.
    #[tokio::test]
    async fn a_window_already_queued_is_not_re_read_a_second_time() {
        use crate::store::segments::{NewSegment, SegmentState};
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest(
                "alpha beta\ngamma delta\nomega sigma\nkappa lambda",
                "web",
                None,
            )
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[
                    NewSegment {
                        start_line: 1,
                        end_line: 2,
                        text: "alpha beta\ngamma delta",
                        carry_lines: 0,
                    },
                    NewSegment {
                        start_line: 3,
                        end_line: 4,
                        text: "omega sigma\nkappa lambda",
                        carry_lines: 0,
                    },
                ],
            )
            .await
            .unwrap();
        for idx in [0, 1] {
            core.store
                .set_segment_state(&out.id, idx, SegmentState::Done, None)
                .await
                .unwrap();
        }
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Partial)
            .await
            .unwrap();
        // The first window is already on its way — an earlier press of the same
        // button, or the read that is about to fill it.
        core.store
            .enqueue(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(&out.id, 0),
            )
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=6",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let states: Vec<SegmentState> = core
            .store
            .segments_for_corpus(&out.id)
            .await
            .unwrap()
            .iter()
            .map(|w| w.state)
            .collect();
        assert_eq!(
            states,
            vec![SegmentState::Done, SegmentState::Pending],
            "the window already queued was reset under the worker holding it"
        );
    }

    #[tokio::test]
    async fn a_loss_crossing_a_window_boundary_re_reads_both_windows() {
        // Uncovered lines are merged into one range across everything lost in
        // a row, and nothing stops that run at a window boundary. Matching the
        // range's first line alone re-read the window the loss opened in and
        // left the rest of it exactly as it was.
        use crate::store::segments::{NewSegment, SegmentState};
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("one\ntwo\nthree\nfour\nfive\nsix", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[
                    NewSegment {
                        start_line: 1,
                        end_line: 3,
                        text: "one\ntwo\nthree",
                        carry_lines: 0,
                    },
                    NewSegment {
                        start_line: 4,
                        end_line: 6,
                        text: "four\nfive\nsix",
                        carry_lines: 0,
                    },
                ],
            )
            .await
            .unwrap();
        // Both settled and neither producing an artifact: the whole document
        // is one uncovered range spanning both windows.
        for idx in [0, 1] {
            core.store
                .set_segment_state(&out.id, idx, SegmentState::Done, None)
                .await
                .unwrap();
        }
        // And the capture itself has finished being read — `partial` is what
        // synthesis sets for a document whose windows resolved without
        // covering it, and `coverage_final` requires it before naming a loss.
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Partial)
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=6",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let pending: Vec<i64> = core
            .store
            .pending_segments(&out.id)
            .await
            .unwrap()
            .iter()
            .map(|w| w.idx)
            .collect();
        assert_eq!(pending, vec![0, 1], "the tail of the loss was left unread");
    }

    #[tokio::test]
    async fn a_fully_covered_corpus_marks_nothing_red() {
        // The anchor still exists — the Recent warning follows it, and it has
        // to land on the sentence that explains why nothing is marked. What a
        // fully claimed corpus has is no red band.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha beta gamma", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(!page.contains("band-gap"), "nothing was missed: {page}");
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
    async fn a_low_coverage_row_links_to_the_lines_that_were_missed() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // After embedding, which is what settles the corpus and recomputes the
        // real coverage — this is the reading the row has to warn about.
        core.store
            .set_corpus_coverage(&out.id, Some(0.31))
            .await
            .unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains(&format!("/ui/corpora/{}#uncovered", out.id)),
            "a warning has to lead somewhere: {frag}"
        );
        assert!(frag.contains("qcov-low"), "{frag}");
    }

    #[tokio::test]
    async fn a_low_row_with_no_windows_warns_without_linking() {
        // A capture read before per-segment windows existed. Its coverage is
        // still measured — against the whole document — but nothing can say
        // which lines were lost, so `#uncovered` renders nothing and the
        // warning must not send anyone there.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        core.store.clear_segments(&out.id).await.unwrap();
        core.store
            .set_corpus_coverage(&out.id, Some(0.31))
            .await
            .unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains("qcov-low"),
            "the reading is still worth warning about: {frag}"
        );
        assert!(
            !frag.contains(&format!("/ui/corpora/{}#uncovered", out.id)),
            "linked to a section that renders nothing: {frag}"
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

        let page = get_body(&app, &cookie, "/ui/capture").await;
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

    #[tokio::test]
    async fn minting_a_token_shows_the_plaintext_exactly_once() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .clone()
            .oneshot(form("/ui/ops/tokens", &cookie, "name=claude-code"))
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("engram_"),
            "the token must be shown once: {html}"
        );

        // It is not recoverable from any later page. Settings, not Housekeeping:
        // that is the page the token table moved to, and asserting against a
        // page that renders no tokens at all asserts nothing.
        let page = body_of(
            app.oneshot(
                Request::builder()
                    .uri("/ui/settings")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(
            page.contains("claude-code"),
            "the minted token's row must be on the page this asserts against: {page}"
        );
        assert!(
            !page.contains("engram_"),
            "a stored token leaked into the settings page"
        );
    }

    /// A feedback-enabled session over an embedded base, an ask on it, and the
    /// recorded event id. Built like `app_with_embedded_corpus`: synthesis and
    /// embedding are run on the core before the router takes it, because a
    /// capture through the page alone leaves nothing to retrieve.
    async fn ask_recorded() -> (axum::Router, String, crate::core::Core, String, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
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
            .fetch_one(&core.store.pool)
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
    async fn the_capture_page_lists_knowledge_gaps_by_group_and_lets_one_be_covered() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        // Two, because one gap is not a group: the sweep leaves a lone question
        // ungrouped rather than buying a name that restates it.
        let mut ids = Vec::new();
        for q in ["how do I mount an E01", "mounting E01 images read only"] {
            let id = core
                .store
                .record_ask(crate::store::asks::NewAsk {
                    question: q.into(),
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; 8],
                    embed_model: core.embedder.model().to_string(),
                    answer: "Not in the knowledge base.".into(),
                    abstained: true,
                    dropped: 0,
                    truncated: false,
                    citations: vec![],
                })
                .await
                .unwrap();
            core.store
                .judge_ask(&id, crate::store::asks::AskVerdict::NothingHere)
                .await
                .unwrap();
            ids.push(id);
        }
        let id = ids[0].clone();

        // Before the sweep: listed, not yet grouped.
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("Knowledge gaps"), "{page}");
        assert!(page.contains("not yet grouped"), "{page}");
        assert!(page.contains("mount an E01"), "{page}");

        crate::jobs::gaps::sweep(&core).await.unwrap();
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("Fake topic"), "{page}");
        assert!(
            page.contains(&format!("/ui/gaps/ask/{id}/dismiss")),
            "{page}"
        );
        assert!(page.contains("/ui/ask?q=how"), "{page}");

        for id in &ids {
            let res = app
                .clone()
                .oneshot(form(&format!("/ui/gaps/ask/{id}/dismiss"), &cookie, ""))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            !page.contains("Knowledge gaps"),
            "a covered gap must leave the page: {page}"
        );
    }

    #[tokio::test]
    async fn a_capture_that_answered_something_says_so_on_its_row() {
        // Coverage is closed silently — nothing asked the operator to confirm
        // it — so the queue row is the only place it is said.
        let mut c = crate::core::test_support::test_core().await;
        c.feedback.enabled = true;
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "mounting an E01".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
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
                    query: "how do I mount an E01".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; crate::core::test_support::TEST_DIM],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(&gap, crate::store::feedback::Verdict::Gap)
            .await
            .unwrap();
        core.store
            .cover_gap(crate::store::gaps::GapKind::Search, &gap, &src.id, &a, 0.71)
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
    async fn a_query_typed_on_search_arrives_in_the_ask_box() {
        // Two pages with nothing carried between them cost a retyped query
        // every time. Nothing here changes an order.
        let (app, cookie, core) = app_session_and_core().await;
        get_body(
            &app,
            &cookie,
            "/ui/search/results?q=how%20do%20I%20mount%20an%20E01",
        )
        .await;

        let ask = get_body(&app, &cookie, "/ui/ask").await;
        assert!(
            ask.contains("how do I mount an E01"),
            "the query was not carried: {ask}"
        );
        let _ = core;
    }

    #[tokio::test]
    async fn a_question_the_operator_arrived_with_is_never_overwritten() {
        // A gap's "ask again" is a question they chose. The sitting fills an
        // empty box and nothing else.
        let (app, cookie, _core) = app_session_and_core().await;
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
    async fn what_this_sitting_opened_is_a_way_back_to_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "mounting an E01".into(),
                    corpus_span: None,
                    title: Some("Mounting an E01".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();

        get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;

        let page = get_body(&app, &cookie, "/ui/search").await;
        assert!(page.contains("Read just now"), "{page}");
        assert!(page.contains("Mounting an E01"), "{page}");

        // And it is still there after a search. The filter form replaces what
        // it targets wholesale, so a sitting inside that target was wiped by
        // the first keystroke and never came back — visible only on a search
        // page with no query, which is the one moment it has nothing to say.
        let form = page
            .split("<form id=\"filters\"")
            .nth(1)
            .expect("the search page has a filter form");
        let target = form
            .split("hx-target=\"")
            .nth(1)
            .and_then(|t| t.split('"').next())
            .expect("the filter form names a target");
        let swapped = page
            .split(&format!("id=\"{}\"", target.trim_start_matches('#')))
            .nth(1)
            .expect("the target is on the page");
        assert!(
            !swapped.contains("Read just now"),
            "a search replaces {target}, and the sitting is inside it: {swapped}"
        );
    }

    #[tokio::test]
    async fn with_priming_off_the_sitting_moves_no_result() {
        // The default. Carrying ships on because it changes no order; this is
        // the part that does, and it waits for the harness.
        let mut c = crate::core::test_support::test_core().await;
        c.feedback.enabled = true;
        assert!(!c.sitting.prime, "priming must ship off");
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let ids: Vec<String> = core
            .store
            .insert_artifacts(
                &src.id,
                &["alpha one", "alpha two", "alpha three", "alpha four"]
                    .iter()
                    .enumerate()
                    .map(|(i, t)| crate::store::artifacts::NewArtifact {
                        ordinal: i as i64,
                        text: (*t).into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(0),
                        caveats: vec![],
                    })
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        crate::jobs::embed::run_corpus(&core, &src.id)
            .await
            .unwrap();

        let before = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        // Read the last one this list returns, then search again.
        for id in &ids {
            get_body(&app, &cookie, &format!("/ui/artifacts/{id}")).await;
        }
        let after = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;

        let rank_of = |html: &str| -> Vec<String> {
            html.match_indices("/ui/artifacts/")
                .map(|(i, _)| html[i + 14..i + 50].to_string())
                .collect()
        };
        assert_eq!(
            rank_of(&before),
            rank_of(&after),
            "the sitting moved a result with priming off"
        );
    }

    #[tokio::test]
    async fn the_sitting_writes_no_activation() {
        // The guard most likely to be lost to a refactor: the sitting is a
        // *read* of what is happening. Writing activation from it would be a
        // loop that reinforces itself, which is the failure mode this whole
        // area is built to close.
        let mut c = crate::core::test_support::test_core().await;
        c.feedback.enabled = true;
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "mounting an E01".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        // Whatever opening it records, record it now, before the sitting is
        // asked for anything.
        get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        core.background.wait_idle().await;
        let before = core
            .store
            .activation_of(std::slice::from_ref(&a))
            .await
            .unwrap();

        // Reading the sitting, repeatedly, from both pages.
        for _ in 0..3 {
            get_body(&app, &cookie, "/ui/search").await;
        }
        core.background.wait_idle().await;

        assert_eq!(
            core.store.activation_of(&[a]).await.unwrap(),
            before,
            "reading the sitting moved an activation"
        );
    }

    #[tokio::test]
    async fn every_kind_of_gap_says_which_kind_it_is() {
        // Four ways of saying the base did not answer, on one list. They are
        // not the same claim, and an operator reading the list can tell them
        // apart.
        let mut c = crate::core::test_support::test_core().await;
        c.feedback.enabled = true;
        // The fake embedder's vectors are not a semantic space, so the shipped
        // threshold would call everything weak. A line above what the
        // candidate below scores and below nothing else.
        c.weak_below = 0.5;
        let core = c.clone();
        let (app, cookie) = app_with_cookie(c).await;
        // Judged a gap.
        let judged = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    query: "judged one".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; crate::core::test_support::TEST_DIM],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(&judged, crate::store::feedback::Verdict::Gap)
            .await
            .unwrap();
        // Nothing came close.
        core.store
            .record_search(
                crate::store::feedback::NewEvent {
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
                    }],
                    answered: false,
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

        let html = get_body(&app, &cookie, "/ui/capture").await;
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
        core.feedback.enabled = true;
        core.pursuit.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let core = handle;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let art = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "how to mount an E01".into(),
                    corpus_span: None,
                    title: Some("Mounting an E01".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
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

        let both = get_body(&app, &cookie, "/ui/ops").await;
        assert!(both.contains("2 went unanswered"), "{both}");

        // A later capture answers one of them.
        core.store
            .cover_gap(
                crate::store::gaps::GapKind::Pursuit,
                &ids[0],
                &src.id,
                &art,
                0.8,
            )
            .await
            .unwrap();

        let one = get_body(&app, &cookie, "/ui/ops").await;
        assert!(one.contains("1 went unanswered"), "{one}");
        assert!(one.contains("is\n  <a href=\"/ui/capture#gaps\""), "{one}");
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

        let note = page
            .find(r#"name="note""#)
            .expect("the note input is on the page");
        // The button, not the nav link of the same name above it.
        let button = page
            .find(r#"type="submit" form="capture""#)
            .expect("the capture button is there");
        assert!(
            note < button,
            "the note field must precede the button that sits under it: {page}"
        );

        // Outside the posted form still: that form posts urlencoded and the
        // file this note describes goes multipart to a different endpoint.
        assert!(page.contains(r#"form="capture""#), "{page}");

        // The order the work happens in: the file arrives and is held, the note
        // says what it is, the button sends both. A photo taken on a phone used
        // to upload the instant the camera handed it back, which put the note
        // after the send and made it unfillable.
        let staged = page
            .find(r#"id="staged""#)
            .expect("a file waits to be sent rather than going on arrival");
        assert!(
            staged < note,
            "what is waiting must sit above the note describing it: {page}"
        );
    }

    #[tokio::test]
    async fn re_reading_one_passage_leaves_the_other_windows_alone() {
        let (app, cookie, core) = app_session_and_core().await;
        // Long enough to be several windows, so "one of them" is meaningful.
        let body = (1..=400)
            .map(|i| format!("line {i} of the document"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        // Settled, or the endpoint rightly refuses: lines a capture has not
        // been read to the end of are not lines it lost. Set directly because
        // this test is about where a re-read is aimed, not about the pipeline
        // that gets a document to Ready.
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Ready)
            .await
            .unwrap();
        let segments = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(segments.len() > 1, "the fixture must span several windows");
        let target = segments[0].clone();
        assert!(target.end_line > 3, "the loss below must fit inside it");

        // One loss, lines 2–3, and it has to be a real one: the endpoint cuts
        // the bands again rather than believing the range in the form, so a
        // fixture where nothing was missed queues nothing however it is aimed.
        // Two artifacts around the gap, written directly, because what this
        // test is about is where the button points.
        let total = core
            .store
            .get_corpus(&out.id)
            .await
            .unwrap()
            .raw_text
            .lines()
            .count() as i64;
        sqlx::query("DELETE FROM artifacts WHERE corpus_id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        let claim = |ord: i64, a: i64, z: i64| crate::store::artifacts::NewArtifact {
            ordinal: ord,
            text: format!("what lines {a} to {z} said"),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: a,
                end_line: z,
            }),
            title: Some(format!("artifact {ord}")),
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        };
        core.store
            .insert_artifacts(&out.id, &[claim(0, 1, 1), claim(1, 4, total)])
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                &format!("from={}&to={}", target.start_line, target.end_line),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let pending = core.store.pending_segments(&out.id).await.unwrap();
        assert_eq!(
            pending.iter().map(|w| w.idx).collect::<Vec<_>>(),
            vec![target.idx],
            "exactly the window holding that line, and no other"
        );
    }

    #[tokio::test]
    async fn re_reading_a_line_in_no_window_is_not_a_500() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=99999&to=99999",
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::SEE_OTHER,
            "nothing to do is not an error"
        );
    }

    #[tokio::test]
    async fn the_corpus_page_puts_each_passage_beside_what_came_of_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("band"), "the page is banded: {page}");
        // The old two-lists arrangement is gone.
        assert!(!page.contains("Raw corpus"), "{page}");
        assert!(!page.contains("<h3>Artifacts</h3>"), "{page}");
        // Every line keeps the anchor an artifact's "open at these lines" uses.
        assert!(page.contains(r#"id="L1""#), "{page}");
    }

    #[tokio::test]
    async fn an_unclaimed_passage_is_a_gap_band_with_its_own_button() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        // Settled, or nothing is a loss yet: a capture still being read has
        // lines nothing claims because nothing has got to them.
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Pull every span back onto line 1, leaving the rest of the document
        // claimed by nobody. Written straight to the column because nothing in
        // the store edits a span — synthesis computes it and is the only
        // writer, which is right everywhere except here.
        sqlx::query(
            r#"UPDATE artifacts SET corpus_span = '{"start_line":1,"end_line":1}' WHERE corpus_id = ?"#,
        )
        .bind(&out.id)
        .execute(&core.store.pool)
        .await
        .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains("band-gap"),
            "the unclaimed run is red: {page}"
        );
        assert!(
            page.contains(r#"name="from""#),
            "a gap band carries a re-read button naming its first line: {page}"
        );
        assert!(
            page.contains("reads lines"),
            "the button says what it will actually read, which is the whole \
             window and so wider than the band: {page}"
        );
    }

    #[tokio::test]
    async fn a_restored_corpus_is_not_banded() {
        // Its "source" is its own artifacts joined back together, so a span
        // into it is a claim the arrangement cannot support.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        sqlx::query("UPDATE corpora SET restored_at = 1 WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("Placeholder source"), "{page}");
        assert!(!page.contains("band-gap"), "{page}");
    }

    /// Only a photo waits on the vision model. Any other capture with no text
    /// is a fetch that came back empty or a paste that was, and the fallback
    /// told the operator a job was queued that nobody had started.
    #[tokio::test]
    async fn an_empty_capture_that_is_not_a_photo_names_no_vision_job() {
        let (app, cookie, core) = app_session_and_core().await;
        let s = core.store.insert_corpus("", "web", None).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", s.id)).await;
        assert!(!page.contains("vision model"), "{page}");
        assert!(page.contains("has no text"), "{page}");
    }

    /// Not banded is not the same as not shown. A placeholder's artifacts are
    /// the only thing it holds, and banding alone left them off their own page.
    #[tokio::test]
    async fn a_restored_corpus_still_shows_its_artifacts() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        let restored = core
            .store
            .insert_artifacts(
                &out.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "what the vector store still had".into(),
                    corpus_span: None,
                    title: Some("recovered".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        sqlx::query("UPDATE corpora SET restored_at = 1 WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains(&format!(r#"id="artifact-{restored}""#)),
            "the placeholder's only content has no card on its own page: {page}"
        );
    }

    /// Rendering bands alone dropped it: banding places an artifact by its
    /// span, and an artifact without one was placed nowhere and shown nowhere
    /// — off the only page that can edit or delete it.
    #[tokio::test]
    async fn an_artifact_naming_no_lines_still_has_its_card() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // One artifact from before spans were recorded. Written straight to the
        // column for the same reason `an_unclaimed_passage_...` does: synthesis
        // is the only writer of a span, which is right everywhere except here.
        sqlx::query(
            "UPDATE artifacts SET corpus_span = NULL
              WHERE id = (SELECT id FROM artifacts WHERE corpus_id = ? LIMIT 1)",
        )
        .bind(&out.id)
        .execute(&core.store.pool)
        .await
        .unwrap();
        let orphan = sqlx::query_scalar::<_, String>(
            "SELECT id FROM artifacts WHERE corpus_id = ? AND corpus_span IS NULL",
        )
        .bind(&out.id)
        .fetch_one(&core.store.pool)
        .await
        .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains("Not placed in the source"),
            "an artifact of this capture is on no page at all: {page}"
        );
        assert!(
            page.contains(&format!(r#"id="artifact-{orphan}""#)),
            "the card is what carries edit and delete: {page}"
        );
    }

    /// It may well have been written from exactly the lines about to be painted
    /// red, and the page would be offering a paid re-read on the strength of a
    /// claim it cannot make.
    #[tokio::test]
    async fn nothing_is_a_loss_while_an_artifact_names_no_lines() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Every span gone: under the old rule the whole document is one red
        // band with a button on it, though every artifact of it still exists.
        sqlx::query("UPDATE artifacts SET corpus_span = NULL WHERE corpus_id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(!page.contains("band-gap"), "{page}");
        assert!(!page.contains(r#"name="from""#), "{page}");

        // And the endpoint behind the button agrees, whatever range reaches it.
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=999999",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            core.store
                .pending_segments(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "a capture nothing is known to have missed was queued to be re-read"
        );
    }

    /// The form carries no token and the range is a claim, not a fact. Taking
    /// it at its word, one POST reset and re-enqueued every window of the
    /// capture — a paid model call each, for lines nothing was missing from.
    #[tokio::test]
    async fn a_re_read_of_a_range_that_lost_nothing_queues_nothing() {
        let (app, cookie, core) = app_session_and_core().await;
        let body = (1..=400)
            .map(|i| format!("line {i} of the document"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        core.store
            .set_corpus_status(&out.id, CorpusStatus::Ready)
            .await
            .unwrap();
        let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
        assert!(windows > 1, "the fixture must span several windows");

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=1&to=999999",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            core.store
                .pending_segments(&out.id)
                .await
                .unwrap()
                .is_empty(),
            "every window of a fully claimed capture was queued to be read again"
        );
    }

    /// Rendered in each band it touches, one artifact appeared three times
    /// under one set of element ids: "edit" on the second copy opened the
    /// editor of the first, and delete swapped the first away and left the
    /// others pointing at a row that no longer exists.
    #[tokio::test]
    async fn an_overlapping_artifact_has_exactly_one_card() {
        use crate::store::artifacts::{CorpusSpan, NewArtifact};
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("one\ntwo\nthree\nfour\nfive\nsix", "web", None)
            .await
            .unwrap();
        let art = |ord: i64, a: i64, z: i64| NewArtifact {
            ordinal: ord,
            text: format!("what lines {a} to {z} said"),
            corpus_span: Some(CorpusSpan {
                start_line: a,
                end_line: z,
            }),
            title: Some(format!("artifact {ord}")),
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        };
        // Wide, and one inside it: three bands, and the wide one claims all
        // three.
        let wide = core
            .store
            .insert_artifacts(&out.id, &[art(0, 1, 6), art(1, 3, 4)])
            .await
            .unwrap()[0]
            .id
            .clone();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert_eq!(
            page.matches(&format!(r#"id="artifact-{wide}""#)).count(),
            1,
            "one artifact, one card, one set of element ids: {page}"
        );
        assert!(
            page.contains(&format!(r##"href="#artifact-{wide}""##)),
            "the later bands it claims still point at it: {page}"
        );
    }

    #[tokio::test]
    async fn the_page_states_the_coverage_the_recent_list_warned_about() {
        // The two measures answer different questions and can disagree: every
        // line claimed, and still only half the wording carried. Following the
        // warning must not land on a page with nothing to see.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        core.store
            .set_corpus_coverage(&out.id, Some(0.55))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(
            page.contains(r#"id="uncovered""#),
            "the anchor still lands: {page}"
        );
        assert!(page.contains("55%"), "{page}");
    }

    #[tokio::test]
    async fn the_nav_is_the_same_width_on_every_page() {
        let (app, cookie) = app_with_session().await;
        for uri in ["/ui/capture", "/ui/search", "/ui/ops"] {
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
    async fn a_page_declares_what_it_holds_rather_than_how_wide_it_is() {
        // The three shell widths are gone. What is left is a statement about
        // content: a rail beside an artifact beside its source, prose at a
        // reading measure, or a table that is as wide as its columns need.
        // Every one of them starts at the shell's left edge, which is what
        // stops the content column moving as you navigate.
        let (app, cookie, core) = app_session_and_core().await;

        let search = get_body(&app, &cookie, "/ui/search").await;
        assert!(
            search.contains("regions-rail-focus-source"),
            "search is a three-region page: {search}"
        );

        let ops = get_body(&app, &cookie, "/ui/ops").await;
        assert!(
            ops.contains("regions-table"),
            "housekeeping is a table and has no reading measure: {ops}"
        );

        // `regions-focus-aside`, not `regions-focus`: a substring check would
        // pass on either, and the difference is the whole point — the prose
        // keeps its measure and what used to be empty beside it holds the
        // page's second thing.
        let capture = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            capture.contains(r#"regions regions-focus-aside"#),
            "capture is prose beside its record: {capture}"
        );
        assert!(
            capture.contains(r#"class="region-aside""#),
            "capture's Recent has nothing to sit in: {capture}"
        );

        let ask = get_body(&app, &cookie, "/ui/ask").await;
        assert!(
            ask.contains(r#"regions regions-focus-aside"#),
            "ask is an answer beside what it was written from: {ask}"
        );
        // The excerpts must not be a rail region. `r` is gated on one, and
        // reading mode rewrites the grid to a spine beside a single column —
        // which would take this page apart on a key that means nothing here.
        assert!(
            !ask.contains("region-rail"),
            "ask's excerpts would answer to reading mode: {ask}"
        );

        // No measure on the one page whose whole subject is an artifact and
        // the lines it came from — it is the same split the search pane holds,
        // so it gets the same room rather than a reading column with the rest
        // of the window empty beside it. Fetched as a real artifact page: the
        // assertion is about what `/ui/artifacts/<id>` declares, and pointing
        // it at any other route would pass without testing that.
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let id = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        let artifact = get_body(&app, &cookie, &format!("/ui/artifacts/{id}")).await;
        assert!(
            artifact.contains(r#"regions regions-split"#),
            "the artifact page is a split, not prose: {artifact}"
        );

        // No page says how wide it is any more.
        for (uri, body) in [
            ("/ui/search", &search),
            ("/ui/ops", &ops),
            ("/ui/capture", &capture),
            ("/ui/ask", &ask),
        ] {
            assert!(!body.contains("shell-wide"), "{uri} still declares a width");
        }
    }

    #[tokio::test]
    async fn the_ask_page_prefills_a_question_from_the_query_string() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/ask?q=mount+an+E01").await;
        assert!(page.contains(r#"value="mount an E01""#), "{page}");
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
        core.feedback.enabled = true;
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

        let (app, cookie) = app_with_session().await;
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

    /// The driver listens for every frame the server sends.
    ///
    /// A frame nobody handles fails silently and only on the asks that send it:
    /// the fan-out's frames fire only when a plan named something, so an ask
    /// page that drops them would look perfect on every question the base
    /// already covered. The names are pulled from `sse_event`'s own source
    /// rather than listed here, so adding an event without a handler fails this
    /// test instead of shipping.
    #[tokio::test]
    async fn the_stream_driver_handles_every_event_the_server_names() {
        let ui = include_str!("ui.rs");
        let body = &ui[ui.find("fn sse_event(").expect("sse_event is in this file")..];
        let body = &body[..body.find("\n}\n").unwrap()];
        // The first string of each arm's `(name, data)` tuple, whether the
        // arm is one line or many.
        let names: Vec<String> = body
            .split('(')
            .filter_map(|rest| rest.trim_start().strip_prefix('"'))
            .filter_map(|rest| rest.split('"').next())
            .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .map(str::to_string)
            .collect();
        assert!(
            names.len() >= 6,
            "the event names could not be read out of sse_event: {names:?}"
        );

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        for name in names {
            assert!(
                js.contains(&format!("addEventListener('{name}'")),
                "the server sends a `{name}` frame and the driver ignores it"
            );
        }
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

        let (app, cookie) = app_with_session().await;
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
        assert!(
            !page.contains("hx-post"),
            "the ask form still posts through htmx: {page}"
        );
    }

    /// The excerpt list, out of the `citations` frame, the way the page reads
    /// it. Keyed apart from the answer's `html` so `done_html` above cannot pick
    /// this frame up by mistake.
    fn rail_html(body: &str) -> String {
        let data = body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
            .find(|v| v.get("rail").is_some())
            .unwrap_or_else(|| panic!("no citations event in {body}"));
        data["rail"].as_str().unwrap().to_string()
    }

    /// Every run that follows `open`, up to the next `end`, in document order.
    fn pulled(html: &str, open: &str, end: char) -> Vec<String> {
        html.match_indices(open)
            .map(|(at, m)| {
                html[at + m.len()..]
                    .chars()
                    .take_while(|c| *c != end)
                    .collect()
            })
            .collect()
    }

    /// A citation link and the rail item it lands on are the two halves of one
    /// claim, and they are numbered by two separate passes over two separate
    /// templates: `link_citations` writes the hrefs into the answer, and
    /// `_ask_rail.html` writes the ids into the excerpts. Nothing but this
    /// assertion makes them agree, and a `[1]` that scrolls nowhere reads to a
    /// reader as provenance the base cannot actually show.
    ///
    /// The reply cites `[01]` as well as `[1]`, because the linker anchors on
    /// the parsed number rather than the digits it found: an id of `cite-01`
    /// would satisfy a lazier reading of this and still be a dead link.
    #[tokio::test]
    async fn every_citation_link_in_the_answer_points_at_an_excerpt_the_rail_carries() {
        let mut core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        // Swapped in after indexing, so the citations in the answer are this
        // reply's and not something retrieval happened to produce.
        core.completer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("alpha [1], bravo [2], and alpha again [01].".into()),
        }));
        let (app, cookie) = app_with_cookie(core).await;

        let body = ask_over_sse(&app, &cookie, "what+is+alpha").await;
        let rail = rail_html(&body);
        let answer = done_html(&body);

        let cited = pulled(&answer, r##"href="#cite-"##, '"');
        // Without this the test passes on an answer that cites nothing, which
        // is the state the page was in before this task and the state a broken
        // linker would put it back into.
        assert!(
            !cited.is_empty(),
            "the answer carries no citation links at all, so nothing was checked: {answer}"
        );
        for n in &cited {
            assert!(
                rail.contains(&format!(r#"id="cite-{n}""#)),
                "the answer links to #cite-{n} and the rail carries no such id: {rail}"
            );
        }

        // Coverage on its own is not enough, and a mutation proved it: numbering
        // the rail from zero leaves every id the answer links to still present
        // on the page — one excerpt further down. The links would all resolve
        // and every one of them would cite the wrong artifact, which is the
        // fabricated provenance this whole scheme exists to avoid. So the
        // numbering itself is pinned: 1..n, in the order the rail lists them.
        let ids = pulled(&rail, r#"id="cite-"#, '"');
        assert!(
            ids.len() > 1,
            "an off-by-one cannot show itself over fewer than two excerpts: {rail}"
        );
        let counted: Vec<String> = (1..=ids.len()).map(|i| i.to_string()).collect();
        assert_eq!(ids, counted, "the rail must number 1..n in order: {rail}");

        // And the n-th rail item has to be the n-th excerpt. The answer fragment
        // lists the same citations in the same order under "Artifacts used"
        // (after its own card, which is why the first title is dropped), so the
        // two renderings of one list are checked against each other rather than
        // each being trusted separately.
        let rail_titles = pulled(&rail, r#"<span class="rail-title">"#, '<');
        let mut card_titles = pulled(&answer, r#"<span class="card-title">"#, '<');
        card_titles.remove(0);
        assert_eq!(
            rail_titles, card_titles,
            "the rail and the answer disagree about which excerpt is which"
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
        core.feedback.enabled = true;
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
        AppState {
            core,
            auth: std::sync::Arc::new(crate::web::state::AuthContext {
                mode: crate::config::AuthMode::Local,
                local: None,
                oidc: None,
                pending: crate::auth::oidc::PendingStore::new(),
                secure_cookies: false,
            }),
            ask_handoff: Default::default(),
        }
    }

    /// A router and a session over a state the caller still holds, so a test
    /// can look at the same handoff map the routes write to. `app_with_cookie`
    /// builds its own state and cannot be asked what is in it.
    async fn app_over(st: &AppState) -> (axum::Router, String) {
        let cid = crate::store::new_id();
        st.core
            .store
            .insert_session(&cid, "user-1", None, 3600)
            .await
            .unwrap();
        (
            crate::web::router(st.clone()),
            format!("engram_session={cid}"),
        )
    }

    /// The model cites more excerpts than it was shown often enough that a link
    /// to a rail item which does not exist is a real outcome; it reads as a
    /// citation and scrolls nowhere.
    #[test]
    fn only_a_bracket_naming_an_excerpt_that_exists_becomes_a_link() {
        let out = super::link_citations("<p>see [1] and [2] but not [9] or [x]</p>", 2);
        assert!(
            out.contains(r##"<a class="cite" href="#cite-1">[1]</a>"##),
            "{out}"
        );
        assert!(
            out.contains(r##"<a class="cite" href="#cite-2">[2]</a>"##),
            "{out}"
        );
        assert!(out.contains("[9]"), "{out}");
        assert!(!out.contains("#cite-9"), "{out}");
        assert!(out.contains("[x]"), "{out}");
    }

    /// A citation link asserts that an excerpt supports the token it wraps.
    /// `argv[1]` is an array index on a base whose answers are full of code,
    /// and the citable range is exactly the range of common indices — so a link
    /// there is provenance the answer never claimed.
    #[test]
    fn an_array_index_inside_a_code_span_is_not_turned_into_a_citation() {
        let out = super::link_citations("<p>see [1]</p><pre><code>argv[1]</code></pre>", 2);
        assert!(
            out.contains(r##"<p>see <a class="cite" href="#cite-1">[1]</a></p>"##),
            "prose still links: {out}"
        );
        assert!(
            out.contains("<code>argv[1]</code>"),
            "code was linked: {out}"
        );
        assert_eq!(out.matches("cite-1").count(), 1, "{out}");
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

    /// `[01]` cites excerpt one; an anchor of `#cite-01` points at nothing the
    /// rail emits.
    #[test]
    fn a_zero_padded_citation_links_to_the_anchor_the_rail_will_carry() {
        let out = super::link_citations("<p>see [01]</p>", 2);
        assert!(out.contains(r##"href="#cite-1""##), "{out}");
        assert!(!out.contains("cite-01"), "{out}");
    }

    /// It runs over sanitized HTML, where a bracket inside a tag is an
    /// attribute rather than prose.
    #[test]
    fn citation_linking_leaves_the_inside_of_a_tag_alone() {
        let out = super::link_citations(r#"<a href="/x?q=[1]">[1]</a>"#, 1);
        assert_eq!(
            out, r##"<a href="/x?q=[1]"><a class="cite" href="#cite-1">[1]</a></a>"##,
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
    async fn with_an_ask_model_the_link_is_there() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/search").await;
        assert!(page.contains("href=\"/ui/ask\""), "{page}");
    }

    #[tokio::test]
    async fn a_promoted_window_is_listed_with_an_undo_that_works() {
        let (app, cookie, core) = app_session_and_core().await;
        let src = core
            .store
            .insert_corpus("l1\nl2", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &src.id,
                &[crate::store::segments::NewSegment {
                    start_line: 1,
                    end_line: 2,
                    text: "l1\nl2",
                    carry_lines: 0,
                }],
            )
            .await
            .unwrap();
        let na = |o: i64, t: &str| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: 1,
                end_line: 2,
            }),
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
        };
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[na(0, "passage")],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let a = core
            .store
            .insert_artifacts(&src.id, &[na(1, "artifact")])
            .await
            .unwrap();
        core.supersede(&p[0].id, &a[0].id).await.unwrap();
        core.store
            .set_segment_state(&src.id, 0, crate::store::segments::SegmentState::Done, None)
            .await
            .unwrap();
        core.store
            .set_corpus_status(&src.id, CorpusStatus::Ready)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", src.id)).await;
        let action = format!("/ui/corpora/{}/segments/0/unpromote", src.id);
        assert!(page.contains(&action), "{page}");

        let res = app
            .clone()
            .oneshot(form(&action, &cookie, ""))
            .await
            .unwrap();
        assert!(res.status().is_redirection(), "{:?}", res.status());
        assert!(
            core.store
                .get_artifact(&p[0].id)
                .await
                .unwrap()
                .in_results()
        );
        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", src.id)).await;
        assert!(!page.contains(&action), "undo still offered after undoing");
    }

    #[tokio::test]
    async fn a_merge_is_listed_under_each_corpus_it_drew_from() {
        let (app, cookie, core) = app_session_and_core().await;
        let c1 = core.store.insert_corpus("one", "web", None).await.unwrap();
        let c2 = core.store.insert_corpus("two", "web", None).await.unwrap();
        let na = |t: &str| crate::store::artifacts::NewArtifact {
            ordinal: 0,
            text: t.into(),
            corpus_span: Some(crate::store::artifacts::CorpusSpan {
                start_line: 1,
                end_line: 1,
            }),
            title: None,
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
        };
        let r1 = core
            .store
            .insert_artifacts(&c1.id, &[na("root one")])
            .await
            .unwrap()[0]
            .id
            .clone();
        let r2 = core
            .store
            .insert_artifacts(&c2.id, &[na("root two")])
            .await
            .unwrap()[0]
            .id
            .clone();
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "the merge of one and two".into(),
                    title: Some("Merged title".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[r1, r2],
            )
            .await
            .unwrap();
        for c in [&c1, &c2] {
            core.store
                .set_corpus_status(&c.id, CorpusStatus::Ready)
                .await
                .unwrap();
            let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", c.id)).await;
            assert!(page.contains("Written from this corpus"), "{page}");
            assert!(page.contains(&m.id), "{page}");
        }
    }

    #[tokio::test]
    async fn opening_from_another_artifacts_page_records_a_pivot() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        core.pursuit.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let made = handle
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "a".into(),
                        corpus_span: None,
                        title: Some("A".into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "b".into(),
                        corpus_span: None,
                        title: Some("B".into()),
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();
        get_body(&app, &cookie, &format!("/ui/artifacts/{}", made[0].id)).await;
        get_body(
            &app,
            &cookie,
            &format!("/ui/artifacts/{}?via={}", made[1].id, made[0].id),
        )
        .await;
        handle.background.wait_idle().await;
        let now = crate::store::now();
        let got = handle.store.interactions_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].kind, "opened");
        assert_eq!(got[1].kind, "pivoted");
        assert_eq!(got[1].via.as_deref(), Some(made[0].id.as_str()));
        assert_eq!(got[1].scope.as_deref(), Some("user-1"));
    }

    #[tokio::test]
    async fn a_generated_artifact_shows_its_cues_is_listed_on_ops_and_badged_in_the_rail() {
        let mut core = crate::core::test_support::test_core().await;
        core.pursuit.enabled = true;
        core.feedback.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let s = handle
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "source text".into(),
                    corpus_span: None,
                    title: Some("S".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        let g = handle
            .store
            .insert_synthesized_artifact(
                &crate::store::artifacts::NewSynthesized {
                    text: "generated from S".into(),
                    title: Some("Generated title".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec!["why was this asked".into()],
                },
                &[s[0].id.clone()],
            )
            .await
            .unwrap();
        crate::jobs::embed::run(&handle, &g.id).await.unwrap();
        handle
            .store
            .insert_pursuit(1, &["why was this asked".into()], &[s[0].id.clone()], None)
            .await
            .unwrap();

        let detail = get_body(&app, &cookie, &format!("/ui/artifacts/{}", g.id)).await;
        assert!(
            detail.contains("Written because these were asked"),
            "{detail}"
        );
        assert!(detail.contains("why was this asked"), "{detail}");

        let ops = get_body(&app, &cookie, "/ui/ops").await;
        assert!(ops.contains("Generated"), "{ops}");
        assert!(
            ops.contains(&format!("/ui/ops/artifacts/{}/deprecate", g.id)),
            "{ops}"
        );
        assert!(ops.contains("Pursuits"), "{ops}");

        let rail = get_body(
            &app,
            &cookie,
            "/ui/search/results?q=Generated%20title%0Agenerated%20from%20S",
        )
        .await;
        assert!(rail.contains("model-written"), "{rail}");
    }

    #[tokio::test]
    async fn the_pursuit_section_is_not_there_when_pursuits_are_off() {
        let (app, cookie) = app_with_session().await;
        let ops = get_body(&app, &cookie, "/ui/ops").await;
        assert!(!ops.contains("<h3>Pursuits</h3>"), "{ops}");
    }

    #[tokio::test]
    async fn the_page_reports_how_long_an_artifact_was_open() {
        let mut core = crate::core::test_support::test_core().await;
        core.feedback.enabled = true;
        core.pursuit.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let src = handle
            .store
            .insert_corpus("raw", "web", None)
            .await
            .unwrap();
        let a = handle
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
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
            .clone();
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/artifacts/{a}/dwell"),
                &cookie,
                "secs=42",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        handle.background.wait_idle().await;
        let now = crate::store::now();
        let got = handle.store.interactions_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "dwell");
        assert_eq!(got[0].detail.as_deref(), Some("42"));
        // The detail root names the artifact, which is what the page's timer reads.
        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{a}")).await;
        assert!(page.contains(&format!("data-artifact=\"{a}\"")), "{page}");
    }
}
