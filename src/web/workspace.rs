//! One text surface: the box never changes shape and the verb is a button (search, `Ask`, `Capture`).
//! The old capture, search and ask doors are deep links into this page.

use crate::tenants::Tenant;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::core::ingest::{ORIGIN_ASK, ORIGIN_WEB};
use crate::error::{Error, Result};
use crate::web::artifact::link_citations;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui::{
    FACET_LIMIT, RenderedResult, UiSearchParams, ensure_facet, render_hit, search_results,
};
use crate::web::ui_error::UiResult;

pub fn routes() -> Router<AppState> {
    Router::new()
        // `/` and `/ui` both land here: there is one page.
        .route("/", get(page))
        .route("/ui", get(page))
        // A deep link, and what a bookmark from before this change points at.
        .route("/ui/search", get(page))
        .route("/ui/search/results", get(search_results))
        // GET fills the box; POST is what the button and the extension send.
        .route("/ui/capture", get(capture_door).post(capture_submit))
        // The held-state regions, out of band. See `held_regions`.
        .route("/ui/held", get(held_regions))
        // The line under an idle box, polling itself while a capture is read.
        .route("/ui/idle-foot", get(idle_line))
        // The POST parks the question and returns an id; an `EventSource` spends it.
        .route("/ui/ask", get(ask_door).post(ask_submit))
        .route("/ui/ask/{id}/stream", get(ask_stream))
        .route("/ui/ask/{id}/verdict", post(ask_verdict))
        // Open to whoever searched: it is their own search.
        .route("/ui/search/{id}/verdict", post(search_verdict))
        .route("/ui/search/{id}/gap", post(search_gap))
        .route("/ui/ask/{id}/carried", post(ask_carried))
        .route("/ui/ask/{id}/keep", post(ask_keep))
        // The microphone: the answer goes into the box, nothing is captured.
        .route("/ui/transcribe", post(transcribe))
}

#[derive(Template)]
#[template(path = "workspace.html")]
struct WorkspaceTemplate {
    /// See `state::ask_enabled`; the button is absent rather than greyed out.
    ask_enabled: bool,
    /// A deep link's `?q=`, or an answer kept from an ask.
    q: String,
    /// Rendered as chips, so a category need not be known in advance.
    facets: crate::vector::Facets,
    /// So the row comes back selected rather than reset to "all".
    category: String,
    /// Whether the area under the box exists at all. See `Core::recommends`.
    recommend: bool,
    /// Off, the control offers text only.
    vision_enabled: bool,
    /// Off, the box has no microphone.
    stt_enabled: bool,
    /// `/ui?explain=1`, carried as a hidden field named in `hx-params`, which strips anything unnamed.
    explain: bool,
    /// Rendered as `data-rerank`, which arms app.js's refining pass.
    search_reranks: bool,
    /// The ask the box was filled from, by id: `capture_submit` turns it into stored provenance.
    prefill_ask: String,
    /// Shown so the operator deciding whether to keep the answer sees the question.
    prefill_question: String,
    /// `""`, `"ask"` or `"capture"`; gates the form's `load` trigger, since only a search-filled box should search.
    /// It also decides `idle`.
    open_with: &'static str,
    /// Empty only where the `load` trigger is about to fill the rail; the ask and capture doors run nothing and need it.
    idle: String,
    /// The condition `idle` is computed from, not "the box is empty".
    idle_state: bool,
    /// Onboarding is a property of an empty base, not of a new user: nothing is stored or dismissed.
    held: bool,
    /// One of the classifier's own prototypes; see `moments::examples_for`.
    example_remind: &'static str,
    /// For `lang` on the chip; see `moments::examples_lang_for`.
    example_lang: &'static str,
    /// Always false: the shared partials mark themselves `hx-swap-oob` only for `HeldTemplate`.
    oob: bool,
}

impl WorkspaceTemplate {
    /// The box is the Search destination.
    fn section(&self) -> &'static str {
        "search"
    }
}

/// The four regions that change once the base stops being empty, swapped out of band after the first stored capture.
/// The same partials the workspace includes, so the two cannot drift.
#[derive(Template)]
#[template(path = "_held.html")]
struct HeldTemplate {
    ask_enabled: bool,
    /// See `WorkspaceTemplate`; `_box_hint.html` reads these.
    example_remind: &'static str,
    example_lang: &'static str,
    /// True by definition; the partials branch on it.
    held: bool,
    oob: bool,
}

/// Only where something is held, or an Ask verb would appear over a base that cannot answer.
async fn held_regions(tenant: Tenant, headers: axum::http::HeaderMap) -> UiResult<Response> {
    let (corpora, _) = tenant.core.store.held_brief().await?;
    if corpora == 0 {
        return Err(crate::error::Error::NotFound.into());
    }
    let (example_remind, example_lang) = examples(&tenant.core, &headers);
    Ok(HtmlTemplate(HeldTemplate {
        ask_enabled: crate::web::state::ask_enabled(&tenant),
        example_remind,
        example_lang,
        held: true,
        oob: true,
    })
    .into_response())
}

/// Not the whole idle swap (`oob`), which would empty the echo while someone types.
/// `HX-Trigger` lets app.js re-run a search stuck at "still reading".
async fn idle_line(tenant: Tenant, headers: axum::http::HeaderMap) -> UiResult<Response> {
    let t = crate::web::ui::idle_foot(&tenant, false, &headers).await?;
    let reading = t.reading;
    let mut res = HtmlTemplate(t).into_response();
    if !reading {
        res.headers_mut().insert(
            "HX-Trigger",
            axum::http::HeaderValue::from_static("engram:read-landed"),
        );
    }
    Ok(res)
}

/// English where the header is absent or unreadable; an empty phrasing without `[infer.synthesize]`.
pub(crate) fn examples(
    core: &crate::core::Core,
    headers: &axum::http::HeaderMap,
) -> (&'static str, &'static str) {
    if !core.synthesizes() {
        return ("", "");
    }
    let raw = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    (
        crate::core::moments::examples_for(raw),
        crate::core::moments::examples_lang_for(raw),
    )
}

async fn base_template(
    tenant: &Tenant,
    headers: &axum::http::HeaderMap,
    q: String,
    category: String,
    open_with: &'static str,
) -> Result<WorkspaceTemplate> {
    // A vector store that cannot answer must not take the page down.
    let mut facets = tenant
        .core
        .vectors
        .facets(FACET_LIMIT)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "facets unavailable; rendering the workspace without chips");
            Default::default()
        });
    // A deep link may name a category outside the top `FACET_LIMIT`; the chip must still show.
    ensure_facet(&mut facets.categories, &category);
    // The complement of the template's `load` trigger condition.
    let idle_state = q.is_empty() || !open_with.is_empty();
    // Always rendered: a cleared deep-link box gets an out-of-band swap onto it.
    let idle = crate::web::ui::idle_foot(tenant, false, headers)
        .await?
        .render()
        .map_err(|e| crate::error::Error::Internal(e.to_string()))?;
    // Unconditional: a search URL against an empty base must not offer Ask either.
    let (corpora, _) = tenant.core.store.held_brief().await?;
    let (example_remind, example_lang) = examples(&tenant.core, headers);
    Ok(WorkspaceTemplate {
        ask_enabled: crate::web::state::ask_enabled(tenant),
        q,
        facets,
        category,
        recommend: tenant.core.recommends(),
        vision_enabled: tenant.core.describer.is_some(),
        stt_enabled: tenant.core.transcriber.is_some(),
        search_reranks: tenant.core.reranks_search(),
        // Only the deep-link door sets it.
        explain: false,
        prefill_ask: String::new(),
        prefill_question: String::new(),
        open_with,
        idle,
        idle_state,
        example_remind,
        example_lang,
        held: corpora > 0,
        oob: false,
    })
}

