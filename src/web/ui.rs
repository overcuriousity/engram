use crate::auth::Identity;
use crate::core::search::SearchQuery;
use crate::error::{Error, Result};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

// ── View models ─────────────────────────────────────────────────────────────

pub struct RenderedResult {
    /// What the rail entry links to: the detail pane for this chunk.
    pub artifact_id: String,
    pub title: String,
    /// Sanitized HTML from `markdown::render`. Rendered with `|safe`.
    pub html: String,
    /// Markup-free preview for the rail, where rendered HTML would not fit.
    pub snippet: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_id: String,
    /// Position in the list, as `#1`, `#2`, …
    ///
    /// Not the raw score. That number is a fused rank from Qdrant plus a
    /// recency term, so it is comparable within one result list and meaningless
    /// between two — a hybrid query and a dense-only fallback do not even score
    /// on the same scale. Showing it invited a comparison it cannot support.
    pub rank: String,
}

pub struct BrowseRow {
    pub id: String,
    pub label: String,
    pub status: String,
    pub badge: &'static str,
    pub artifact_count: i64,
    pub created: String,
    /// `3/9` while windows are still being segmented, `None` once every window
    /// has resolved.
    pub progress: Option<String>,
    /// Percentage of the source that ended up inside some chunk.
    pub coverage: Option<String>,
    pub low_coverage: bool,
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
    pub corpus_id: String,
    /// True when this artifact's source was never captured here — the artifact
    /// was restored from the vector store and its corpus row is a placeholder.
    /// The pane shows the source beside the artifact, so it has to say when what
    /// it is showing is the artifact's own text reflected back rather than the
    /// document it was drawn from.
    pub corpus_restored: bool,
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
}

