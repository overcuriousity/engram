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

use crate::tenants::Tenant;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

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
        // The held-state regions, out of band. See `held_regions`.
        .route("/ui/held", get(held_regions))
        // The ask door, and the two-request stream behind it: the POST parks
        // the question and hands back an id, and an `EventSource` spends it.
        .route("/ui/ask", get(ask_door).post(ask_submit))
        .route("/ui/ask/{id}/stream", get(ask_stream))
        .route("/ui/ask/{id}/verdict", post(ask_verdict))
        // Judging at the moment of search: the bar under an opened result and
        // the gap button on the rail. Open to whoever searched, like the ask
        // bar — it is their own search — and unlike the deck, whose grant also
        // covers writing `config.toml`.
        .route("/ui/search/{id}/verdict", post(search_verdict))
        .route("/ui/search/{id}/gap", post(search_gap))
        .route("/ui/ask/{id}/carried", post(ask_carried))
        .route("/ui/ask/{id}/keep", post(ask_keep))
}

#[derive(Template)]
#[template(path = "workspace.html")]
struct WorkspaceTemplate {
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
    /// Whether this page asks the rail to say why each row ranked where it
    /// did — `/ui?explain=1`. Carried onto the form as a hidden field and
    /// named in `hx-params`, because that allowlist strips everything it does
    /// not name and the fragment would otherwise never see the flag.
    ///
    /// A link rather than a control: the line is for an operator looking into
    /// a ranking, and a page that paints it on every keystroke for everybody
    /// is a page with a sentence under every row.
    explain: bool,
    /// Whether a reranker serves the search path — `Core::reranks_search`.
    /// Rendered onto the form as `data-rerank`, which is what arms app.js's
    /// refining pass: without it no second request fires, ever, because it
    /// could only buy the same order back.
    search_reranks: bool,
    ///
    /// At `earned` and `off` it spends none: the text is embedded as written,
    /// and at `earned` a window is rewritten later only where reading has
    /// earned it. The page has to say which of those is happening — promising
    /// "16 model calls" on a base that will make none is the page lying about
    /// what the button costs.
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
    /// Which door this is: `""` for the workspace and its search deep link,
    /// `"ask"` or `"capture"` for the two that arrive with the box already
    /// filled.
    ///
    /// It gates the form's `load` trigger. A filled box is a search to run
    /// only where a search filled it; the other two doors fill it with a
    /// question or with an answer being kept, and running a search over
    /// either was a query nobody typed — an embedding call, an activation
    /// bump and a Judge-queue row for the capture door especially, whose box
    /// holds a whole model answer.
    ///
    /// It also decides `idle`: a door that runs no search on arrival has to
    /// render the rail's idle state, or the rail is the column of nothing
    /// that state exists to remove.
    open_with: &'static str,
    /// The idle rail, pre-rendered: the base introducing itself, and an empty
    /// string only where the `load` trigger is about to fill the rail with
    /// results — rendering an introduction under them for one round trip
    /// would be flicker.
    ///
    /// Which is not the same as "the box is empty". The ask and capture doors
    /// arrive with a filled box and run nothing, so for them there is no
    /// round trip coming and no results to flicker under: without this they
    /// rendered a rail with nothing in it at all.
    ///
    /// Rendered here rather than composed in the template because the same
    /// fragment is what the results endpoint returns when the box is emptied
    /// — one account of the idle state, however it is reached.
    idle: String,
    /// Whether this page paints its idle state: the column under the box, and
    /// no rail and no pane. Exactly the condition `idle` is computed from, and
    /// deliberately not "the box is empty" — the ask and capture doors arrive
    /// with a filled box and run nothing, so for them nothing is coming and
    /// the idle column is what the page has to show.
    idle_state: bool,
    /// Whether the base holds anything at all.
    ///
    /// Onboarding here is a property of an empty base rather than of a new
    /// user: no flag is stored, nothing is dismissed, and the same page serves
    /// someone who has just arrived and someone who has just deleted
    /// everything. Two of the three verbs cannot work with nothing held —
    /// search returns nothing and ask can only abstain — so the page offers
    /// the one that can, and the rest appears when there is something for it
    /// to act on.
    held: bool,
    /// The two example phrasings under the box, in the reader's language. See
    /// `moments::examples_for`: they are the classifier's own prototypes, so
    /// what the page teaches and what the base recognises cannot drift apart.
    example_remind: &'static str,
    example_journal: &'static str,
    /// Always false here: the shared partials this page renders inline —
    /// `_ask_verb.html`, `_keyhint.html`, `_box_hint.html`, `_pane_idle.html`
    /// — mark themselves `hx-swap-oob` for the other caller, `HeldTemplate`,
    /// which swaps the same four regions into a page already on screen.
    oob: bool,
}

