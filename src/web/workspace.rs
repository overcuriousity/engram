//! One text surface, and the page built around it.
//!
//! Capture, search and ask were three pages, and moving between them meant
//! retyping or carrying a prefill: the same words are a query on one, a
//! question on the second and a document on the third, and the operator
//! navigated to say which. Here the box never changes shape and the verb is a
//! button — typing searches, `Ask` spends the model call, `Capture` stores
//! what is in the box.
//!
//! The three old doors still open. They are deep links into this page now,
//! which is what keeps a bookmark, the extension's capture post and the
//! *keep this answer* flow working. See
//! `docs/superpowers/specs/2026-08-22-one-text-surface-design.md` §3.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::auth::Identity;
use crate::core::ingest::{ORIGIN_ASK, ORIGIN_WEB};
use crate::error::{Error, Result};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui::{
    FACET_LIMIT, RenderedResult, UiSearchParams, ensure_facet, link_citations, render_hit,
    search_results,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        // `/` and `/ui` both land here rather than redirecting onward: there
        // is one page, so there is nothing to redirect to.
        .route("/", get(page))
        .route("/ui", get(page))
        // A deep link, and what a bookmark from before this change points at.
        .route("/ui/search", get(page))
        .route("/ui/search/results", get(search_results))
        // The capture door. GET is the workspace with the box filled; POST is
        // what the button on it and the browser extension both send.
        .route("/ui/capture", get(capture_door).post(capture_submit))
        // The ask door, and the two-request stream behind it: the POST parks
        // the question and hands back an id, and an `EventSource` spends it.
        .route("/ui/ask", get(ask_door).post(ask_submit))
        .route("/ui/ask/{id}/stream", get(ask_stream))
        .route("/ui/ask/{id}/verdict", post(ask_verdict))
        .route("/ui/ask/{id}/carried", post(ask_carried))
        .route("/ui/ask/{id}/keep", post(ask_keep))
}