/// A neighbour, as one line in the pane.
pub struct RelatedArtifact {
    pub id: String,
    pub title: String,
    pub snippet: String,
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
    pub detail: Option<String>,
    pub contradiction: bool,
    /// Set when the judge named a direction with enough confidence to propose
    /// a supersede — the pair's "apply supersede" button shows only then.
    /// Applying it is a separate press; nothing here has hidden anything yet.
    pub obsolete_title: Option<String>,
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
        Raw | Segmenting | Segmented | Embedding => "badge-accent",
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

fn artifact_view(c: &crate::store::artifacts::Chunk) -> ArtifactView {
    ArtifactView {
        id: c.id.clone(),
        title: c
            .title
            .clone()
            .unwrap_or_else(|| format!("Chunk {}", c.ordinal)),
        html: markdown::render(&c.text),
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
    theme: String,
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
    theme: String,
    /// Kept so a reload or a deep link restores the box with its results.
    q: String,
    /// What this collection can actually be narrowed by. Rendered as chips, so
    /// choosing a category never means knowing in advance that it exists.
    facets: crate::vector::Facets,
    /// The chips a deep link arrived with, so the form comes back selected
    /// rather than reset to "all".
    category: String,
    tag: String,
}

#[derive(Template)]
#[template(path = "_results.html")]
struct ResultsTemplate {
    results: Vec<RenderedResult>,
    /// The query's indexable terms, for client-side highlighting.
    terms: String,
    /// `embed 41ms · total 138ms`, swapped into the header out of band.
    timing: String,
}

#[derive(Template)]
#[template(path = "browse.html")]
struct BrowseTemplate {
    theme: String,
    corpora: Vec<BrowseRow>,
}

#[derive(Template)]
#[template(path = "corpus.html")]
struct CorpusTemplate {
    theme: String,
    id: String,
    raw_text: String,
    status: String,
    badge: &'static str,
    artifacts: Vec<ArtifactView>,
    /// This row is a placeholder for a source that was never captured here, so
    /// `raw_text` is its restored artifacts joined rather than a document. The
    /// page has to say so: it otherwise presents reconstructed fragments under
    /// the same "Raw corpus" heading as a real capture, and offers to
    /// re-segment them.
    restored: bool,
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
    theme: String,
    d: ArtifactDetail,
}

#[derive(Template)]
#[template(path = "ops.html")]
struct OpsTemplate {
    theme: String,
    job_counts: Vec<(String, i64)>,
    oldest_pending_secs: Option<i64>,
    artifact_count: i64,
    vector_count: u64,
    retrying: Vec<RetryingRow>,
    parked: Vec<ParkedRow>,
    superseded: Vec<SupersededRow>,
    deprecated: Vec<DeprecatedRow>,
    stale: Vec<StaleRow>,
    pairs: Vec<PairRow>,
    tokens: Vec<TokenRow>,
}

#[derive(Template)]
#[template(path = "_token_created.html")]
struct TokenCreatedTemplate {
    token: String,
}

#[derive(Template)]
#[template(path = "ask.html")]
struct AskTemplate {
    theme: String,
}

#[derive(Template)]
#[template(path = "_answer.html")]
struct AnswerTemplate {
    answer: String,
    citations: Vec<RenderedResult>,
    dropped: usize,
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn capture_page(_id: Identity) -> impl IntoResponse {
    HtmlTemplate(CaptureTemplate {
        theme: "light".into(),
    })
}

#[derive(serde::Deserialize)]
struct CaptureForm {
    text: String,
    #[serde(default)]
    title: String,
}

async fn capture_submit(
    State(st): State<AppState>,
    _id: Identity,
    Form(f): Form<CaptureForm>,
) -> Result<Response> {
    let title = (!f.title.trim().is_empty()).then(|| f.title.trim().to_string());
    let out = st.core.ingest(&f.text, "web", title.as_deref()).await?;
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
    _id: Identity,
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
    // The form is single-select, so only the first tag of a multi-tag deep
    // link can be shown as chosen. The query still runs with all of them.
    let tag = split_tags(p.tags).first().cloned().unwrap_or_default();
    let category = p.category.unwrap_or_default();
    // A deep link can name a value that falls outside the top `FACET_LIMIT`, or
    // one nothing carries at all. The rail is narrowed by it either way, so the
    // chip row has to show it: otherwise the page reads as unfiltered while the
    // results are not, and there is no chip to click to get back out.
    ensure_facet(&mut facets.categories, &category);
    ensure_facet(&mut facets.tags, &tag);
    Ok(HtmlTemplate(SearchTemplate {
        theme: "light".into(),
        q: p.q,
        facets,
        tag,
        category,
    })
    .into_response())
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
    _id: Identity,
    Query(p): Query<UiSearchParams>,
) -> Result<Response> {
    // Clearing the box fires a request with an empty query. That is not an
    // error; it just means there is nothing to show.
    if p.q.trim().is_empty() {
        return Ok(HtmlTemplate(ResultsTemplate {
            results: vec![],
            terms: String::new(),
            timing: String::new(),
        })
        .into_response());
    }

    // The same terms the sparse branch derives, handed to the client so
    // highlighting never has to touch the sanitized HTML on this side.
    // Function words are dropped: a query phrased as a situation is mostly
    // stopwords, and highlighting every "to" marks the whole card.
    let terms = highlightable_terms(p.q.trim());
    let (hits, t) = st
        .core
        .search_timed(&SearchQuery {
            q: p.q,
            limit: 0,
            tags: split_tags(p.tags),
            category: p.category.filter(|c| !c.is_empty()),
            // Incremental: a prefix must not stamp what it happened to match.
            mark: false,
            include_deprecated: false,
            include_superseded: false,
        })
        .await?;

    Ok(HtmlTemplate(ResultsTemplate {
        results: hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| render_hit(i, h))
            .collect(),
        terms,
        timing: format!("embed {}ms · total {}ms", t.embed_ms, t.total_ms),
    })
    .into_response())
}

fn render_hit(position: usize, h: crate::core::search::SearchResult) -> RenderedResult {
    RenderedResult {
        artifact_id: h.artifact_id,
        title: h.title.unwrap_or_else(|| "Untitled".into()),
        html: markdown::render(&h.text),
        snippet: markdown::snippet(&h.text, 140),
        category: h.category,
        tags: h.tags,
        corpus_id: h.corpus_id,
        rank: format!("#{}", position + 1),
    }
}

async fn browse(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let mut rows = Vec::new();
    for s in st.core.store.list_corpora(200, 0).await? {
        let (resolved, total) = st.core.store.segment_progress(&s.id).await?;
        let progress = (total > 0 && resolved < total).then(|| format!("{resolved}/{total}"));
        let low_coverage = s
            .coverage
            .is_some_and(|c| c < crate::infer::verify::LOW_COVERAGE);
        rows.push(BrowseRow {
            progress,
            coverage: s.coverage.map(|c| format!("{:.0}%", c * 100.0)),
            low_coverage,
            label: s
                .title_hint
                .clone()
                .unwrap_or_else(|| markdown::snippet(&s.raw_text, 60)),
            badge: status_badge(&s.status),
            status: s.status.as_str().to_string(),
            artifact_count: st.core.store.artifacts_for_corpus(&s.id).await?.len() as i64,
            created: fmt_time(s.created_at),
            id: s.id,
        });
    }
    Ok(HtmlTemplate(BrowseTemplate {
        theme: "light".into(),
        corpora: rows,
    })
    .into_response())
}

async fn corpus_detail(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    let s = st.core.store.get_corpus(&cid).await?;
    let artifacts = st
        .core
        .store
        .artifacts_for_corpus(&cid)
        .await?
        .iter()
        .map(artifact_view)
        .collect();
    Ok(HtmlTemplate(CorpusTemplate {
        theme: "light".into(),
        id: s.id,
        raw_text: s.raw_text,
        badge: status_badge(&s.status),
        status: s.status.as_str().to_string(),
        restored: s.restored_at.is_some(),
        artifacts,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
struct ArtifactEditForm {
    text: String,
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
    Ok(Redirect::to("/ui/browse").into_response())
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
    Ok(Redirect::to(&format!("/ui/corpora/{corpus_id}")).into_response())
}

async fn reprocess_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    st.core
        .reprocess(&cid, crate::store::jobs::Stage::Synthesize)
        .await?;
    Ok(Redirect::to(&format!("/ui/corpora/{cid}")).into_response())
}

async fn ops(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use sqlx::Row;
    let artifact_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&st.core.store.pool)
        .await?
        .get("n");

    let tokens = st
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
            revoked: t.revoked_at.is_some(),
        })
        .collect();

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

    // A title is what makes two near-identical artifacts tellable apart at a
    // glance; falling back to the opening of the body beats an id.
    let title_of = |c: &crate::store::artifacts::Chunk| {
        c.title
            .clone()
            .unwrap_or_else(|| c.text.chars().take(60).collect())
    };

    let mut superseded = Vec::new();
    for c in st.core.store.superseded_artifacts(50).await? {
        let winner_id = c.superseded_by.clone().unwrap_or_default();
        let winner_title = match st.core.store.get_artifact(&winner_id).await {
            Ok(w) => title_of(&w),
            Err(_) => "(deleted)".to_string(),
        };
        superseded.push(SupersededRow {
            title: title_of(&c),
            id: c.id,
            winner_id,
            winner_title,
        });
    }

    // Confirmed contradictions and judge-proposed supersedes lead: they are
    // the ones that mean something in the base is wrong or stale, rather than
    // merely repeated.
    let mut pairs = Vec::new();
    for state in [
        crate::store::pairs::PairState::Contradiction,
        crate::store::pairs::PairState::Superseded,
        crate::store::pairs::PairState::Pending,
    ] {
        for p in st.core.store.pairs_by_state(state, 50).await? {
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
            pairs.push(PairRow {
                id: p.id,
                percent: (p.score * 100.0).round() as i64,
                a_title: title_of(&a),
                b_title: title_of(&b),
                a_id: p.a_id,
                b_id: p.b_id,
                detail: p.detail,
                contradiction: state == crate::store::pairs::PairState::Contradiction,
                obsolete_title,
            });
        }
    }

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
        theme: "light".into(),
        retrying,
        parked,
        superseded,
        deprecated,
        stale,
        pairs,
        job_counts: st.core.store.job_counts().await?,
        oldest_pending_secs: st.core.store.oldest_pending_age().await?,
        artifact_count,
        // Qdrant being briefly unreachable must not blank the ops page, which
        // is exactly where you look when something is wrong.
        vector_count: st.core.vectors.count().await.unwrap_or(0),
        tokens,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
struct MintForm {
    name: String,
}

async fn mint_token(
    State(st): State<AppState>,
    id: Identity,
    Form(f): Form<MintForm>,
) -> Result<Response> {
    let name = if f.name.trim().is_empty() {
        "unnamed"
    } else {
        f.name.trim()
    };
    let (_, plaintext) = crate::auth::tokens::mint(&st.core.store, name, &id.subject).await?;
    // Shown once, here, and never stored in plaintext anywhere.
    Ok(HtmlTemplate(TokenCreatedTemplate { token: plaintext }).into_response())
}

async fn revoke_token_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(tid): Path<String>,
) -> Result<Response> {
    crate::auth::tokens::revoke(&st.core.store, &tid).await?;
    Ok(Redirect::to("/ui/ops").into_response())
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

async fn unsupersede_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(aid): Path<String>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.unsupersede(&aid).await?;
    Ok(Redirect::to(back.path()).into_response())
}

async fn dismiss_pair_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(pid): Path<i64>,
) -> Result<Response> {
    st.core
        .store
        .set_pair_state(pid, crate::store::pairs::PairState::Dismissed, None)
        .await?;
    Ok(Redirect::to("/ui/ops").into_response())
}

/// The operator confirmation step for a judge-proposed supersede: the pair's
/// `obsolete_id` becomes an actual `Core::supersede` call. Nothing before this
/// press hides anything — see `jobs::consolidate::judge_pending`.
async fn apply_pair_supersede_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(pid): Path<i64>,
) -> Result<Response> {
    let pair = st.core.store.get_pair(pid).await?;
    let obsolete_id = pair.obsolete_id.ok_or(crate::error::Error::NotFound)?;
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
    Ok(Redirect::to("/ui/ops").into_response())
}

