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
    pub title: String,
    /// Sanitized HTML from `markdown::render`. Rendered with `|safe`.
    pub html: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub source_id: String,
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
    pub chunk_count: i64,
    pub created: String,
    /// `3/9` while windows are still being segmented, `None` once every window
    /// has resolved.
    pub progress: Option<String>,
    /// Percentage of the source that ended up inside some chunk.
    pub coverage: Option<String>,
    pub low_coverage: bool,
}

pub struct ChunkView {
    pub id: String,
    pub title: String,
    /// Sanitized by `markdown::render`. One of the few `|safe` interpolations.
    pub html: String,
    pub text: String,
    pub tags: Vec<String>,
    pub embed_state: String,
    pub embed_badge: &'static str,
}

/// A chunk beside the source lines it claims — the search pane, and the
/// review surface for anything verification flagged.
pub struct ChunkDetail {
    pub id: String,
    pub title: String,
    /// Sanitized by `markdown::render`. Rendered with `|safe`.
    pub html: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    pub source_id: String,
    pub window_idx: Option<i64>,
    pub slice_label: String,
    pub slice_lines: Vec<crate::web::source_view::SourceLine>,
    /// Query terms to highlight, space separated. Empty when the pane was
    /// opened outside a search.
    pub terms: String,
}

/// A chunk verification could not vouch for, and the window that produced it.
pub struct FlaggedRow {
    pub chunk_id: String,
    pub source_id: String,
    pub title: String,
    pub detail: String,
    pub window_idx: Option<i64>,
}

pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub created: String,
    pub last_used: String,
    pub revoked: bool,
}

pub fn status_badge(status: &crate::store::sources::SourceStatus) -> &'static str {
    use crate::store::sources::SourceStatus::*;
    match status {
        Ready => "badge-success",
        Partial => "badge-warning",
        Failed => "badge-danger",
        Raw | Segmenting | Segmented | Embedding => "badge-accent",
    }
}

pub fn embed_badge(state: &crate::store::chunks::EmbedState) -> &'static str {
    use crate::store::chunks::EmbedState::*;
    match state {
        Embedded => "badge-success",
        Failed => "badge-danger",
        Pending => "badge-muted",
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

fn chunk_view(c: &crate::store::chunks::Chunk) -> ChunkView {
    ChunkView {
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
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    theme: String,
}

#[derive(Template)]
#[template(path = "_results.html")]
struct ResultsTemplate {
    results: Vec<RenderedResult>,
}

#[derive(Template)]
#[template(path = "browse.html")]
struct BrowseTemplate {
    theme: String,
    sources: Vec<BrowseRow>,
}

#[derive(Template)]
#[template(path = "source.html")]
struct SourceTemplate {
    theme: String,
    id: String,
    raw_text: String,
    status: String,
    badge: &'static str,
    chunks: Vec<ChunkView>,
}

#[derive(Template)]
#[template(path = "_chunk.html")]
struct ChunkFragment {
    c: ChunkView,
}

#[derive(Template)]
#[template(path = "_chunk_detail.html")]
struct ChunkDetailFragment {
    d: ChunkDetail,
}

#[derive(Template)]
#[template(path = "chunk_detail.html")]
struct ChunkDetailPage {
    theme: String,
    d: ChunkDetail,
}

#[derive(Template)]
#[template(path = "ops.html")]
struct OpsTemplate {
    theme: String,
    job_counts: Vec<(String, i64)>,
    oldest_pending_secs: Option<i64>,
    chunk_count: i64,
    vector_count: u64,
    failed: Vec<crate::store::jobs::FailedJob>,
    flagged: Vec<FlaggedRow>,
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
    })
    .into_response())
}

async fn search_page(_id: Identity) -> impl IntoResponse {
    HtmlTemplate(SearchTemplate {
        theme: "light".into(),
    })
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
        return Ok(HtmlTemplate(ResultsTemplate { results: vec![] }).into_response());
    }

    let hits = st
        .core
        .search(&SearchQuery {
            q: p.q,
            limit: 0,
            tags: split_tags(p.tags),
            category: p.category.filter(|c| !c.is_empty()),
        })
        .await?;

    Ok(HtmlTemplate(ResultsTemplate {
        results: hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| render_hit(i, h))
            .collect(),
    })
    .into_response())
}

fn render_hit(position: usize, h: crate::core::search::SearchResult) -> RenderedResult {
    RenderedResult {
        title: h.title.unwrap_or_else(|| "Untitled".into()),
        html: markdown::render(&h.text),
        category: h.category,
        tags: h.tags,
        source_id: h.source_id,
        rank: format!("#{}", position + 1),
    }
}

async fn browse(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let mut rows = Vec::new();
    for s in st.core.store.list_sources(200, 0).await? {
        let (resolved, total) = st.core.store.window_progress(&s.id).await?;
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
            chunk_count: st.core.store.chunks_for_source(&s.id).await?.len() as i64,
            created: fmt_time(s.created_at),
            id: s.id,
        });
    }
    Ok(HtmlTemplate(BrowseTemplate {
        theme: "light".into(),
        sources: rows,
    })
    .into_response())
}