async fn page(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Query(p): Query<UiSearchParams>,
) -> UiResult<Response> {
    let explain = p.explain.unwrap_or(false);
    let mut t = base_template(&tenant, &headers, p.q, p.category.unwrap_or_default(), "").await?;
    t.explain = explain;
    Ok(HtmlTemplate(t).into_response())
}

/// `from_ask` rather than the answer: an answer can exceed a URL.
#[derive(serde::Deserialize)]
struct CapturePrefill {
    #[serde(default)]
    from_ask: Option<String>,
}

/// Text only: a name arrives from synthesis, which has read the document.
#[derive(serde::Deserialize)]
struct CaptureForm {
    text: String,
    /// Records the text as model-written and its sources, however much was edited.
    #[serde(default)]
    from_ask: Option<String>,
    /// From the hidden `#box-tz` app.js fills, so reminders land in the viewer's zone.
    #[serde(default)]
    tz: Option<String>,
    /// The search the box was typing (`#fold-of`); carried so the hole it would become closes against this text.
    #[serde(default)]
    from_search: Option<String>,
}

#[derive(Template)]
#[template(path = "_captured.html")]
struct CapturedTemplate {
    id: String,
    duplicate: bool,
    /// Without it the page says "processing" for a capture nothing will process.
    near_dupe_of: Option<String>,
    near_dupe_percent: i64,
}

async fn capture_submit(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Form(f): Form<CaptureForm>,
) -> UiResult<Response> {
    // The account setting, or `Accept-Language` on automatic.
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
    // A kept answer is an ordinary paste with an `ask` origin and metadata.
    // The two travel together: if the ask row is gone, it falls back to a plain paste.
    let capture = match f.from_ask.as_deref().filter(|s| !s.is_empty()) {
        Some(ask_id) => match tenant.core.store.ask_event(ask_id).await? {
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
    // Only the caller's own search (`Store::event_is_mine`); dropped rather than refused.
    let capture = match f.from_search.as_deref().filter(|s| !s.is_empty()) {
        Some(ev)
            if tenant
                .core
                .store
                .event_is_mine(ev, &tenant.user.subject)
                .await? =>
        {
            capture.with_search(ev)
        }
        Some(ev) => {
            tracing::warn!(
                event_id = ev,
                "a capture named a search that is not the capturer's; keeping it as an ordinary paste"
            );
            capture
        }
        None => capture,
    };
    let out = tenant
        .core
        .ingest_capture(
            capture
                .with_lang(lang)
                .with_tz(viewer_zone(f.tz.as_deref())),
        )
        .await?;
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

/// An unknown name is dropped, not refused: nobody typed this field, and a 400 would lose the capture.
fn viewer_zone(tz: Option<&str>) -> Option<String> {
    let name = tz.map(str::trim).filter(|t| !t.is_empty())?;
    match name.parse::<chrono_tz::Tz>() {
        Ok(z) => Some(z.name().to_string()),
        Err(_) => {
            tracing::warn!(tz = %name, "the box sent a zone the table does not know; reading its dates in the default");
            None
        }
    }
}

/// An unrecorded ask leaves the box empty rather than erroring.
async fn capture_door(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Query(p): Query<CapturePrefill>,
) -> UiResult<Response> {
    let prefilled = match &p.from_ask {
        Some(id) => tenant.core.store.ask_event(id).await?,
        None => None,
    };
    let (q, prefill_ask, prefill_question) = match prefilled {
        Some(ev) => (ev.answer, ev.id, ev.question),
        None => (String::new(), String::new(), String::new()),
    };
    let mut t = base_template(&tenant, &headers, q, String::new(), "capture").await?;
    t.prefill_ask = prefill_ask;
    t.prefill_question = prefill_question;
    Ok(HtmlTemplate(t).into_response())
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
    /// A cut-off answer is otherwise indistinguishable from a finished one.
    truncated: bool,
    /// Badged so the operator sees what the harness will count.
    abstained: bool,
    /// Badged and marked in `answer`, so held text and model text can be told apart.
    unsupported: Vec<String>,
    /// Every cited excerpt has been retired.
    retired_only: bool,
    /// Both sides cited; the base picks neither.
    disagreements: Vec<crate::store::pairs::Disagreement>,
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
    /// Set when the bar rides along with another response, not when it is the click's own target.
    oob: bool,
}

#[derive(Template)]
#[template(path = "_ask_kept.html")]
struct AskKeptTemplate {
    id: String,
    duplicate: bool,
    /// Resembles something already held; still read and searchable.
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

#[derive(serde::Deserialize)]
struct AskForm {
    q: String,
}

/// Nothing is recorded or stored: dictation is typing.
/// A 404 where no speech model is configured; `text/plain` because it fills a textarea.
async fn transcribe(tenant: Tenant, multipart: axum::extract::Multipart) -> UiResult<Response> {
    let text = hear(&tenant, multipart).await?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        text,
    )
        .into_response())
}

/// Shared by `/ui/transcribe` and `/api/v1/transcribe`.
/// An empty recording answers the empty transcript without a model call.
pub(crate) async fn hear(
    tenant: &Tenant,
    mut multipart: axum::extract::Multipart,
) -> Result<String> {
    let Some(model) = tenant.core.transcriber.clone() else {
        return Err(Error::NotFound);
    };

    let mut audio: Option<(Vec<u8>, String)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("multipart: {e}")))?
    {
        if field.name() != Some("audio") {
            continue;
        }
        // Read before the bytes, which consume the field.
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| Error::Validation(format!("audio: {e}")))?;
        audio = Some((bytes.to_vec(), mime));
        break;
    }
    let Some((bytes, mime)) = audio else {
        return Err(Error::Validation("no audio part".into()));
    };
    if bytes.is_empty() {
        return Ok(String::new());
    }
    model.transcribe(&bytes, &mime).await
}

async fn ask_submit(
    State(st): State<AppState>,
    tenant: Tenant,
    Form(f): Form<AskForm>,
) -> UiResult<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound.into());
    }
    // Refused before anything is parked.
    if f.q.trim().is_empty() {
        return Err(Error::Validation("question is empty".into()).into());
    }
    let handoff = st.ask_handoff_park(
        crate::core::ask::AskRequest {
            q: f.q,
            limit: None,
            tags: vec![],
            category: None,
        },
        &tenant.user.subject,
    );
    Ok(axum::Json(serde_json::json!({ "id": handoff })).into_response())
}

/// Takes an `Identity`: an unauthenticated inference endpoint would be a free-inference hole.
/// Leaving before `Done` records nothing: the id reaches the page only there.
async fn ask_stream(
    State(st): State<AppState>,
    tenant: Tenant,
    Path(handoff): Path<String>,
) -> UiResult<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound.into());
    }
    use tokio_stream::StreamExt as _;

    // Unknown, spent, expired or another subject's are one answer, never a fresh ask.
    let req = st
        .ask_handoff_take(&handoff, &tenant.user.subject)
        .ok_or(Error::NotFound)?;
    let core = tenant.core.clone();
    let origin = crate::store::feedback::Door::Ui.by(tenant.user.subject);
    let events = async_stream::stream! {
        let s = core.ask_events(&req, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            yield match ev {
                Ok(e) => sse_event(e),
                // Terminal: the `try_stream!` producer ends at its first error.
                Err(e) => Ok(SseEvent::default().event("error").data(e.to_string())),
            };
        }
    };
    // Kept alive: a slow model outlasts a proxy's idle timeout.
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// JSON for every payload: SSE frames data by line, so newlines would not survive.
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
        // A list: the page joins it where the sentence is written.
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