/// The four regions that read differently once the base stops being empty,
/// rendered for an out-of-band swap into a workspace that is already open.
///
/// The page is built once. A capture posts to `/ui/capture` and swaps a
/// receipt; the rail refresh swaps `#results`. Nothing else moves — so the
/// paste that ends an empty base left the whole page still arranged for one,
/// which is the state onboarding exists to leave. Fetched by app.js on the
/// first capture that stores, and only that one.
///
/// The same partials the workspace includes, so the two accounts of what the
/// held state says cannot drift apart.
#[derive(Template)]
#[template(path = "_held.html")]
struct HeldTemplate {
    ask_enabled: bool,
    /// See `WorkspaceTemplate`: `_box_hint.html` is one of the partials this
    /// swaps, and it reads these two.
    example_remind: &'static str,
    example_journal: &'static str,
    /// True by definition: this fragment exists only for the transition into
    /// it. The field is here because the partials branch on it.
    held: bool,
    /// True by definition, for the same reason.
    oob: bool,
}

/// Serves `HeldTemplate`, and only where something really is held: a page that
/// asked for this before its capture stored would swap in an Ask verb over a
/// base that still cannot answer.
async fn held_regions(tenant: Tenant, headers: axum::http::HeaderMap) -> Result<Response> {
    let (corpora, _) = tenant.core.store.held_brief().await?;
    if corpora == 0 {
        return Err(crate::error::Error::NotFound);
    }
    let (example_remind, example_journal) = examples(&headers);
    Ok(HtmlTemplate(HeldTemplate {
        ask_enabled: crate::web::state::ask_enabled(&tenant),
        example_remind,
        example_journal,
        held: true,
        oob: true,
    })
    .into_response())
}

/// Everything every door renders, before the door says what it opened for.
///
/// Split out because the three deep links differ only in what is in the box
/// and what happens on first paint; a copy of this per door is how they come
/// to disagree about the chips.
/// The reader's language, off the request. A header that is absent, empty or
/// unreadable is a reader we know nothing about, and English is what the page
/// says then.
fn examples(headers: &axum::http::HeaderMap) -> (&'static str, &'static str) {
    let raw = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok());
    crate::core::moments::examples_for(raw.unwrap_or(""))
}

async fn base_template(
    tenant: &Tenant,
    headers: &axum::http::HeaderMap,
    q: String,
    category: String,
    open_with: &'static str,
) -> Result<WorkspaceTemplate> {
    // A vector store that cannot answer must not take the page down with it:
    // without chips the page is what it was yesterday, with them it is better,
    // and neither is worth a 500.
    let mut facets = tenant
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
    // The same condition the template's `load` trigger is written from, and
    // its complement: whatever will not be filled by a search on arrival is
    // filled by the idle rail here.
    let idle_state = q.is_empty() || !open_with.is_empty();
    // Rendered whatever state the page arrived in. It is the idle column's
    // closing line now rather than the rail's contents, so there is no list to
    // flicker under a result set that is about to arrive — and a deep link
    // whose box is cleared has to find the line already there, because what
    // comes back from the results endpoint is an out-of-band swap onto it.
    let idle = crate::web::ui::idle_foot(tenant, false)
        .await?
        .render()
        .map_err(|e| crate::error::Error::Internal(e.to_string()))?;
    // The slimmest read there is, and the same one the idle rail takes. Asked
    // unconditionally because the deep-link path renders no idle rail and
    // still has to know: a search URL against an empty base is a page that
    // must not offer Ask either.
    let (corpora, _) = tenant.core.store.held_brief().await?;
    let (example_remind, example_journal) = examples(headers);
    Ok(WorkspaceTemplate {
        ask_enabled: crate::web::state::ask_enabled(tenant),
        q,
        facets,
        category,
        recommend: tenant.core.recommends(),
        vision_enabled: tenant.core.describer.is_some(),
        search_reranks: tenant.core.reranks_search(),
        // The deep-link door sets it; every other door onto this page is not
        // an operator looking into a ranking.
        explain: false,
        prefill_ask: String::new(),
        prefill_question: String::new(),
        open_with,
        idle,
        idle_state,
        example_remind,
        example_journal,
        held: corpora > 0,
        oob: false,
    })
}