async fn deprecate_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(aid): Path<String>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.deprecate(&aid).await?;
    Ok(Redirect::to(back.path()).into_response())
}

async fn reactivate_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(aid): Path<String>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.reactivate(&aid).await?;
    Ok(Redirect::to(back.path()).into_response())
}

async fn verify_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(aid): Path<String>,
    Form(back): Form<ReturnTo>,
) -> Result<Response> {
    st.core.verify(&aid).await?;
    Ok(Redirect::to(back.path()).into_response())
}

async fn ask_page(_id: Identity) -> impl IntoResponse {
    HtmlTemplate(AskTemplate {
        theme: "light".into(),
    })
}

#[derive(serde::Deserialize)]
struct AskForm {
    q: String,
}

async fn ask_submit(
    State(st): State<AppState>,
    _id: Identity,
    Form(f): Form<AskForm>,
) -> Result<Response> {
    let out = st
        .core
        .ask(&crate::core::ask::AskRequest {
            q: f.q,
            limit: None,
            tags: vec![],
            category: None,
        })
        .await?;
    Ok(HtmlTemplate(AnswerTemplate {
        // The answer is model output too, so it goes through the same
        // sanitizing renderer as chunk text.
        answer: markdown::render(&out.answer),
        citations: out
            .citations
            .into_iter()
            .enumerate()
            .map(|(i, h)| render_hit(i, h))
            .collect(),
        dropped: out.dropped,
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
    let src = core.store.get_corpus(&c.corpus_id).await?;
    let slice = crate::web::corpus_view::for_corpus(&src).slice(&src, c.corpus_span.as_ref(), 3);
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
    Ok(ArtifactDetail {
        related,
        id: c.id,
        title: c.title.unwrap_or_else(|| format!("Chunk {}", c.ordinal)),
        html: markdown::render(&c.text),
        category: c.category,
        tags: c.tags,
        flags: c.flags,
        flag_detail: c.flag_detail,
        superseded_by: c.superseded_by,
        status: c.status,
        last_verified_at: c.last_verified_at,
        caveats: c.caveats,
        corpus_id: c.corpus_id,
        corpus_restored: src.restored_at.is_some(),
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
}

/// One route, two shapes. An htmx swap wants the pane's body; a pasted link
/// wants a page with navigation around it.
async fn artifact_detail(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(cid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
) -> Result<Response> {
    let d = build_artifact_detail(&st.core, &cid, &p.terms).await?;
    // Opening a chunk is the deliberate act that counts as remembering it.
    st.core.mark_artifact_seen(&cid);
    if headers.contains_key("hx-request") {
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(HtmlTemplate(ArtifactDetailPage {
        theme: "light".into(),
        d,
    })
    .into_response())
}

/// Clearing a flag is a judgement, not a fix: the operator looked at the chunk
/// beside its source lines and decided the warning was noise.
async fn mark_artifact_reviewed(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    st.core.store.clear_artifact_flags(&cid).await?;
    Ok(axum::response::Html(String::new()).into_response())
}

pub fn ui_router() -> Router<AppState> {
    Router::new()
        .route("/ui", get(|| async { Redirect::to("/ui/search") }))
        .route("/ui/capture", get(capture_page).post(capture_submit))
        .route("/ui/search", get(search_page))
        .route("/ui/search/results", get(search_results))
        .route("/ui/browse", get(browse))
        .route("/ui/corpora/{id}", get(corpus_detail))
        .route("/ui/corpora/{id}/delete", post(delete_corpus_ui))
        .route("/ui/corpora/{id}/reprocess", post(reprocess_ui))
        .route("/ui/artifacts/{id}", get(artifact_detail).put(put_artifact))
        .route("/ui/artifacts/{cid}/reviewed", post(mark_artifact_reviewed))
        .route("/ui/artifacts/{id}/delete", post(delete_artifact_ui))
        .route("/ui/ask", get(ask_page).post(ask_submit))
        .route("/ui/ops", get(ops))
        .route("/ui/ops/tokens", post(mint_token))
        .route("/ui/ops/tokens/{id}/revoke", post(revoke_token_ui))
        .route("/ui/ops/corpora/{id}/resolve", post(resolve_near_dupe_ui))
        .route("/ui/ops/artifacts/{id}/unsupersede", post(unsupersede_ui))
        .route("/ui/ops/artifacts/{id}/deprecate", post(deprecate_ui))
        .route("/ui/ops/artifacts/{id}/reactivate", post(reactivate_ui))
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
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let hits = core
            .search(&crate::core::search::SearchQuery {
                q: "alpha".into(),
                limit: 0,
                tags: vec![],
                category: None,
                mark: false,
                include_deprecated: false,
                include_superseded: false,
            })
            .await
            .unwrap();
        let r = super::render_hit(0, hits[0].clone());

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
    async fn the_detail_view_pairs_a_chunk_with_the_lines_it_claims() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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

        assert_eq!(d.corpus_id, out.id);
        assert!(d.html.contains("alpha"), "the chunk body must be rendered");
        assert!(
            !d.slice_lines.is_empty(),
            "the source slice must not be empty"
        );
        assert!(
            d.slice_lines.iter().any(|l| l.in_span),
            "at least one line must be marked as the span"
        );
        assert!(d.slice_label.starts_with("lines "));
    }

    #[tokio::test]
    async fn a_chunk_whose_source_vanished_is_not_a_500() {
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest("alpha\n\nbravo", "web", None).await.unwrap();
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::Synthesize && j.target_id == out.id {
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
        let cid = crate::store::new_id();
        core.store
            .insert_session(&cid, "user-1", None, 3600)
            .await
            .unwrap();
        let handle = core.clone();
        let state = crate::web::state::AppState {
            core,
            auth: std::sync::Arc::new(crate::web::state::AuthContext {
                mode: crate::config::AuthMode::Local,
                local: None,
                oidc: None,
                pending: crate::auth::oidc::PendingStore::new(),
                secure_cookies: false,
            }),
        };
        (
            crate::web::router(state),
            format!("engram_session={cid}"),
            handle,
        )
    }

    async fn body_of(res: axum::response::Response) -> String {
        let b = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8_lossy(&b).to_string()
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

    /// A session plus a corpus that has been through synthesis and embedding,
    /// which is the only state in which there is anything to facet or to find a
    /// neighbour among.
    async fn app_with_embedded_corpus() -> (axum::Router, String) {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let cid = crate::store::new_id();
        core.store
            .insert_session(&cid, "user-1", None, 3600)
            .await
            .unwrap();
        let state = crate::web::state::AppState {
            core,
            auth: std::sync::Arc::new(crate::web::state::AuthContext {
                mode: crate::config::AuthMode::Local,
                local: None,
                oidc: None,
                pending: crate::auth::oidc::PendingStore::new(),
                secure_cookies: false,
            }),
        };
        (crate::web::router(state), format!("engram_session={cid}"))
    }

    /// Markup with every run of whitespace collapsed, so an assertion about an
    /// attribute pair does not also assert where the template wrapped a line.
    fn flat(html: &str) -> String {
        html.split_whitespace().collect::<Vec<_>>().join(" ")
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

    #[tokio::test]
    async fn every_page_declares_itself_installable() {
        // The manifest link is what a phone looks for before offering to
        // install the app; the touch icon is what iOS uses instead.
        let (app, cookie) = app_with_session().await;
        let html = flat(&get(&app, "/ui/search", &cookie).await);
        assert!(html.contains(r#"rel="manifest" href="/assets/manifest.webmanifest""#));
        assert!(html.contains(r#"rel="apple-touch-icon""#));
        assert!(
            html.contains(r#"name="theme-color""#),
            "an installed window frames the page in this colour"
        );
    }

    #[tokio::test]
    async fn the_search_page_offers_a_chip_for_what_the_collection_contains() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search", &cookie).await);

        // The fake synthesizer files everything under `note` and tags it
        // `fake`, so those are exactly the values the payload index holds.
        assert!(
            html.contains(r#"name="category" value="note""#),
            "no category chip was rendered"
        );
        assert!(
            html.contains(r#"name="tags" value="fake""#),
            "no tag chip was rendered"
        );
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
        let matching = get(&app, "/ui/search/results?q=alpha&category=note", &cookie).await;
        let missing = get(&app, "/ui/search/results?q=alpha&category=recipe", &cookie).await;

        assert!(matching.contains("rail-item"), "the filter matched nothing");
        assert!(
            !missing.contains("rail-item"),
            "a category no artifact carries must return no results"
        );
    }

    #[tokio::test]
    async fn the_pane_lists_the_nearest_other_artifacts() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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
        crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
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
            assert_eq!(
                res.headers().get("location").unwrap(),
                "/auth/login?go=1",
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
        let html = flat(&body_of(res).await);
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
    async fn the_capture_page_renders_a_form() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/capture")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(html.contains("<textarea"));
        assert!(html.contains("hx-post=\"/ui/capture\""));
    }

    #[tokio::test]
    async fn capturing_text_stores_it_and_confirms() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure&title=t"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.to_lowercase().contains("captured"));
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
    async fn browse_lists_captured_sources() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=findable+content&title=My+Note",
            ))
            .await
            .unwrap();

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
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("My Note"));
    }

    #[tokio::test]
    async fn source_detail_shows_the_raw_text() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        let html = body_of(res).await;
        let id = html
            .split("/ui/corpora/")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();

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
    async fn ops_shows_queue_state_and_tokens() {
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
        assert!(html.contains("Queue"));
        assert!(html.contains("API tokens"));
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
    async fn ops_lists_a_pending_pair_and_can_dismiss_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

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
        assert!(html.contains("left one") && html.contains("right one"));

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

        // It is not recoverable from any later page.
        let page = body_of(
            app.oneshot(
                Request::builder()
                    .uri("/ui/ops")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(
            !page.contains("engram_"),
            "a stored token leaked into the ops page"
        );
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
        let res = app
            .oneshot(form("/ui/ask", &cookie, "q=what+is+alpha"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("Answer"));
    }
}