/// Rendered server-side so the rail's ids and `link_citations`' links are numbered by one pass.
/// Excerpts are already sanitized; the page never renders markdown.
fn rail_fragment(hits: Vec<crate::core::search::SearchResult>) -> Result<String> {
    AskRailTemplate {
        citations: hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| render_hit(i, h, &Default::default(), false))
            .collect(),
    }
    .render()
    .map_err(|e| Error::Internal(e.to_string()))
}

/// The same template the blocking render used: one account of an answer.
fn answer_fragment(out: crate::core::ask::AskResponse) -> Result<String> {
    // Sanitized first, then marked (on escaped text), then linked, so neither pass knows the other's markup.
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
            .map(|(i, h)| render_hit(i, h, &Default::default(), false))
            .collect(),
        dropped: out.dropped,
        truncated: out.truncated,
        retired_only: out.retired_only,
        abstained: out.abstained,
        unsupported: out.unsupported,
        disagreements: out.disagreements,
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

pub(crate) fn verdict_label(v: crate::store::asks::AskVerdict) -> String {
    use crate::store::asks::AskVerdict::*;
    match v {
        Right => "right",
        Wrong => "wrong",
        NothingHere => "nothing here",
    }
    .into()
}

async fn ask_verdict_bar(tenant: &Tenant, id: &str, oob: bool) -> Result<String> {
    let ev = tenant
        .core
        .store
        .ask_event(id)
        .await?
        .ok_or(Error::NotFound)?;
    AskVerdictTemplate {
        event_id: ev.id,
        verdict: ev.verdict.map(verdict_label),
        oob,
    }
    .render()
    .map_err(|e| Error::Internal(e.to_string()))
}

async fn ask_verdict(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<VerdictForm>,
) -> UiResult<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound.into());
    }
    match f.verdict.as_str() {
        "none" => tenant.core.store.unjudge_ask(&id).await?,
        v => {
            let verdict = crate::store::asks::AskVerdict::parse(v)
                .ok_or_else(|| Error::Validation(format!("unknown verdict {v}")))?;
            tenant.core.judge_ask(&id, verdict).await?;
        }
    }
    Ok(axum::response::Html(ask_verdict_bar(&tenant, &id, false).await?).into_response())
}

#[derive(Template)]
#[template(path = "_search_verdict.html")]
struct SearchVerdictTemplate {
    event_id: String,
    artifact_id: String,
    /// `hit` / `no` / `skip` for display; empty shows the buttons.
    state: &'static str,
}

#[derive(serde::Deserialize)]
struct SearchVerdictForm {
    verdict: String,
    artifact_id: String,
}

/// Every refused write reaches the person as the same sentence: the search is no longer waiting.
fn already_judged() -> Response {
    axum::response::Html(
        r#"<span class="muted">nothing to record — that search was already judged.</span>"#,
    )
    .into_response()
}

