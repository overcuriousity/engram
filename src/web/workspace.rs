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
use axum::extract::{Form, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::auth::Identity;
use crate::core::ingest::{ORIGIN_ASK, ORIGIN_WEB};
use crate::error::Result;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui::{FACET_LIMIT, UiSearchParams, ensure_facet, search_results};

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
