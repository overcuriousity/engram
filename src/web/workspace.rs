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
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::auth::Identity;
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