async fn page(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Query(p): Query<UiSearchParams>,
) -> Result<Response> {
    let explain = p.explain.unwrap_or(false);
    let mut t = base_template(&tenant, &headers, p.q, p.category.unwrap_or_default(), "").await?;
    t.explain = explain;
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
    /// The viewer's zone, from the hidden `#box-tz` app.js fills on load — the
    /// same field the echo under the box already reads its dates in.
    ///
    /// Without it the flagship path stored no zone at all: *remind me tomorrow
    /// at 9*, echoed under the box in Berlin as 09:00, landed as a moment at
    /// 09:00 UTC and the band underneath said 11:00.
    #[serde(default)]
    tz: Option<String>,
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
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Form(f): Form<CaptureForm>,
) -> Result<Response> {
    // Which of the ten the reading of this text is instructed in: the account
    // setting, or this browser's `Accept-Language` where it is on automatic.
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
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

/// The zone the box was typed in, as the zone table spells it.
///
/// A name that does not parse is dropped rather than refused: this is a hidden
/// field the browser fills from `Intl`, nobody typed it, and a 400 here would
/// lose the text of a capture over a zone database that disagrees with ours.
/// Dropping it falls back to `time.default_tz`, which is the server's own zone.
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

/// The capture door: the workspace with the box already filled.
///
/// The extension posts here and so does *keep this answer*, and neither knows
/// anything about the three pages having folded into one. A prefill that names
/// an ask nobody recorded is not an error worth a page for: the box is simply
/// empty, which is what an ordinary visit looks like.
async fn capture_door(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Query(p): Query<CapturePrefill>,
) -> Result<Response> {
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
    /// Every excerpt this was written from has been retired. Badged, because
    /// an answer drawn entirely from what the base has put away is a different
    /// kind of answer and the reader is the one who decides what to do about
    /// it.
    retired_only: bool,
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
    tenant: Tenant,
    Form(f): Form<AskForm>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
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
        &tenant.user.subject,
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
    tenant: Tenant,
    Path(handoff): Path<String>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    use tokio_stream::StreamExt as _;

    // Unknown, already spent, expired, or somebody else's — all one answer.
    // Never a fresh ask against an empty question, which would spend a model
    // call on a replay; never another subject's question, which would be
    // answered to the wrong person and recorded under their name.
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
            .map(|(i, h)| render_hit(i, h, &Default::default(), false))
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
            .map(|(i, h)| render_hit(i, h, &Default::default(), false))
            .collect(),
        dropped: out.dropped,
        truncated: out.truncated,
        retired_only: out.retired_only,
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
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    match f.verdict.as_str() {
        "none" => tenant.core.store.unjudge_ask(&id).await?,
        v => {
            let verdict = crate::store::asks::AskVerdict::parse(v)
                .ok_or_else(|| Error::Validation(format!("unknown verdict {v}")))?;
            tenant.core.store.judge_ask(&id, verdict).await?;
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

/// What the bar says when the write it was drawn for is refused. Every guard
/// on this path — a verdict from the deck, a search that expired, a query the
/// box has since moved on from — reaches the person as the same ordinary
/// sentence, because from where they are sitting it is the same fact: the
/// search they were looking at is no longer waiting for them.
fn already_judged() -> Response {
    axum::response::Html(
        r#"<span class="muted">nothing to record — that search was already judged.</span>"#,
    )
    .into_response()
}

/// The bar under an opened result. `none` is the undo; `no` is "not this
/// one", which leaves the search a question for the deck.
async fn search_verdict(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<SearchVerdictForm>,
) -> Result<Response> {
    if !tenant.core.learn.enabled {
        return Err(Error::NotFound);
    }
    use crate::store::feedback::Labeller;
    let store = &tenant.core.store;
    // The id comes off the page, so it is whatever the caller sent. One check
    // for all four answers below — see `Store::event_is_mine`.
    if !store.event_is_mine(&id, &tenant.user.subject).await? {
        return Err(Error::NotFound);
    }
    let state = match f.verdict.as_str() {
        "hit" => {
            // `eval::export` drops any pair naming an artifact search will
            // not return, so recording one here would raise the recall on
            // Insights while contributing nothing to `pairs.json`. The bar is
            // not drawn over such an artifact at all — see
            // `ui::artifact_detail` — so this is the write refusing what the
            // page already refuses to offer.
            if !store.get_artifact(&f.artifact_id).await?.in_results() {
                return Err(Error::Validation(
                    "that one is deprecated or superseded, so the benchmark can't hold it".into(),
                ));
            }
            // `NotFound` here is the store's guard, not a missing route:
            // another tab can answer this search while the one holding this
            // bar is open, and `judge_hit` refuses to write over a verdict
            // rather than replace it. The same line "no" gets, for the same
            // reason.
            match store
                .judge_hit(&id, &f.artifact_id, Labeller::Confirm)
                .await
            {
                Ok(()) => "hit",
                Err(Error::NotFound) => return Ok(already_judged()),
                Err(e) => return Err(e),
            }
        }
        "no" => {
            // The one answer that clears columns rather than filling them, so
            // the one that can undo somebody else's work: another tab can
            // judge this search while the one holding this bar is open. Refused
            // rather than applied, and said in the same words the rail's gap
            // button uses for the same situation.
            if !store.decline(&id).await? {
                return Ok(already_judged());
            }
            "no"
        }
        // Not `Verdict::Discard`, which says something else entirely: that
        // the search was never real. A discard is dropped from the eval pairs
        // for good, and — alone among the verdicts — not exempt from the
        // retention purge. Somebody unsure whether the result in front of them
        // was the one has not said any of that. Skip is what the label
        // promises: the search stays a question.
        "skip" => {
            // Not a verdict, so nothing can have got here first, and a search
            // already gone was refused by the ownership check above. What is
            // left is the window between that check and this write, which
            // retention and an Ops purge can both fall into. `judged_one` calls
            // it `NotFound`, and a 404 is the one answer htmx will not swap —
            // the button would do nothing at all, silently, where the other
            // three say so in words.
            match store.skip_event(&id).await {
                Ok(()) => "skip",
                Err(Error::NotFound) => return Ok(already_judged()),
                Err(e) => return Err(e),
            }
        }
        "none" => {
            // Only back over what this bar wrote — `Labeller::Confirm`. The
            // undo appears after "no", which leaves the search pending, so
            // another tab can record a hit on it while this one is still open;
            // unguarded, this button then erased a confirmed pair. The store
            // says so by matching nothing.
            match store.unjudge(&id, Labeller::Confirm).await {
                Ok(()) => "",
                Err(Error::NotFound) => return Ok(already_judged()),
                Err(e) => return Err(e),
            }
        }
        v => return Err(Error::Validation(format!("unknown verdict {v}"))),
    };
    // A verdict is what buys the next measurement. The deck used to spawn the
    // sweep after each of its verdicts; the bar and the rail are the labellers
    // now, so the check rides here — off the request path, and cheap when the
    // thresholds say no.
    crate::eval::sweep::maybe_spawn(&tenant.core);
    Ok(HtmlTemplate(SearchVerdictTemplate {
        event_id: id,
        artifact_id: f.artifact_id,
        state,
    })
    .into_response())
}

/// The rail's "nothing here has it": a gap against the search that filled the
/// rail, named by the page the way an open is. Answers with the line that
/// replaces the button.
///
/// The query travels with the id for the same reason the open path names an
/// artifact: it is what the button was drawn over, and a trailing keystroke can
/// fold a later wording into the row between the render and the press. See
/// `Store::gap_event`.
#[derive(serde::Deserialize)]
struct GapParams {
    q: String,
}

async fn search_gap(
    tenant: Tenant,
    Path(id): Path<String>,
    axum::extract::Query(p): axum::extract::Query<GapParams>,
) -> Result<Response> {
    if !tenant.core.learn.enabled {
        return Err(Error::NotFound);
    }
    // Only against the caller's own search — see `Store::event_is_mine`.
    if !tenant
        .core
        .store
        .event_is_mine(&id, &tenant.user.subject)
        .await?
    {
        return Err(Error::NotFound);
    }
    let line = match tenant.core.store.gap_event(&id, p.q.trim()).await? {
        true => "recorded as a gap: your base doesn't know this yet.",
        false => "nothing to record — that search was already judged.",
    };
    // A gap is a verdict too, and counts towards the sweep's floor.
    crate::eval::sweep::maybe_spawn(&tenant.core);
    Ok(axum::response::Html(format!(r#"<span class="muted">{line}</span>"#)).into_response())
}

async fn ask_carried(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<CarriedForm>,
) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
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

/// Keep an answer: store it as a source, here, without a detour through the
/// capture box.
///
/// The same pipeline as any paste — one corpus, segmented, embedded, searchable
/// — and the same concession the capture door already made: `origin = "ask"`
/// and the `ask` metadata, so what the base holds says a model wrote it, from
/// which question, and from which artifacts. Nothing about it is special
/// downstream: captured verbatim, ending in artifacts with vectors.
///
/// The answer as the model wrote it, not as the operator retyped it: an
/// operator who wants to edit first has `edit first` beside this, which is the
/// old path unchanged.
async fn ask_keep(tenant: Tenant, Path(id): Path<String>) -> Result<Response> {
    // No ask model, no ask door: the route is not there. See `Core::asks`.
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    // Unlike the capture door, there is no text to fall back to here: the row
    // is where the answer lives. A question that retention has already taken
    // has nothing left to keep, and saying so is better than storing an empty
    // source or an unprovenanced one.
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

/// The ask door: the workspace with the question already in the box, and
/// still. A gap's "ask again" links here.
///
/// Nothing is asked on arrival. A GET that spends a model call is a bill any
/// link, prefetch or reload can run up, and the question is one press from
/// where the door leaves it.
///
/// No ask model, no ask door: the route is not there. See `Core::asks`.
///
/// The last-query fallback this handler used to carry is gone with the pages
/// it bridged. It existed because a query typed on the rail and then retyped
/// into ask was the cost of two pages with nothing carried between them —
/// there is one box now, and the query is already in it.
async fn ask_door(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Query(p): Query<AskPrefill>,
) -> Result<Response> {
    if !tenant.core.asks() {
        return Err(crate::error::Error::NotFound);
    }
    // Nothing held, no Ask button — `base_template` sets `held = false` and the
    // template renders no `[data-verb="ask"]`, so this door used to answer 200
    // with the question sitting in a box that has no way to send it and no
    // word about why. Reachable: `_gaps.html` renders "ask again" from
    // recorded searches, which outlive a purge of what they searched.
    //
    // Sent to the plain page rather than 404'd, because the question survives
    // the redirect and search is the one verb an empty base can still honour.
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

    /// The same, over a base holding one source.
    ///
    /// The ask door only opens where something is held: with nothing stored
    /// the workspace renders no Ask verb, so the door redirects rather than
    /// answer with a question in a box that has no way to send it.
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

    /// Two of the three verbs cannot work on a base with nothing in it, and a
    /// list with nothing in it has nothing to move through. A disabled button
    /// is a promise the page cannot keep and seven shortcuts are a wall; both
    /// are absent until there is something for them to act on.
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
            empty.contains("Paste anything worth keeping"),
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

    /// An OIDC user never sees the login card, so the tagline and the privacy
    /// boundary have to be said where the eye already is — under the box, not
    /// on a settings page nobody opens before pasting.
    #[tokio::test]
    async fn an_empty_base_says_what_this_is_and_whose_it_is() {
        let html = workspace("/ui").await;
        assert!(
            html.contains("finds it again by meaning"),
            "what the application does, in one clause"
        );
        assert!(
            html.contains("nobody else can search this base"),
            "and the boundary, which is what a person wants before pasting \
             their own notes onto someone else's server"
        );
        assert!(
            !html.contains("Search to see an artifact here"),
            "an instruction that cannot be followed on an empty base"
        );
        assert!(
            html.contains("kept exactly as you wrote it"),
            "the pane says what will happen to the first thing pasted"
        );
    }

    /// An ordinary capture answers with nothing to render.
    ///
    /// It used to answer with three blocks: a receipt line, a sentence saying
    /// what "segmenting 3/7" counts towards, and `_queue.html` polling itself.
    /// They landed in `#capture-result`, a child of `.region-bar` — and on a
    /// phone that bar is fixed to the bottom of the viewport with no height
    /// bound, so it grew past the top of the screen and took the textarea with
    /// it. Nothing scrolls a fixed element back.
    ///
    /// The acknowledgment is the "last kept" line in `_idle_foot.html`, which
    /// the same `submit` that refreshes the rail swaps out of band. It names
    /// the capture and links to it; a second line directly above the box was
    /// never carrying anything it did not.
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

    /// The two things that are not receipts still answer.
    ///
    /// A duplicate and a parked near-duplicate are states the operator cannot
    /// learn any other way — a phone has no other surface that would say it —
    /// and they are the reason this fragment still exists at all.
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
            again.contains("nothing was processed a second time"),
            "the second paste of the same text is a duplicate: {again}"
        );
        // And it still never promises reading it is not doing.
        assert!(!again.contains("searchable once it settles"));
        assert!(!again.contains(r#"hx-get="/ui/queue""#));
    }

    /// The whole of the language path, from the browser to the corpus.
    ///
    /// The setting is stamped at the door and never read at the job, because a
    /// job holds a `Core` cached per tenant in a fixed-size LRU that knows no
    /// subject: a setting that only took effect on eviction would be a setting
    /// nobody could trust. So what the reading is instructed in is on the
    /// corpus, and this is where it gets there.
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

        // On automatic — which is every account that has never opened Settings
        // — the browser decides, and a regional tag is still its language.
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

        // The account setting overrides the browser: somebody reading a German
        // page in an English browser still wants their base in German.
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

        // And a language outside the ten is English rather than a refusal.
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

    /// The page is rendered once. Nothing re-renders it after a capture — the
    /// receipt swaps into the pane and the rail refreshes — so the paste that
    /// ended an empty base left the whole workspace still arranged for one.
    /// `/ui/held` is what app.js swaps over that.
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

        // Nothing held, nothing to swap: an Ask verb over a base that cannot
        // answer is the promise this whole state exists to avoid.
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
            r#"id="keyhint-slot""#,
            r#"id="box-hint""#,
            r#"id="pane-idle""#,
        ] {
            assert!(held.contains(region), "{region} is not in the swap: {held}");
        }
        assert_eq!(
            held.matches(r#"hx-swap-oob="true""#).count(),
            4,
            "every region names its own target, or it lands nowhere: {held}"
        );
        assert!(
            held.contains(r#"data-verb="ask""#) && held.contains(r#"class="keyhint""#),
            "the two controls an empty base withholds: {held}"
        );
        assert!(
            held.contains("Search to see an artifact here"),
            "and the hints for the state it is now in: {held}"
        );
    }

    /// A question in a box with no way to send it. `held = false` renders no
    /// Ask verb, so this door used to answer 200 with the question prefilled,
    /// no button, and no word about why — reachable from a gap's "ask again",
    /// which outlives a purge of what it was asked about.
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

    /// A button that is dead on arrival and says nothing about why reads as a
    /// broken application rather than as a control waiting for its input.
    #[tokio::test]
    async fn a_disabled_verb_says_what_it_is_waiting_for() {
        let html = workspace("/ui").await;
        assert!(
            html.contains(r#"title="Type or attach something first""#),
            "the disabled verb names what it wants: {html}"
        );
        // The types are on the label's `title` and on the input's `aria-label`,
        // and nowhere else. The third copy — a line of prose under the verb
        // row — was one of four muted sentences stacked under the box, and it
        // was the one saying least: the picker a phone opens names what it
        // accepts itself.
        assert!(
            html.contains(r#"aria-label="Attach a file"#),
            "and Attach still names its types to a screen reader: {html}"
        );
        assert!(
            !html.contains("attach-types"),
            "but not a third time in prose: {html}"
        );
    }

    /// Results appear beside a person who pressed nothing. The old hint
    /// explained how to phrase a query, which is true and is not the thing
    /// they do not know.
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
        // Two examples and one clause, where four muted prose lines used to
        // stack. The claim that typing searches is taught by a phrasing you
        // can press rather than asserted at somebody — and the phrasings are
        // the classifier's own prototypes, so the page cannot come to teach a
        // wording the base has stopped recognising.
        assert!(
            html.contains("remind me"),
            "the one thing a first-time user cannot deduce from the page: {html}"
        );
        assert!(html.contains("chip-example"), "and it is pressable: {html}");
        assert!(
            html.contains("a sentence finds more than keywords do"),
            "with the one true thing the old hint said kept: {html}"
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
            event_id: None,
            verdict_bar: String::new(),
        })
        .unwrap()
    }

    /// The rail's why-line is reachable only if the flag survives three
    /// filters: the page's own query string, the hidden field, and the
    /// `hx-params` allowlist. It shipped past the first two and died at the
    /// third, so the line no operator could turn on was rendered by nothing
    /// but its unit tests.
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

    /// The refining pass is a second request per settled query, and app.js
    /// decides whether to fire it by reading the form. A box with no search
    /// reranker must not advertise one: every pause would buy a second search
    /// that answers in the same order, and the rail would claim "refined" over
    /// an order nothing confirmed.
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
            html.contains(r#"hx-params="q,category,rerank,explain,fold,tz""#),
            "hx-params is the allowlist for what rides a search GET; without \
             `rerank` on it the refining pass's own flag is filtered off the \
             wire and the server only ever runs the fast path — and `explain` \
             was missing from it for exactly as long as the rail's why-line \
             was unreachable"
        );
    }

    /// The refining pass, pinned at the seams that keep it honest: it arms
    /// only off the form's `data-rerank`, it fires `rerank=true` rather than
    /// re-running the fast search, and it never schedules itself off its own
    /// swap — which is the loop that would turn one settled query into a
    /// rerank call every half second forever.
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

    /// Three destinations, because there are three places. Capture and Ask
    /// were destinations while they were pages; they are verbs on the box now,
    /// and the box is on this screen.
    #[tokio::test]
    async fn the_tab_bar_points_at_the_three_places_there_are() {
        let html = workspace("/ui").await;
        let bar = html
            .split_once(r#"<nav class="tabbar""#)
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

        // The same rule in the row above it, which is the same three places
        // seen on a wider screen.
        let top = html
            .split_once(r#"<nav class="top""#)
            .expect("the top row is there")
            .1;
        let top = &top[..top.find("</nav>").expect("the top row ends")];
        assert!(!top.contains(r#"href="/ui/capture""#), "{top}");
        assert!(!top.contains(r#"href="/ui/ask""#), "{top}");
    }

    /// A door that fails silently is worse than one that fails.
    ///
    /// htmx swaps nothing on an error of any kind, so before this the rail
    /// stayed exactly as empty on a failed search as it was before the
    /// keystroke — and the only reading left was that the base holds nothing.
    /// 401 was handled because a dead session is the case that was noticed;
    /// every other status was not.
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

        // A transport error fires no responseError at all — there is no
        // response — and it is the case a base behind a dropped connection
        // produces.
        assert!(
            js.contains("addEventListener('htmx:sendError'"),
            "a request that never arrived says nothing"
        );

        // The server's own reason, verbatim. A generic failure line would hide
        // what actually goes wrong here.
        let f = js
            .split_once("function failedSwap(")
            .expect("the driver has no failedSwap()")
            .1;
        assert!(
            f.contains("JSON.parse(xhr.responseText).error"),
            "the reason the server gave is thrown away"
        );
        assert!(
            f.contains("textContent"),
            "an error string is the one payload here that went through no renderer"
        );
    }

    /// A result click must never swap away the ask and capture targets: they
    /// exist nowhere but the workspace's first paint, so a fragment that
    /// replaced the whole pane left Ask streaming into detached nodes and
    /// Capture with nowhere to answer, until a full reload.
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

    /// The provenance of a kept answer belongs to the text that was kept, and
    /// the box does not close behind a capture. Left standing, the hidden
    /// `from_ask` was read again by the next press: something the operator
    /// typed themselves went into the base as `origin = "ask"`, carrying a
    /// question and citations it had nothing to do with — a corpus asserting
    /// model authorship for hand-written words.
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

    /// Emptying the box is not a statement that the answer is over. The box
    /// keeps the question after an Ask on purpose, so select-all and Delete
    /// went through `showIdle`, which hides `#pane` and `#rail` — taking the
    /// streaming answer and the way back to the results with them, with
    /// nothing to bring them back until another keystroke or a reload.
    #[test]
    fn emptying_the_box_does_not_take_away_an_answer_being_written() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let body = &js[js.find("function showIdle()").expect("the idle transition")..];
        let body = &body[..body.find("function show(").expect("it ends")];
        assert!(
            body.contains("if (paneIsBusy()) return;"),
            "the idle transition asks what the pane is doing first: {body}"
        );
        assert!(
            js.contains("function paneIsBusy()")
                && js.contains("classList.contains('asking')")
                && js.contains("getElementById('ask-result')"),
            "and an ask in flight or an answer standing both count"
        );
    }

    /// A press was once two captures — a staged file and the text above it —
    /// and both answered in `#capture-result`, so whichever request landed
    /// last wiped the other's line. The text above a file is that file's note
    /// now, so one press is one capture and writes one receipt. What is
    /// guarded here is that it stayed one: a second sender reaching this node
    /// is the bug the appending swap was added for.
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

    /// An answer is not swapped into `#pane-content`, so it gained none of the
    /// room a result click does: wide, the rail kept the 40rem it is allowed
    /// while nothing is open and the answer streamed into the remainder;
    /// narrow, the rail comes first in the DOM and every excerpt sat above the
    /// answer.
    ///
    /// Its own class rather than `has-selection`, which narrow reads as "hide
    /// the rail" — and the rail is where this answer's own `[n]` links point.
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

        let css = include_str!("../../assets/css/20-layout.css");
        let idle = css
            .split_once(":not(.pane-open):not(.answering)")
            .expect("the idle 40rem rail is not held while an answer is being written")
            .0;
        // Bounded to the widths that have two columns. The two chained
        // `:not()`s outrank the single class the one-up block sets its track
        // list with — chained rather than `:not(.pane-open, .answering)`,
        // which would count one class and not two — and specificity does not
        // care that the two rules answer different widths: unbounded, this
        // two-column rule won on a narrow screen too, and left the pane beside
        // a 40rem rail twelve pixels wide.
        assert!(
            idle.rsplit_once("@container")
                .is_some_and(|(_, open)| open.trim_start().starts_with("shell (width > 60rem)")),
            "the idle rail's track list applies where there is only one column"
        );
        assert!(
            css.contains(".regions.answering .region-rail { order: 1; }"),
            "and narrow puts the excerpts under the answer rather than over it"
        );
        let phone = include_str!("../../assets/css/50-phone.css");
        assert!(
            phone.contains(".regions.answering .region-rail { order: 1; }"),
            "including in the block that restates the narrow rules for a phone"
        );
    }

    /// One class was answering two questions. Narrow asks "should the rail
    /// still be on screen", which a fresh list answers yes to — the results
    /// have changed under whatever is open. Wide asks "does the pane hold
    /// something that needs its width", which a fresh list does not change at
    /// all. `has-selection` was both, so a capture — which empties the box, and
    /// an empty box comes back as the idle rail through the same `#results`
    /// swap — handed the rail 40rem while an artifact was open beside it, and
    /// the artifact finished in a 24rem strip setting one word per line.
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

        let css = include_str!("../../assets/css/20-layout.css");
        assert!(
            css.contains(":not(.pane-open):not(.answering)"),
            "the wide rail keys on what the pane holds, not on the narrow rule"
        );
        assert!(
            !css.contains(":not(.has-selection)"),
            "nothing about width is still asking the narrow question"
        );
    }

    /// The split decides its columns from the window, which is the right
    /// question only where the window *is* the pane. Inside the focus pane its
    /// width is whatever the rail leaves, and no width of window says what that
    /// is — so a narrowed pane kept two columns of eleven rem and set German
    /// compounds one word to the line.
    #[test]
    fn the_split_stacks_when_the_pane_it_landed_in_is_too_narrow_for_two() {
        let css = include_str!("../../assets/css/40-workspace.css");
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
        // Two stacked halves that each scroll would be the lockstep pair with
        // the one thing that makes it a pair removed: scrolling either would
        // move the other off screen rather than alongside it.
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

    /// The command overlay was a second text surface for the problem the first
    /// one now solves. `/` focuses the real box instead.
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

    /// One act in flight. Pressing Ask makes the box read-only, and that is
    /// what disables search-while-type: nothing can be typed or pasted into a
    /// read-only box, so it fires no `input` and the form's `hx-trigger` goes
    /// quiet with no second mechanism and no flag to keep in sync.
    ///
    /// Read-only and not `disabled`, which is what it was. `disabled` also
    /// makes text unselectable and hands focus back to the body, so the
    /// question being answered could not be copied out of the box holding it —
    /// while `.box[readonly]` in the CSS goes to the trouble of keeping that
    /// text legible, on the grounds that a box someone is waiting on is still
    /// a box someone is reading.
    ///
    /// The re-enable belongs in `stop()` and nowhere else. Every exit already
    /// runs through it — the answer completing, the Stop button, and the
    /// transport error that `fail()` funnels into it. Put it on the `done`
    /// handler instead and a dropped connection leaves the box read-only
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
            busy.contains("box.readOnly"),
            "setBusy does not silence the box, so typing still searches: {busy}"
        );
        assert!(
            !busy.contains("box.disabled"),
            "the box is disabled again, and the question cannot be selected \
             out of it while it is being answered: {busy}"
        );

        // The other half: nothing else may re-enable it. A second caller is a
        // second thing to keep in step with the three exits.
        assert_eq!(
            js.matches("setBusy(false)").count(),
            1,
            "setBusy(false) is called from more than one place"
        );
    }

    /// Both verbs, without a pointer. `/` reaches the box and nothing reached
    /// back out of it: a hand that never left the keys had to find the mouse
    /// to press the button it had just finished typing into.
    ///
    /// This is not the box inferring a verb from a newline, which is the rule
    /// it is built on — Enter puts in a line break here and always will. The
    /// chord is a second deliberate gesture, the same act as the button.
    ///
    /// Routed through the button and not the handler behind it, so everything
    /// the button already knows stays true: an empty box or an ask in flight
    /// leaves it disabled, and a disabled verb does nothing here either. Where
    /// the install has no Ask there is no button, and the unshifted chord is
    /// simply not a key.
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

    /// The box is `border-box` and `scrollHeight` is not: it measures the
    /// content and the padding, never the 1px border on each edge. Set the
    /// height straight from it and the box ends two pixels shorter than the
    /// text it was measured from, every time — which with `overflow-y: auto`
    /// is a live scrollbar on a box that has nothing to scroll. The cap is the
    /// same mistake: ten lines of text *plus* the padding they sit in, or the
    /// tenth line is the one the padding eats.
    ///
    /// And the height is a measurement of wrapped text, so it is wrong the
    /// moment the box changes width. Bound to `input` alone it was only ever
    /// correct at the width the last keystroke was typed at.
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
        let html = workspace_held("/ui/ask").await;
        assert!(html.contains(r#"id="ask-stop""#), "{html}");
    }

    #[tokio::test]
    async fn an_ask_does_not_open_with_the_models_reasoning_showing() {
        // The deployment streamed the chain of thought into the page for fifty
        // seconds, restating the prompt's own constraints verbatim — "Answer
        // *only* using the provided knowledge-base excerpts" — above the empty
        // space where the answer was going to be.
        let html = workspace_held("/ui/ask").await;
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

    #[tokio::test]
    async fn an_unforced_note_is_a_plain_capture_on_the_receipt() {
        // The cue table retired with the classifier: the web box files
        // nothing at the door, and an entry chip appears only once the
        // judged synthesis call has said so.
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
        // The instant, so app.js can rebuild the path in the viewer's zone. The
        // date in the href is UTC and the day page builds its window locally,
        // so east of Greenwich every capture between local midnight and the
        // offset linked to a page reading "Nothing on this day."
        assert!(html.contains("data-day-at="), "{html}");
    }

    /// The two halves the server cannot check for itself: the zone the box
    /// posts, and the day a stamp links to.
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
        // A capture empties the box, and an empty box hides `#pane`. With the
        // receipt inside the pane, "Captured — view source" and the undo beside
        // a note filed as today's entry were swapped in and hidden in the same
        // turn: the undo for a wrongly-cued entry was unreachable without a
        // reload.
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
        // The flagship path. Without it *remind me tomorrow at 9* was echoed
        // under the box as 09:00 in the viewer's zone and stored in the
        // server's, and the band underneath then said something else.
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

        // A zone the table does not know is dropped rather than stored: nobody
        // typed this field, and a 400 would lose the text of the capture.
        post(app, cookie, "text=Another+note+entirely&tz=Europe%2FBerlim").await;
        let stored = core.store.recent_captures(1).await.unwrap();
        let cid = stored[0].0.clone();
        assert!(core.store.get_corpus(&cid).await.unwrap().metadata["tz"].is_null());
    }
}