/// `none` is the undo; `no` leaves the search a question.
/// `None` means already judged, by another tab or door.
pub(crate) async fn judge_search(
    tenant: &Tenant,
    id: &str,
    verdict: &str,
    artifact_id: &str,
) -> Result<Option<&'static str>> {
    if !tenant.core.learn.enabled {
        return Err(Error::NotFound);
    }
    use crate::store::feedback::Labeller;
    let store = &tenant.core.store;
    // The id comes off the page; one ownership check for all four answers (`Store::event_is_mine`).
    if !store.event_is_mine(id, &tenant.user.subject).await? {
        return Err(Error::NotFound);
    }
    let state = match verdict {
        "hit" => {
            // A hit on an artifact search will not return would inflate recall; the page already withholds the bar.
            if !store.get_artifact(artifact_id).await?.in_results() {
                return Err(Error::Validation(
                    "that one is deprecated or superseded, so the benchmark can't hold it".into(),
                ));
            }
            // `NotFound` is the store refusing to overwrite another tab's verdict.
            match store.judge_hit(id, artifact_id, Labeller::Confirm).await {
                Ok(()) => {
                    // Confirming the result says the note is still right.
                    tenant
                        .core
                        .confirm_cited(std::slice::from_ref(&artifact_id.to_string()))
                        .await;
                    "hit"
                }
                Err(Error::NotFound) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        "no" => {
            // Refused rather than applied if another tab judged meanwhile.
            if !store.decline(id).await? {
                return Ok(None);
            }
            "no"
        }
        // Not `Verdict::Discard`, which drops the search from eval pairs and the purge exemption.
        "skip" => match store.skip_event(id).await {
            Ok(()) => "skip",
            Err(Error::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        },
        "none" => {
            // Only back over what this bar wrote (`Labeller::Confirm`), so a hit from another tab survives.
            match store.unjudge(id, Labeller::Confirm).await {
                Ok(()) => "",
                Err(Error::NotFound) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        v => return Err(Error::Validation(format!("unknown verdict {v}"))),
    };
    Ok(Some(state))
}

async fn search_verdict(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<SearchVerdictForm>,
) -> UiResult<Response> {
    let Some(state) = judge_search(&tenant, &id, &f.verdict, &f.artifact_id).await? else {
        return Ok(already_judged());
    };
    Ok(HtmlTemplate(SearchVerdictTemplate {
        event_id: id,
        artifact_id: f.artifact_id,
        state,
    })
    .into_response())
}

/// The query travels with the id: a trailing keystroke can fold a later wording into the row. See `Store::gap_event`.
#[derive(serde::Deserialize)]
struct GapParams {
    q: String,
}

/// False where the search was already judged.
pub(crate) async fn gap_search(tenant: &Tenant, id: &str, q: &str) -> Result<bool> {
    if !tenant.core.learn.enabled {
        return Err(Error::NotFound);
    }
    // Only against the caller's own search; see `Store::event_is_mine`.
    if !tenant
        .core
        .store
        .event_is_mine(id, &tenant.user.subject)
        .await?
    {
        return Err(Error::NotFound);
    }
    let recorded = tenant.core.store.gap_event(id, q.trim()).await?;
    Ok(recorded)
}

async fn search_gap(
    tenant: Tenant,
    Path(id): Path<String>,
    axum::extract::Query(p): axum::extract::Query<GapParams>,
) -> UiResult<Response> {
    let line = match gap_search(&tenant, &id, &p.q).await? {
        true => "Recorded as a gap.",
        false => "Already judged.",
    };
    Ok(axum::response::Html(format!(r#"<span class="muted">{line}</span>"#)).into_response())
}

async fn ask_carried(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<CarriedForm>,
) -> UiResult<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound.into());
    }
    let carried = tenant.core.store.toggle_carried(&id, f.n).await?;
    let bar = ask_verdict_bar(&tenant, &id, true).await?;
    Ok(HtmlTemplate(AskCarriedTemplate {
        event_id: id,
        n: f.n,
        carried,
        bar: Some(bar),
    })
    .into_response())
}

/// The same pipeline as any paste, with `origin = "ask"` and the `ask` metadata.
/// The answer as written; `edit first` is the path for editing.
async fn ask_keep(tenant: Tenant, Path(id): Path<String>) -> UiResult<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound.into());
    }
    // No text to fall back to: a question retention took has nothing left to keep.
    let ev = tenant
        .core
        .store
        .ask_event(&id)
        .await?
        .ok_or(Error::NotFound)?;
    let out = tenant
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

#[derive(serde::Deserialize)]
struct VerdictForm {
    verdict: String,
}

#[derive(serde::Deserialize)]
struct CarriedForm {
    n: i64,
}

/// Nothing is asked on arrival: a GET that spends a model call is replayable by any prefetch.
async fn ask_door(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Query(p): Query<AskPrefill>,
) -> UiResult<Response> {
    if !tenant.core.asks() {
        return Err(crate::error::Error::NotFound.into());
    }
    // An empty base renders no Ask button, so the question is redirected to the plain page.
    let (corpora, _) = tenant.core.store.held_brief().await?;
    if corpora == 0 {
        let q: String = url::form_urlencoded::byte_serialize(p.q.as_bytes()).collect();
        return Ok(axum::response::Redirect::to(&format!("/ui?q={q}")).into_response());
    }
    let t = base_template(&tenant, &headers, p.q, String::new(), "ask").await?;
    Ok(HtmlTemplate(t).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test_support::{app_with_cookie, body_of, get_body};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn workspace(uri: &str) -> String {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        body_of(res).await
    }

    async fn workspace_held(uri: &str) -> String {
        let core = crate::core::test_support::test_core().await;
        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        body_of(res).await
    }

    #[tokio::test]
    async fn an_empty_base_offers_only_the_verb_that_can_work() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let empty = body_of(res).await;

        assert!(
            !empty.contains(r#"data-verb="ask""#),
            "Ask can only abstain on an empty base, so the door is not there"
        );
        assert!(
            !empty.contains(r#"class="keyhint""#),
            "seven shortcuts for moving through a list with nothing in it"
        );
        assert!(
            empty.contains("Paste a note"),
            "the placeholder names the one verb that can work"
        );

        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let held = body_of(res).await;

        assert!(
            held.contains(r#"data-verb="ask""#),
            "one source is enough to have something to ask about"
        );
        assert!(
            held.contains(r#"class="keyhint""#),
            "and something to move through"
        );
        assert!(
            held.contains("Describe the situation"),
            "the placeholder goes back to naming all three verbs"
        );
    }

    #[tokio::test]
    async fn an_empty_base_says_what_this_is_and_whose_it_is() {
        let html = workspace("/ui").await;
        assert!(
            html.contains("only you can search it"),
            "what the application does, in one clause"
        );
        assert!(
            html.contains("Paste a note"),
            "and the boundary, which is what a person wants before pasting \
             their own notes onto someone else's server"
        );
        assert!(
            !html.contains("An artifact opens here"),
            "the held pane's line, on a base that holds nothing"
        );
        assert!(
            html.contains("beside the original text"),
            "the pane says what will happen to the first thing pasted"
        );
    }

    #[tokio::test]
    async fn an_empty_base_shows_what_a_paste_becomes() {
        let html = workspace("/ui").await;
        // The `<dl>` is in every response, since out-of-band swaps replace only what they carry.
        assert!(html.contains("<dt>note</dt>"), "{html}");
        assert_eq!(
            html.matches(r#"class="chip-example""#).count(),
            1,
            "the phrasing the reader recognises, pressable: {html}"
        );
    }

    #[tokio::test]
    async fn the_list_of_what_a_paste_becomes_goes_at_five_sources() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        // `ingest_capture` dedupes on `content_hash`, so the texts must differ.
        let text = |n: i64| format!("source number {n}");
        for n in 0..4 {
            core.ingest_capture(crate::core::ingest::Capture::new(text(n), "ui"))
                .await
                .unwrap();
        }
        let four = get_body(&app, &cookie, "/ui").await;
        assert!(four.contains("<dt>note</dt>"), "four sources: {four}");
        assert_eq!(
            four.matches(r#"class="chip-example""#).count(),
            1,
            "the chips stand once, under the box: {four}"
        );

        core.ingest_capture(crate::core::ingest::Capture::new(text(4), "ui"))
            .await
            .unwrap();
        let five = get_body(&app, &cookie, "/ui").await;
        assert!(
            five.contains(r#"<dl id="teach" class="teach" hx-swap-oob="true"></dl>"#),
            "the list comes back empty rather than absent, or the swap has \
             nothing to replace the standing one with: {five}"
        );
        assert_eq!(
            five.matches(r#"class="chip-example""#).count(),
            1,
            "and the chips are where they always were: {five}"
        );
    }

    #[tokio::test]
    async fn the_idle_line_says_reading_while_a_capture_is_read() {
        let core = crate::core::test_support::test_core().await;
        crate::web::test_support::hold_something(&core).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let reading = get_body(&app, &cookie, "/ui/search/results?q=").await;
        assert!(reading.contains("reading…"), "{reading}");
        assert!(
            reading.contains(r#"hx-get="/ui/idle-foot""#),
            "and polls until it is done: {reading}"
        );

        drain(&core).await;
        let done = get_body(&app, &cookie, "/ui/search/results?q=").await;
        assert!(!done.contains("reading…"), "{done}");
        assert!(
            !done.contains(r#"hx-get="/ui/idle-foot""#),
            "nothing left to poll for: {done}"
        );
    }

    async fn drain(core: &crate::core::Core) {
        while let Some(j) = core.store.claim_job().await.unwrap() {
            core.store.complete_job(j.id).await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_polled_line_says_reading_and_clears_nothing() {
        let core = crate::core::test_support::test_core().await;
        crate::web::test_support::hold_something(&core).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let line = get_body(&app, &cookie, "/ui/idle-foot").await;
        assert!(line.contains("reading…"), "{line}");
        assert!(
            line.contains("sse:idle"),
            "and asks again until it is not: {line}"
        );
        assert!(
            !line.contains("rail-head"),
            "the heading over the rail is not this fragment's to empty: {line}"
        );
        assert!(
            !line.contains("fold-of"),
            "nor is the search the box is folding into: {line}"
        );

        drain(&core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/idle-foot")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.headers().get("HX-Trigger").map(|v| v.to_str().unwrap()),
            Some("engram:read-landed"),
            "the rail is never told the read landed"
        );
        let done = body_of(res).await;
        assert!(!done.contains("every 3s"), "the poll stops: {done}");
    }

    #[tokio::test]
    async fn the_idle_line_refreshes_alone() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/idle-foot")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let line = body_of(res).await;
        assert!(line.contains(r#"id="idle-foot""#), "{line}");
        assert!(!line.contains("rail-head"), "{line}");
        assert!(!line.contains("intent-echo"), "{line}");
        assert!(
            !line.contains("fold-of"),
            "nor forget the search the box is folding into: {line}"
        );
    }

    #[tokio::test]
    async fn an_ordinary_capture_writes_nothing_over_the_box() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/capture")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("text=LevelDB+tombstones+survive+compaction."))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.trim().is_empty(),
            "an ordinary capture still writes into the bar the box lives in: {html}"
        );
    }

    #[tokio::test]
    async fn a_capture_names_the_search_it_was_typed_from() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let event = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "Termin Foto Dienstausweis Mittwoch 0900".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some(crate::store::TEST_SUBJECT.into()),
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0, 0.0, 0.0],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/capture")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "text=Termin+Foto+Dienstausweis+Mittwoch+0900&from_search={event}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let stored = &core.store.list_corpora(10, 0).await.unwrap()[0];
        assert_eq!(
            crate::core::ingest::typed_from(&stored.metadata),
            Some(event.as_str()),
            "the capture does not say which query it was typed from"
        );
    }

    #[tokio::test]
    async fn a_capture_cannot_name_somebody_elses_search() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let theirs = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "their own private query".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some("somebody-else".into()),
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0, 0.0, 0.0],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/capture")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "text=something+of+my+own&from_search={theirs}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let stored = &core.store.list_corpora(10, 0).await.unwrap()[0];
        assert_eq!(crate::core::ingest::typed_from(&stored.metadata), None);
    }

    #[tokio::test]
    async fn a_capture_nothing_processed_still_says_so() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let post = |app: axum::Router, cookie: String| async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/ui/capture")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from("text=LevelDB+tombstones+survive+compaction."))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            body_of(res).await
        };

        assert!(post(app.clone(), cookie.clone()).await.trim().is_empty());
        let again = post(app, cookie).await;
        assert!(
            again.contains("not processed again"),
            "the second paste of the same text is a duplicate: {again}"
        );
        assert!(!again.contains("searchable once it settles"));
        assert!(!again.contains(r#"hx-get="/ui/queue""#));
    }

    #[tokio::test]
    async fn a_capture_is_stamped_with_the_language_it_will_be_read_in() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let post = |app: axum::Router, cookie: String, text: &'static str, accept: &'static str| async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/ui/capture")
                        .header("cookie", &cookie)
                        .header("accept-language", accept)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(format!("text={text}")))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        };

        post(
            app.clone(),
            cookie.clone(),
            "eine+deutsche+notiz",
            "de-DE,de;q=0.9,en;q=0.8",
        )
        .await;
        let stored = &core.store.list_corpora(10, 0).await.unwrap()[0];
        assert_eq!(
            crate::infer::lang::of_corpus(&stored.metadata),
            crate::infer::lang::Lang::De
        );

        core.store
            .control
            .set_lang(
                crate::store::TEST_SUBJECT,
                Some(crate::infer::lang::Lang::Fr),
            )
            .await
            .unwrap();
        post(app.clone(), cookie.clone(), "une+autre+note", "en-GB").await;
        let stored = &core.store.list_corpora(10, 0).await.unwrap()[0];
        assert_eq!(
            crate::infer::lang::of_corpus(&stored.metadata),
            crate::infer::lang::Lang::Fr
        );

        core.store
            .control
            .set_lang(crate::store::TEST_SUBJECT, None)
            .await
            .unwrap();
        post(app, cookie, "a+third+note", "ja").await;
        let stored = &core.store.list_corpora(10, 0).await.unwrap()[0];
        assert_eq!(
            crate::infer::lang::of_corpus(&stored.metadata),
            crate::infer::lang::Lang::En
        );
    }

    #[tokio::test]
    async fn the_first_capture_can_redress_a_page_built_for_an_empty_base() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let empty = workspace("/ui").await;
        assert!(
            empty.contains(r#"data-held="0""#),
            "the page says it was built for an empty base: {empty}"
        );
        assert!(
            empty.contains(r#"data-placeholder-held="Describe the situation"#),
            "and carries the sentence its box will use once something is held, \
             because swapping a textarea would take the caret with it: {empty}"
        );

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/held")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/held")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let held = body_of(res).await;
        for region in [
            r#"id="ask-verb-slot""#,
            r#"id="decide-verb-slot""#,
            r#"id="keyhint-slot""#,
            r#"id="box-hint""#,
            r#"id="pane-idle""#,
        ] {
            assert!(held.contains(region), "{region} is not in the swap: {held}");
        }
        assert_eq!(
            held.matches(r#"hx-swap-oob="true""#).count(),
            5,
            "every region names its own target, or it lands nowhere: {held}"
        );
        assert!(
            held.contains(r#"data-verb="ask""#) && held.contains(r#"class="keyhint""#),
            "the two controls an empty base withholds: {held}"
        );
        assert!(
            held.contains("An artifact opens here"),
            "and the hints for the state it is now in: {held}"
        );
    }

    #[tokio::test]
    async fn the_ask_door_over_an_empty_base_sends_the_question_somewhere_it_works() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/ask?q=why+did+the+reindex+fail")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let to = res.headers()["location"].to_str().unwrap().to_string();
        assert!(
            to.starts_with("/ui?q="),
            "sent to the one verb an empty base can honour: {to}"
        );
        assert!(
            to.contains("why") && to.contains("reindex"),
            "with the question still in it: {to}"
        );
    }

    #[tokio::test]
    async fn the_workspace_offers_decide_once_the_base_holds_something() {
        let html = workspace_held("/ui").await;
        assert!(html.contains(r#"data-verb="decide""#), "{html}");
        let row = html.split(r#"id="decide-row""#).nth(1).expect("the row");
        assert!(row.split('>').next().unwrap().contains("hidden"), "{row}");
        assert!(
            html.contains(r#"name="decide-kind" value="noul" checked"#),
            "{html}"
        );
    }

    #[tokio::test]
    async fn an_empty_base_has_the_slot_and_no_button() {
        let html = workspace("/ui").await;
        assert!(html.contains(r#"id="decide-verb-slot""#), "{html}");
        assert!(!html.contains(r#"data-verb="decide""#), "{html}");
    }

    #[tokio::test]
    async fn the_held_bundle_swaps_the_decide_verb_in() {
        let html = workspace_held("/ui/held").await;
        assert!(
            html.contains(r#"id="decide-verb-slot" hx-swap-oob="true""#),
            "{html}"
        );
        assert!(html.contains(r#"data-verb="decide""#), "{html}");
    }

    #[tokio::test]
    async fn the_shortcut_is_listed() {
        assert!(workspace("/ui").await.contains("<dd>Decide</dd>"));
    }

    #[tokio::test]
    async fn a_disabled_verb_says_what_it_is_waiting_for() {
        let html = workspace("/ui").await;
        assert!(
            html.contains(r#"title="Type or attach something first""#),
            "the disabled verb names what it wants: {html}"
        );
        assert!(
            html.contains(r#"aria-label="Attach a file"#),
            "and Attach still names its types to a screen reader: {html}"
        );
        assert!(
            !html.contains("attach-types"),
            "but not a third time in prose: {html}"
        );
    }

    #[tokio::test]
    async fn the_box_says_that_typing_is_already_searching() {
        let core = crate::core::test_support::test_core().await;
        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("remind me"),
            "the one thing a first-time user cannot deduce from the page: {html}"
        );
        assert!(html.contains("chip-example"), "and it is pressable: {html}");
        assert!(
            html.contains("finds more than keywords do"),
            "with the one true thing the old hint said kept: {html}"
        );
    }

    #[test]
    fn a_cited_passage_gets_no_heading_over_its_excerpt() {
        let mut cited = crate::web::ui::RenderedResult {
            title: String::new(),
            html: "<p>Der Vorgang setzt voraus, dass das Journal noch steht.</p>".into(),
            ..crate::web::ui::RenderedResult::default()
        };
        cited.rank = "#1".into();
        let html = askama::Template::render(&AnswerTemplate {
            answer: "<p>An answer.</p>".into(),
            citations: vec![cited],
            dropped: 0,
            truncated: false,
            retired_only: false,
            abstained: false,
            unsupported: vec![],
            disagreements: vec![],
            event_id: None,
            verdict_bar: String::new(),
        })
        .unwrap();
        assert!(html.contains("Der Vorgang setzt voraus"), "{html}");
        assert_eq!(
            html.matches("cited-title").count(),
            0,
            "a nameless excerpt is not headed: {html}"
        );
    }

    fn answer_fixture(dropped: usize) -> String {
        askama::Template::render(&AnswerTemplate {
            answer: "<p>An answer.</p>".into(),
            citations: vec![],
            dropped,
            truncated: false,
            retired_only: false,
            abstained: false,
            unsupported: vec![],
            disagreements: vec![],
            event_id: None,
            verdict_bar: String::new(),
        })
        .unwrap()
    }

    #[test]
    fn an_answer_across_a_disagreement_names_the_other_note() {
        let html = askama::Template::render(&AnswerTemplate {
            answer: "<p>30 days [1], or 14 [2].</p>".into(),
            citations: vec![],
            dropped: 0,
            truncated: false,
            retired_only: false,
            abstained: false,
            unsupported: vec![],
            disagreements: vec![crate::store::pairs::Disagreement {
                artifact_id: "a1".into(),
                created_at: 1_767_139_200,
                other_id: "b2".into(),
                other_title: Some("Retention, revised".into()),
                other_created_at: 1_767_225_600,
                detail: Some("30 days there, 14 here".into()),
            }],
            event_id: None,
            verdict_bar: String::new(),
        })
        .unwrap();
        assert!(
            html.contains("your notes disagree: Retention, revised"),
            "{html}"
        );
        assert!(html.contains("30 days there, 14 here"), "{html}");
        assert!(
            !answer_fixture(0).contains("your notes disagree"),
            "no disagreement, no badge"
        );
    }

    #[tokio::test]
    async fn the_explain_flag_reaches_the_search_form_from_the_url() {
        let plain = workspace("/ui").await;
        assert!(
            !plain.contains(r#"name="explain""#),
            "an ordinary visit is not an operator looking into a ranking: \
             {plain}"
        );

        let asked = workspace("/ui?explain=1").await;
        assert!(
            asked.contains(r#"<input type="hidden" name="explain" value="1">"#),
            "the field the fragment request is built from is missing, so the \
             rail is asked without the flag however the link was written: \
             {asked}"
        );
    }

    #[tokio::test]
    async fn the_form_says_whether_a_refining_pass_is_worth_firing() {
        let html = workspace("/ui").await;
        assert!(
            !html.contains("data-rerank"),
            "no reranker, so the form must not advertise a refining pass"
        );

        let (core, _reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains(r#"data-rerank="true""#),
            "a reranker serving search is what arms the refining pass"
        );
        assert!(
            html.contains(r#"hx-params="q,category,rerank,explain,fold""#),
            "hx-params is the allowlist for what rides a search GET; without \
             `rerank` on it the refining pass's own flag is filtered off the \
             wire and the server only ever runs the fast path — and `explain` \
             was missing from it for exactly as long as the rail's why-line \
             was unreachable. `tz` is not on it and must not be: \
             `UiSearchParams` has no such field, so serde dropped it on every \
             keystroke — the capture path reads `#box-tz` directly"
        );
    }

    #[test]
    fn the_refining_pass_is_armed_by_the_form_and_never_by_itself() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("data-rerank"),
            "the driver reads the form's flag; without it no refine ever fires"
        );
        assert!(
            js.contains("rerank: 'true'"),
            "the refining request asks for the reranked order by name"
        );
        assert!(
            js.contains("wasRefine"),
            "a refine swap must be told apart from a typing swap, or the \
             refine reschedules off its own landing and never stops"
        );
        assert!(
            js.contains("unfilteredParameters"),
            "wasRefine must read the pre-filter parameter set: `parameters` \
             is what survived hx-params, so an allowlist edit there would \
             silently turn every refine swap back into a typing swap — and \
             the refine reschedules off its own landing forever"
        );
    }

    #[tokio::test]
    async fn the_tab_bar_points_at_the_three_places_there_are() {
        let html = workspace("/ui").await;
        let bar = html
            .split_once(r#"<nav class="oc-tabbar""#)
            .expect("the tab bar is there")
            .1;
        let bar = &bar[..bar.find("</nav>").expect("the tab bar ends")];
        assert!(
            bar.contains("/ui/insights"),
            "Insights is a destination: {bar}"
        );
        assert!(
            !bar.contains("/ui/capture"),
            "Capture is not a place any more: {bar}"
        );
        assert!(!bar.contains("/ui/ask"), "and neither is Ask: {bar}");

        let top = html
            .split_once(r#"<nav class="oc-topnav""#)
            .expect("the top row is there")
            .1;
        let top = &top[..top.find("</nav>").expect("the top row ends")];
        assert!(!top.contains(r#"href="/ui/capture""#), "{top}");
        assert!(!top.contains(r#"href="/ui/ask""#), "{top}");
    }

    #[test]
    fn a_failed_swap_says_so_where_the_answer_was_going_to_be() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let h = js
            .split_once("addEventListener('htmx:responseError'")
            .expect("nothing handles a failed swap")
            .1;
        let h = &h[..h.find("\n    });").expect("the handler does not end")];
        assert!(
            h.contains("failedSwap("),
            "only the 401 case is handled, so every other failure is silent: {h}"
        );

        assert!(
            js.contains("addEventListener('htmx:sendError'"),
            "a request that never arrived says nothing"
        );

        let f = js
            .split_once("function failedSwap(")
            .expect("the driver has no failedSwap()")
            .1;
        let f = &f[..f.find("\n  }\n").expect("failedSwap() ends")];
        assert!(
            f.contains("reasonOf(xhr)"),
            "the reason the server gave is thrown away"
        );
        assert!(
            f.contains("textContent"),
            "an error string is the one payload here that went through no renderer"
        );
        let r = js
            .split_once("function reasonOf(")
            .expect("the driver has no reasonOf()")
            .1;
        let r = &r[..r.find("\n  }\n").expect("reasonOf() ends")];
        assert!(r.contains("JSON.parse(xhr.responseText).error"), "{r}");
        assert!(r.contains("[data-error-detail]"), "{r}");
    }

    #[tokio::test]
    async fn opening_a_result_leaves_the_ask_and_capture_targets_standing() {
        let html = workspace("/ui").await;
        for id in ["ask-live", "ask-result", "capture-result", "pane-content"] {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "{id} is on first paint"
            );
        }
        let tpl = include_str!("templates/_results.html");
        assert!(
            !tpl.contains(r##"hx-target="#pane""##),
            "a result swapping the whole pane destroys the ask/capture targets"
        );
        assert!(
            tpl.contains(r##"hx-target="#pane-content""##),
            "results open inside the pane's content slot"
        );
    }

    #[test]
    fn the_kept_from_provenance_is_retired_by_the_capture_it_belongs_to() {
        let tpl = include_str!("templates/workspace.html");
        let kept = tpl
            .split_once(r#"<div id="kept-from">"#)
            .expect("the provenance is one removable block")
            .1;
        assert!(
            kept[..kept.find("</div>").expect("the block ends")].contains(r#"name="from_ask""#),
            "the hidden input is inside it, so the two go away together"
        );

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("getElementById('kept-from')") && js.contains("removeChild(kept)"),
            "a stored capture leaves its provenance behind on the page"
        );
    }

    #[test]
    fn emptying_the_box_does_not_take_away_an_answer_being_written() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let body = &js[js
            .find("function showIdle(force)")
            .expect("the idle transition")..];
        let body = &body[..body.find("function show(").expect("it ends")];
        assert!(
            body.contains("if (paneIsAsking() || (!force && paneIsBusy())) return;"),
            "the idle transition asks what the pane is doing first: {body}"
        );
        assert!(
            js.contains("function paneIsBusy()")
                && js.contains("classList.contains('asking')")
                && js.contains("getElementById('ask-result')"),
            "and an ask in flight or an answer standing both count"
        );
    }

    #[test]
    fn a_capture_brings_the_column_back_over_an_answer_that_is_finished() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let refresh = &js[js
            .find("function refreshRail()")
            .expect("the capture's rail refresh")..];
        let refresh = &refresh[..refresh.find('}').expect("it ends")];
        assert!(
            refresh.contains("showIdle(true)"),
            "the capture overrules a finished answer: {refresh}"
        );
        let body = &js[js
            .find("function showIdle(force)")
            .expect("the idle transition")..];
        let body = &body[..body.find("function show(").expect("it ends")];
        assert!(
            body.contains("paneIsAsking() ||"),
            "an ask in flight is not overruled by anything: {body}"
        );
    }

    #[test]
    fn a_press_with_a_file_is_one_capture_carrying_the_box_as_its_note() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("send(file, note);"),
            "the file goes with the box's text as its note"
        );
        assert!(
            !js.contains("if (file) send(file)"),
            "and not beside a second capture of those same words"
        );
        assert!(
            js.contains("var note = box.value.trim();"),
            "read at press time, so the order the file and the words arrived \
             in does not matter"
        );
        assert!(
            js.contains("function clearReceipts()"),
            "the node is cleared once per press"
        );
    }

    #[test]
    fn an_ask_claims_the_pane_without_hiding_what_it_was_written_from() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("regions.classList.add('answering')"),
            "pressing Ask says the pane is the act"
        );
        assert!(
            js.contains("classList.remove('has-selection', 'answering')"),
            "and a fresh result list takes it back"
        );

        let css = include_str!("../../assets/css/72-layout.css");
        let idle = css
            .split_once(":not(.pane-open):not(.answering)")
            .expect("the idle 40rem rail is not held while an answer is being written")
            .0;
        // Bounded to two-column widths: the chained `:not()`s outrank the one-up rule's single class at every width.
        assert!(
            idle.rsplit_once("@container")
                .is_some_and(|(_, open)| open.trim_start().starts_with("shell (width > 60rem)")),
            "the idle rail's track list applies where there is only one column"
        );
        assert!(
            css.contains(".regions.answering .region-rail { order: 1; }"),
            "and narrow puts the excerpts under the answer rather than over it"
        );
        let phone = include_str!("../../assets/css/78-phone.css");
        assert!(
            phone.contains(".regions.answering .region-rail { order: 1; }"),
            "including in the block that restates the narrow rules for a phone"
        );
    }

    #[test]
    fn a_fresh_list_shows_the_rail_again_without_taking_the_open_artifact_s_width() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("classList.add('has-selection', 'pane-open')"),
            "opening an artifact says both things at once"
        );
        assert!(
            js.contains("classList.remove('has-selection', 'answering')"),
            "and a fresh list takes back the one about the rail"
        );
        assert!(
            !js.contains("classList.remove('has-selection', 'answering', 'pane-open')")
                && !js.contains("classList.remove('pane-open', 'has-selection'"),
            "and only that one: the pane still holds what it held"
        );
        assert!(
            js.contains("regions.classList.remove('pane-open')"),
            "an ask empties the pane, so the pane is no longer open"
        );

        let css = include_str!("../../assets/css/72-layout.css");
        assert!(
            css.contains(":not(.pane-open):not(.answering)"),
            "the wide rail keys on what the pane holds, not on the narrow rule"
        );
        assert!(
            !css.contains(":not(.has-selection)"),
            "nothing about width is still asking the narrow question"
        );
    }

    #[test]
    fn the_split_stacks_when_the_pane_it_landed_in_is_too_narrow_for_two() {
        let css = include_str!("../../assets/css/74-workspace.css");
        assert!(
            css.contains(
                "[data-artifact] { container-type: inline-size; container-name: artifact; }"
            ),
            "the fragment's own root is the split's parent in both places it renders"
        );
        assert!(
            css.contains("@container artifact (width <= 36rem) { .split { grid-template-columns: minmax(0, 1fr); } }"),
            "and below a reading width the two halves stack"
        );
        let boxes = css
            .split_once(".split > :first-child {\n    display: flex")
            .expect("the artifact half is a scroll box where the two are side by side")
            .0;
        assert!(
            boxes
                .rsplit_once("@container")
                .is_some_and(|(_, open)| open.trim_start().starts_with("artifact (width > 36rem)")),
            "the scroll boxes agree with the backstop about what side by side means"
        );
    }

    #[tokio::test]
    async fn the_second_text_surface_is_gone() {
        let html = workspace("/ui").await;
        assert!(
            !html.contains("cmdk"),
            "the overlay is still in the layout: {html}"
        );

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(!js.contains("commandBar"), "its driver went with it");
        assert!(!js.contains("cmdk"), "and so did everything it looked up");
    }

    #[test]
    fn the_ask_disables_the_surface_and_only_stop_gives_it_back() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let stop = js
            .split_once("function stop() {")
            .expect("the driver has no stop()")
            .1;
        let stop = &stop[..stop.find("\n    }").expect("stop() does not end")];
        assert!(
            stop.contains("setBusy(false)"),
            "stop() does not give the surface back: {stop}"
        );

        let busy = js
            .split_once("function setBusy(")
            .expect("the driver has no setBusy()")
            .1;
        let busy = &busy[..busy.find("\n    }").expect("setBusy() does not end")];
        assert!(
            busy.contains("box.readOnly"),
            "setBusy does not silence the box, so typing still searches: {busy}"
        );
        assert!(
            !busy.contains("box.disabled"),
            "the box is disabled again, and the question cannot be selected \
             out of it while it is being answered: {busy}"
        );

        assert_eq!(
            js.matches("setBusy(false)").count(),
            1,
            "setBusy(false) is called from more than one place"
        );
    }

    #[test]
    fn the_two_verbs_are_reachable_from_the_keys() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let chord = js
            .split_once("if (e.key !== 'Enter'")
            .expect("nothing commits the box from the keyboard")
            .1;
        let chord = &chord[..chord.find("\n    });").expect("the handler does not end")];
        assert!(
            chord.contains("e.shiftKey ? 'capture' : 'ask'"),
            "the chord does not pick a verb: {chord}"
        );
        assert!(
            chord.contains("verb.disabled") && chord.contains("verb.click()"),
            "the chord goes around the button instead of pressing it: {chord}"
        );
    }

    #[test]
    fn the_box_measures_its_own_padding_and_remeasures_on_a_resize() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let grow = js
            .split_once("function grow() {")
            .expect("the box does not size itself")
            .1;
        let grow = &grow[..grow.find("\n    }").expect("grow() does not end")];
        assert!(
            grow.contains("box.offsetHeight - box.clientHeight"),
            "grow() does not add the border back: {grow}"
        );
        assert!(
            grow.contains("+ pad + border"),
            "grow() sets a height that is not the box's own outside: {grow}"
        );
        assert!(
            js.contains("window.addEventListener('resize', grow)"),
            "a re-wrap at a new width leaves the height where the last keystroke put it"
        );
        assert!(
            js.contains("document.fonts.ready.then(grow)"),
            "the first measurement is of the fallback face, and nothing recomputes it"
        );
    }

    #[test]
    fn the_answer_says_what_was_dropped_in_words_a_person_uses() {
        let html = answer_fixture(18);
        assert!(!html.contains("excerpt(s)"), "{html}");
        assert!(!html.contains("context budget"), "{html}");
        assert!(html.contains("18 more excerpts did not fit"), "{html}");
        let one = answer_fixture(1);
        assert!(one.contains("1 more excerpt did not fit"), "{one}");
    }

    #[tokio::test]
    async fn an_ask_in_flight_offers_a_way_to_stop_it() {
        let html = workspace_held("/ui/ask").await;
        assert!(html.contains(r#"id="ask-stop""#), "{html}");
    }

    #[tokio::test]
    async fn an_ask_does_not_open_with_the_models_reasoning_showing() {
        let html = workspace_held("/ui/ask").await;
        assert!(html.contains("ask-reasoning-box"), "{html}");
        assert!(
            !html.contains("<details open")
                && !html.contains(r#"<details id="ask-reasoning-box" open"#),
            "reasoning must start closed: {html}"
        );
    }

    #[tokio::test]
    async fn the_stream_driver_handles_every_event_the_server_names() {
        let src = include_str!("workspace.rs");
        let body = &src[src
            .find("fn sse_event(")
            .expect("sse_event is in this file")..];
        let body = &body[..body.find("\n}\n").unwrap()];
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

    #[tokio::test]
    async fn an_unforced_note_is_a_plain_capture_on_the_receipt() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/capture")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("text=Dear+diary,+the+move+is+over."))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            !html.contains("Kept as today&rsquo;s entry"),
            "nothing is filed at the door any more: {html}"
        );
    }

    #[tokio::test]
    async fn the_rails_recent_row_links_its_stamp_to_the_day() {
        let html = workspace_held("/ui").await;
        assert!(html.contains(r#"href="/ui/day/"#), "{html}");
        assert!(html.contains("data-day-link"));
        assert!(html.contains("data-day-at="), "{html}");
    }

    #[test]
    fn the_box_sends_its_zone_and_a_day_link_is_rebuilt_locally() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("getElementById('box-tz')"),
            "the capture reads the same hidden field the echo already does"
        );
        assert!(
            js.contains("data-day-at"),
            "a day link is rebuilt from the instant, in the viewer's zone: the              date the server wrote into the path is UTC"
        );
        assert!(
            js.contains("function localDay("),
            "and from the local parts, never sliced out of toISOString, which              is the UTC this exists to correct"
        );
    }

    #[tokio::test]
    async fn the_capture_receipt_outlives_the_page_going_idle() {
        let html = workspace_held("/ui").await;
        let pane = html
            .find(r#"<div id="pane""#)
            .expect("the pane is on the page");
        let receipt = html
            .find(r#"<div id="capture-result">"#)
            .expect("and so is the receipt");
        assert!(
            receipt < pane,
            "the receipt is above the pane, not inside it: {html}"
        );
    }

    #[tokio::test]
    async fn the_box_stores_the_zone_it_was_typed_in() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let post = |app: axum::Router, cookie: String, body: &'static str| async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/ui/capture")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            body_of(res).await
        };

        post(
            app.clone(),
            cookie.clone(),
            "text=Remind+me+tomorrow+at+9&tz=Europe%2FBerlin",
        )
        .await;
        let stored = core.store.recent_captures(1).await.unwrap();
        let cid = stored[0].0.clone();
        assert_eq!(
            core.store.get_corpus(&cid).await.unwrap().metadata["tz"],
            "Europe/Berlin"
        );

        post(app, cookie, "text=Another+note+entirely&tz=Europe%2FBerlim").await;
        let stored = core.store.recent_captures(1).await.unwrap();
        let cid = stored[0].0.clone();
        assert!(core.store.get_corpus(&cid).await.unwrap().metadata["tz"].is_null());
    }

    fn dictation(cookie: &str, mime: Option<&str>, body: &[u8]) -> Request<Body> {
        const B: &str = "engramtestboundary";
        let typed = match mime {
            Some(m) => format!("Content-Type: {m}\r\n"),
            None => String::new(),
        };
        let mut buf: Vec<u8> = format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"audio\"; filename=\"recording\"\r\n{typed}\r\n"
        )
        .into_bytes();
        buf.extend_from_slice(body);
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(format!("--{B}--\r\n").as_bytes());
        Request::builder()
            .uri("/ui/transcribe")
            .method("POST")
            .header("cookie", cookie)
            .header("content-type", format!("multipart/form-data; boundary={B}"))
            .body(Body::from(buf))
            .unwrap()
    }

    #[tokio::test]
    async fn the_microphone_is_on_the_box_only_where_a_speech_model_is_configured() {
        let html = workspace("/ui").await;
        assert!(
            !html.contains("id=\"mic\""),
            "shipped default has no microphone"
        );

        let core = crate::core::test_support::test_core_with_transcriber(std::sync::Arc::new(
            crate::infer::fake::FakeTranscriber::default(),
        ))
        .await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(html.contains("id=\"mic\""), "{html}");
        assert!(
            html.contains("box-with-mic"),
            "the box has to leave room for it: {html}"
        );
    }

    #[tokio::test]
    async fn dictation_hands_back_the_words_and_stores_nothing() {
        let heard = std::sync::Arc::new(crate::infer::fake::FakeTranscriber::saying(
            "tombstones in leveldb",
        ));
        let core = crate::core::test_support::test_core_with_transcriber(heard.clone()).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let res = app
            .oneshot(dictation(
                &cookie,
                Some("audio/webm;codecs=opus"),
                b"fake audio",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_of(res).await, "tombstones in leveldb");

        assert_eq!(heard.calls(), 1);
        assert_eq!(
            heard.last_mime(),
            "audio/webm;codecs=opus",
            "the container the browser recorded in is passed through"
        );
        assert_eq!(heard.last_len(), b"fake audio".len());
        assert!(core.store.recent_captures(5).await.unwrap().is_empty());
    }

    #[test]
    fn the_recording_is_converted_to_wav_before_it_is_uploaded() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("toWav(blob).then(send)"),
            "the recorder's own container never reaches the upload"
        );
        assert!(
            js.contains("var STT_RATE = 16000;"),
            "and it is resampled to what whisper reads internally anyway"
        );
        for spelling in [
            "ascii(0, 'RIFF')",
            "ascii(8, 'WAVE')",
            "v.setUint16(20, 1, true)",
            "v.setUint16(22, 1, true)",
            "v.setUint16(34, 16, true)",
            "type: 'audio/wav'",
        ] {
            assert!(
                js.contains(spelling),
                "the RIFF header is missing {spelling}"
            );
        }
        assert!(
            js.contains(".catch(function () { return blob; })"),
            "a browser that cannot decode its own output still uploads what it has"
        );
    }

    #[tokio::test]
    async fn an_empty_recording_costs_no_call() {
        let heard = std::sync::Arc::new(crate::infer::fake::FakeTranscriber::default());
        let core = crate::core::test_support::test_core_with_transcriber(heard.clone()).await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(dictation(&cookie, Some("audio/webm"), b""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_of(res).await, "");
        assert_eq!(heard.calls(), 0);
    }

    #[tokio::test]
    async fn there_is_no_transcription_route_without_a_speech_model() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(dictation(&cookie, Some("audio/webm"), b"audio"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_failing_speech_endpoint_is_a_bad_gateway() {
        let heard = std::sync::Arc::new(crate::infer::fake::FakeTranscriber::failing(
            "connection refused",
        ));
        let core = crate::core::test_support::test_core_with_transcriber(heard).await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(dictation(&cookie, Some("audio/webm"), b"audio"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    }
}