#[derive(Template)]
#[template(path = "workspace.html")]
struct WorkspaceTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`. The `Ask`
    /// button is absent where it is false — the door is simply not there,
    /// rather than greyed out over a page that says so.
    ask_enabled: bool,
    /// The box's contents on arrival: a deep link's `?q=`, or an answer kept
    /// from an ask. One field, because there is one box.
    q: String,
    /// What this collection can actually be narrowed by, rendered as chips in
    /// the verb row, so choosing a category never means knowing in advance
    /// that it exists.
    facets: crate::vector::Facets,
    /// The chip a deep link arrived with, so the row comes back selected
    /// rather than reset to "all".
    category: String,
    /// Whether the area under the box exists at all. See `Core::recommends`.
    recommend: bool,
    /// Whether the image door is open, i.e. `[infer.vision]` is configured.
    /// Off, the control offers text only rather than a picker that fails.
    vision_enabled: bool,
    /// Whether capture spends a synthesis call per segment, i.e. `eager`.
    ///
    /// At `earned` and `off` it spends none: the text is embedded as written,
    /// and at `earned` a window is rewritten later only where reading has
    /// earned it. The page has to say which of those is happening — promising
    /// "16 model calls" on a base that will make none is the page lying about
    /// what the button costs.
    eager: bool,
    /// The ask the box was filled from, carried through the form so the
    /// capture records where the text came from. Empty on an ordinary visit.
    ///
    /// The id rather than the prose: a note is a string someone can edit away,
    /// while this is the join back to the question and the artifacts the
    /// answer was built from, and `capture_submit` turns it into stored
    /// provenance.
    prefill_ask: String,
    /// The question this answer answered, in the operator's own words.
    ///
    /// The provenance already recorded it — `with_ask` carries the question
    /// and the citations into the corpus metadata. What was missing was saying
    /// so on the page: the box arrived holding an answer with no sign of what
    /// it was an answer to, and the operator deciding whether to keep it is
    /// the person who most needs to see the question.
    prefill_question: String,
    /// What app.js should do on first paint: `""`, `"ask"` or `"capture"`.
    /// Search needs no value — typing already covers it. Rendered into
    /// `data-open-with`, so the decision is made here and the template holds
    /// no logic of its own.
    open_with: &'static str,
}

/// Everything every door renders, before the door says what it opened for.
///
/// Split out because the three deep links differ only in what is in the box
/// and what happens on first paint; a copy of this per door is how they come
/// to disagree about the chips.
async fn base_template(st: &AppState, q: String, category: String) -> Result<WorkspaceTemplate> {
    // A vector store that cannot answer must not take the page down with it:
    // without chips the page is what it was yesterday, with them it is better,
    // and neither is worth a 500.
    let mut facets = st
        .core
        .vectors
        .facets(FACET_LIMIT)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "facets unavailable; rendering the workspace without chips");
            Default::default()
        });
    // A deep link can name a value that falls outside the top `FACET_LIMIT`,
    // or one nothing carries at all. The rail is narrowed by it either way, so
    // the chip row has to show it: otherwise the page reads as unfiltered
    // while the results are not, and there is no chip to click to get back
    // out.
    ensure_facet(&mut facets.categories, &category);
    Ok(WorkspaceTemplate {
        judge_pending: crate::web::state::judge_pending(st).await,
        ask_enabled: crate::web::state::ask_enabled(st),
        q,
        facets,
        category,
        recommend: st.core.recommends(),
        vision_enabled: st.core.describer.is_some(),
        eager: st.core.synthesis == crate::config::SynthesisMode::Eager,
        prefill_ask: String::new(),
        prefill_question: String::new(),
        open_with: "",
    })
}

async fn page(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<UiSearchParams>,
) -> Result<Response> {
    let t = base_template(&st, p.q, p.category.unwrap_or_default()).await?;
    Ok(HtmlTemplate(t).into_response())
}

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

/// The capture door: the workspace with the box already filled.
///
/// The extension posts here and so does *keep this answer*, and neither knows
/// anything about the three pages having folded into one. A prefill that names
/// an ask nobody recorded is not an error worth a page for: the box is simply
/// empty, which is what an ordinary visit looks like.
async fn capture_door(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<CapturePrefill>,
) -> Result<Response> {
    let prefilled = match &p.from_ask {
        Some(id) => st.core.store.ask_event(id).await?,
        None => None,
    };
    let (q, prefill_ask, prefill_question) = match prefilled {
        Some(ev) => (ev.answer, ev.id, ev.question),
        None => (String::new(), String::new(), String::new()),
    };
    let mut t = base_template(&st, q, String::new()).await?;
    t.open_with = "capture";
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

#[derive(serde::Deserialize)]
struct VerdictForm {
    verdict: String,
}

#[derive(serde::Deserialize)]
struct CarriedForm {
    n: i64,
}

/// The ask door: the workspace with the question already in the box and an
/// answer requested on first paint. A gap's "ask again" links here.
///
/// No ask model, no ask door: the route is not there. See `Core::asks`.
///
/// The last-query fallback this handler used to carry is gone with the pages
/// it bridged. It existed because a query typed on the rail and then retyped
/// into ask was the cost of two pages with nothing carried between them —
/// there is one box now, and the query is already in it.
async fn ask_door(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<AskPrefill>,
) -> Result<Response> {
    if !st.core.asks() {
        return Err(crate::error::Error::NotFound);
    }
    let mut t = base_template(&st, p.q, String::new()).await?;
    t.open_with = "ask";
    Ok(HtmlTemplate(t).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test_support::{app_with_cookie, body_of};
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

    /// One act in flight. Pressing Ask disables the box, and disabling the box
    /// is what disables search-while-type: a disabled input fires no `keyup`,
    /// so the form's `hx-trigger` goes quiet with no second mechanism and no
    /// flag to keep in sync.
    ///
    /// The re-enable belongs in `stop()` and nowhere else. Every exit already
    /// runs through it — the answer completing, the Stop button, and the
    /// transport error that `fail()` funnels into it. Put it on the `done`
    /// handler instead and a dropped connection leaves the box disabled
    /// forever, with no way back but a reload.
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
            busy.contains("box.disabled"),
            "setBusy does not disable the box, so typing still searches: {busy}"
        );

        // The other half: nothing else may re-enable it. A second caller is a
        // second thing to keep in step with the three exits.
        assert_eq!(
            js.matches("setBusy(false)").count(),
            1,
            "setBusy(false) is called from more than one place"
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

    #[tokio::test]
    async fn an_ask_in_flight_offers_a_way_to_stop_it() {
        // Fifty seconds signalled by a small grey "thinking…" beside the
        // button, and nothing on the page to end it with.
        let html = workspace("/ui/ask").await;
        assert!(html.contains(r#"id="ask-stop""#), "{html}");
    }

    #[tokio::test]
    async fn an_ask_does_not_open_with_the_models_reasoning_showing() {
        // The deployment streamed the chain of thought into the page for fifty
        // seconds, restating the prompt's own constraints verbatim — "Answer
        // *only* using the provided knowledge-base excerpts" — above the empty
        // space where the answer was going to be.
        let html = workspace("/ui/ask").await;
        assert!(html.contains("ask-reasoning-box"), "{html}");
        assert!(
            !html.contains("<details open")
                && !html.contains(r#"<details id="ask-reasoning-box" open"#),
            "reasoning must start closed: {html}"
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
        let src = include_str!("workspace.rs");
        let body = &src[src
            .find("fn sse_event(")
            .expect("sse_event is in this file")..];
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
}