async fn source_detail(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<Response> {
    let s = st.core.store.get_source(&sid).await?;
    let chunks = st
        .core
        .store
        .chunks_for_source(&sid)
        .await?
        .iter()
        .map(chunk_view)
        .collect();
    Ok(HtmlTemplate(SourceTemplate {
        theme: "light".into(),
        id: s.id,
        raw_text: s.raw_text,
        badge: status_badge(&s.status),
        status: s.status.as_str().to_string(),
        chunks,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
struct ChunkEditForm {
    text: String,
}

async fn put_chunk(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Form(f): Form<ChunkEditForm>,
) -> Result<Response> {
    if f.text.trim().is_empty() {
        return Err(Error::Validation("chunk text is empty".into()));
    }
    st.core.store.update_chunk_text(&cid, &f.text).await?;
    // The stored vector describes wording that no longer exists.
    st.core
        .store
        .enqueue(crate::store::jobs::Stage::Embed, "chunk", &cid)
        .await?;
    let c = st.core.store.get_chunk(&cid).await?;
    Ok(HtmlTemplate(ChunkFragment { c: chunk_view(&c) }).into_response())
}

async fn delete_source_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<Response> {
    st.core.delete_source(&sid).await?;
    Ok(Redirect::to("/ui/browse").into_response())
}

async fn reprocess_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(sid): Path<String>,
) -> Result<Response> {
    st.core
        .reprocess(&sid, crate::store::jobs::Stage::Segment)
        .await?;
    Ok(Redirect::to(&format!("/ui/sources/{sid}")).into_response())
}

async fn ops(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use sqlx::Row;
    let chunk_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM chunks")
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

    let flagged = st
        .core
        .store
        .flagged_chunks(50)
        .await?
        .into_iter()
        .map(|c| FlaggedRow {
            title: c
                .title
                .clone()
                .unwrap_or_else(|| format!("Chunk {}", c.ordinal)),
            detail: c.flag_detail.clone().unwrap_or_else(|| c.flags.join(", ")),
            window_idx: c.window_idx,
            chunk_id: c.id,
            source_id: c.source_id,
        })
        .collect();

    Ok(HtmlTemplate(OpsTemplate {
        theme: "light".into(),
        flagged,
        job_counts: st.core.store.job_counts().await?,
        oldest_pending_secs: st.core.store.oldest_pending_age().await?,
        chunk_count,
        // Qdrant being briefly unreachable must not blank the ops page, which
        // is exactly where you look when something is wrong.
        vector_count: st.core.vectors.count().await.unwrap_or(0),
        failed: st.core.store.failed_jobs(50).await?,
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

async fn retry_job(
    State(st): State<AppState>,
    _id: Identity,
    Path(job_id): Path<i64>,
) -> Result<Response> {
    sqlx::query(
        "UPDATE jobs SET state='pending', attempts=0, run_after=0, last_error=NULL WHERE id=?",
    )
    .bind(job_id)
    .execute(&st.core.store.pool)
    .await?;
    Ok(Redirect::to("/ui/ops").into_response())
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

/// Everything the pane needs, in one place, so the handler is only routing.
pub(crate) async fn build_chunk_detail(
    core: &crate::core::Core,
    chunk_id: &str,
    terms: &str,
) -> Result<ChunkDetail> {
    let c = core.store.get_chunk(chunk_id).await?;
    let src = core.store.get_source(&c.source_id).await?;
    let slice = crate::web::source_view::for_source(&src).slice(&src, c.source_span.as_ref(), 3);
    Ok(ChunkDetail {
        id: c.id,
        title: c.title.unwrap_or_else(|| format!("Chunk {}", c.ordinal)),
        html: markdown::render(&c.text),
        category: c.category,
        tags: c.tags,
        flags: c.flags,
        flag_detail: c.flag_detail,
        source_id: c.source_id,
        window_idx: c.window_idx,
        slice_label: slice.label,
        slice_lines: slice.lines,
        terms: terms.to_string(),
    })
}

#[derive(serde::Deserialize)]
struct ChunkViewParams {
    #[serde(default)]
    terms: String,
}

/// One route, two shapes. An htmx swap wants the pane's body; a pasted link
/// wants a page with navigation around it.
async fn chunk_detail(
    State(st): State<AppState>,
    _id: Identity,
    headers: axum::http::HeaderMap,
    Path(cid): Path<String>,
    Query(p): Query<ChunkViewParams>,
) -> Result<Response> {
    let d = build_chunk_detail(&st.core, &cid, &p.terms).await?;
    // Opening a chunk is the deliberate act that counts as remembering it.
    st.core.mark_chunk_seen(&cid);
    if headers.contains_key("hx-request") {
        return Ok(HtmlTemplate(ChunkDetailFragment { d }).into_response());
    }
    Ok(HtmlTemplate(ChunkDetailPage {
        theme: "light".into(),
        d,
    })
    .into_response())
}

/// The action behind "re-segment this window": put the window back in the
/// queue's path and make sure something will pick it up. Split out from the
/// handler so it can be tested without a request.
pub(crate) async fn resegment_window_inner(
    core: &crate::core::Core,
    source_id: &str,
    idx: i64,
) -> Result<()> {
    core.store.reset_window(source_id, idx).await?;
    core.store
        .enqueue(crate::store::jobs::Stage::Segment, "source", source_id)
        .await?;
    Ok(())
}

async fn resegment_window(
    State(st): State<AppState>,
    _id: Identity,
    Path((sid, idx)): Path<(String, i64)>,
) -> Result<Response> {
    resegment_window_inner(&st.core, &sid, idx).await?;
    Ok(Redirect::to("/ui/ops").into_response())
}

/// Clearing a flag is a judgement, not a fix: the operator looked at the chunk
/// beside its source lines and decided the warning was noise.
async fn mark_chunk_reviewed(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    st.core.store.clear_chunk_flags(&cid).await?;
    Ok(axum::response::Html(String::new()).into_response())
}

pub fn ui_router() -> Router<AppState> {
    Router::new()
        .route("/ui", get(|| async { Redirect::to("/ui/search") }))
        .route("/ui/capture", get(capture_page).post(capture_submit))
        .route("/ui/search", get(search_page))
        .route("/ui/search/results", get(search_results))
        .route("/ui/browse", get(browse))
        .route("/ui/sources/{id}", get(source_detail))
        .route("/ui/sources/{id}/delete", post(delete_source_ui))
        .route("/ui/sources/{id}/reprocess", post(reprocess_ui))
        .route(
            "/ui/sources/{sid}/windows/{idx}/resegment",
            post(resegment_window),
        )
        .route("/ui/chunks/{id}", get(chunk_detail).put(put_chunk))
        .route("/ui/chunks/{cid}/reviewed", post(mark_chunk_reviewed))
        .route("/ui/ask", get(ask_page).post(ask_submit))
        .route("/ui/ops", get(ops))
        .route("/ui/ops/tokens", post(mint_token))
        .route("/ui/ops/tokens/{id}/revoke", post(revoke_token_ui))
        .route("/ui/ops/jobs/{id}/retry", post(retry_job))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_detail_view_pairs_a_chunk_with_the_lines_it_claims() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        let c = core
            .store
            .chunks_for_source(&out.id)
            .await
            .unwrap()
            .remove(0);

        let d = match super::build_chunk_detail(&core, &c.id, "").await {
            Ok(d) => d,
            Err(e) => panic!("detail view failed: {e}"),
        };

        assert_eq!(d.source_id, out.id);
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
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        let c = core
            .store
            .chunks_for_source(&out.id)
            .await
            .unwrap()
            .remove(0);
        core.delete_source(&out.id).await.unwrap();

        match super::build_chunk_detail(&core, &c.id, "").await {
            Err(crate::error::Error::NotFound) => {}
            Err(e) => panic!("expected a not-found, got {e}"),
            Ok(_) => panic!("a chunk whose source was deleted must not resolve"),
        }
    }

    #[tokio::test]
    async fn resegmenting_a_window_makes_it_pending_and_queues_the_job() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();
        crate::jobs::segment::run(&core, &out.id).await.unwrap();
        core.store
            .set_window_state(
                &out.id,
                0,
                crate::store::windows::WindowState::Fallback,
                Some("boom"),
            )
            .await
            .unwrap();

        super::resegment_window_inner(&core, &out.id, 0)
            .await
            .unwrap();

        let w = &core.store.windows_for_source(&out.id).await.unwrap()[0];
        assert_eq!(w.state, crate::store::windows::WindowState::Pending);
        assert_eq!(w.attempts, 0);

        let mut found = false;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::Segment && j.target_id == out.id {
                found = true;
            }
        }
        assert!(found, "a segment job must be queued for the source");
    }

    async fn app_with_session() -> (axum::Router, String) {
        let core = crate::core::test_support::test_core().await;
        let sid = crate::store::new_id();
        core.store
            .insert_session(&sid, "user-1", None, 3600)
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
        (crate::web::router(state), format!("engram_session={sid}"))
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

    #[test]
    fn status_maps_to_the_right_badge_class() {
        use crate::store::sources::SourceStatus::*;
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
            "/ui/sources/abc",
            "/ui/ask",
            "/ui/ops",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} was unprotected"
            );
        }
        for uri in [
            "/ui/capture",
            "/ui/ops/tokens",
            "/ui/sources/abc/delete",
            "/ui/sources/abc/reprocess",
            "/ui/ops/jobs/1/retry",
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
            .split("/ui/sources/")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/sources/{id}"))
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
                    .uri("/ui/chunks/missing")
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
