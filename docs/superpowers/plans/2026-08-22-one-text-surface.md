# One Text Surface for the Web UI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fold capture, search and ask into one workspace route at `/ui`, move
management and measurement to a new Insights page, and take the V1 visual pass.

**Architecture:** Every htmx fragment endpoint keeps its path and its response
byte-for-byte; only the page that hosts them changes. The three page handlers in
`src/web/ui.rs` collapse into one handler in a new `src/web/workspace.rs`, and
the maintenance surfaces move to `src/web/insights.rs`. The rail and the focus
column each show what the current act produced — typing gives results, Ask
replaces them with the excerpts the answer was written from.

**Tech Stack:** Rust, axum, askama templates, htmx, hand-written CSS in
`assets/css/` (numeric layer prefixes; the cascade depends on the order), plain
ES5-style JavaScript in `assets/app.js` (no build step, no framework).

**Spec:** `docs/superpowers/specs/2026-08-22-one-text-surface-design.md`

## Global Constraints

Copied from spec §11. Every task's requirements implicitly include these.

- **No reachable route stops answering.** `/ui/search`, `/ui/ask`, `/ui/capture`
  and `/ui/ops` all keep responding — as deep links or as redirects, never a 404.
- **No fragment endpoint changes shape.** `/ui/search/results`,
  `/ui/ask/{id}/stream`, `/ui/ask/{id}/verdict`, `/ui/ask/{id}/carried`,
  `/ui/ask/{id}/keep`, `/ui/context`, `/ui/context/seen`, `/ui/queue`,
  `/ui/artifacts/{id}` answer exactly as they do now. The browser extension,
  `/api/v1` and `/mcp` are untouched.
- **`prefill_text`, `prefill_ask`, `prefill_question` keep working** on
  `/ui/capture`. The extension and *keep this answer* post into it.
- **`core::sitting` keeps every writer and every reader it has.** Only its two
  UI surfaces go. Do not touch `src/core/sitting.rs`, `src/core/search.rs:995`,
  or `src/web/ui.rs:3799`.
- **The phone rules in `assets/css/50-phone.css` keep their reasons.** The fixed
  bar at the thumb, 16px inputs (iOS zoom), 44px targets, safe-area insets,
  `has-selection` rail hiding, and the back link at the container width rather
  than the phone width.
- **The retrieval is not touched.** No change to ranking, the cliff, priming,
  activation, gaps, pursuits, or what any of them record. The `Door` an event is
  recorded against must keep meaning what it means.
- **Both themes stay complete.** No colour gets its only definition inside a
  media query or a `[data-theme]` block.
- **The page works without JavaScript to the extent it does today**, and no
  further.

**Every task ends green.** `cargo fmt --all --check`, `cargo clippy
--all-targets --locked -- -D warnings` and `cargo test --locked` must all pass
before the commit. CI runs exactly these.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/web/workspace.rs` (new) | The one page: its handler, its template struct, and the search/ask/capture fragment handlers it hosts. |
| `src/web/insights.rs` (new) | The Insights page: maintenance sections (Task 1–2) and the measures (Task 11). |
| `src/web/templates/workspace.html` (new) | The workspace markup — box, verb row, three regions. |
| `src/web/templates/insights.html` (new) | The Insights markup. |
| `assets/css/40-workspace.css` (new) | One stylesheet for the one page, replacing three. |
| `src/web/ui.rs` (shrinks) | Everything else: artifacts, corpora, settings, tokens, the shared render helpers. |
| `src/web/templates/search.html`, `ask.html`, `capture.html`, `_sitting.html` | Deleted. |
| `assets/css/40-search.css`, `41-capture.css`, `45-ask.css` | Deleted. |

Shared fragment templates (`_results.html`, `_answer.html`, `_ask_rail.html`,
`_ask_verdict.html`, `_captured.html`, `_context.html`, `_queue.html`,
`_decide.html`, `_gaps.html`) are **not** rewritten. They move between page
hosts unchanged.

---

## Task 1: The Insights page, and Housekeeping moves into it

**Files:**
- Create: `src/web/insights.rs`
- Create: `src/web/templates/insights.html`
- Modify: `src/web/mod.rs` (register the router)
- Modify: `src/web/ui.rs` (move `ops` handler out; leave a redirect)
- Modify: `src/web/templates/layout.html` (nav: `Insights` replaces the quiet Housekeeping link)
- Modify: `src/web/templates/capture.html:130-133` (the quiet-links paragraph)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn routes() -> axum::Router<crate::web::state::AppState>` in
  `src/web/insights.rs`, mounted by `src/web/mod.rs`. Serves `GET /ui/insights`.
  `GET /ui/ops` becomes a 303 redirect to `/ui/insights`. Every
  `POST /ui/ops/...` action endpoint keeps its exact path.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the bottom of `src/web/ui.rs` (it already has
`app_for`, `get` and `app_with_cookie` in scope):

```rust
#[tokio::test]
async fn housekeeping_moved_to_insights_and_the_old_door_still_opens() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;

    // The new door renders the page.
    let html = get(&app, "/ui/insights", &cookie).await;
    assert!(html.contains("Housekeeping"), "the maintenance section is there");

    // The old door is not a 404. A bookmark, a link in a runbook and the
    // quiet link at the bottom of Capture all still point at it.
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
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --locked housekeeping_moved_to_insights`
Expected: FAIL — `GET /ui/insights` 404s, so `get()`'s `assert_eq!(status, OK)` trips.

- [ ] **Step 3: Create `src/web/insights.rs`**

Move the `ops` handler and its template struct out of `src/web/ui.rs` verbatim —
rename the struct `OpsTemplate` to `InsightsTemplate` and point it at the new
template path. Keep every `#[doc]` comment on the fields; they carry the
reasoning and this task is a move, not a rewrite.

```rust
//! Insights: what is true about this installation, and what needs a person.
//!
//! Two halves. The maintenance half is Housekeeping relocated — hidden, stale
//! and retrying artifacts, the merge undo log, tokens, sources — plus the three
//! surfaces that used to sit on Capture. The measures half reads aggregates
//! over tables that already exist.
//!
//! `/ui/ops` redirects here rather than 404ing: it is in bookmarks, in the
//! quiet link at the bottom of the workspace, and in at least one runbook.

use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::web::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/insights", get(page))
        .route("/ui/ops", get(|| async { Redirect::to("/ui/insights") }))
}
```

Then move the existing `ops` handler body in as `page`, and move every
`POST /ui/ops/...` route registration from `ui.rs` into this `routes()`
unchanged — same paths, same handlers.

- [ ] **Step 4: Create `src/web/templates/insights.html`**

Copy `src/web/templates/ops.html` to `insights.html` verbatim, then wrap its
existing content in a named section so the next task has somewhere to add to:

```html
{% extends "layout.html" %}
{% block title %}Insights — engram{% endblock %}
{% block regions %}regions-table{% endblock %}
{% block content %}
<h2>Housekeeping</h2>
{# Everything ops.html held, unchanged. This is a relocation: the reasons
   recorded against each of these sections are still the reasons. #}
...
{% endblock %}
```

Delete `src/web/templates/ops.html`.

- [ ] **Step 5: Register the router**

In `src/web/mod.rs`, beside the existing `.merge(...)` calls:

```rust
.merge(crate::web::insights::routes())
```

and add `pub mod insights;` to the module list.

- [ ] **Step 6: Point the nav at it**

In `src/web/templates/layout.html`, in `nav.top` after the Judge link:

```html
<a href="/ui/insights">Insights</a>
```

In `src/web/templates/capture.html`, change the quiet link's text and target:

```html
<a class="quiet-link" href="/ui/insights">Insights</a>
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings && cargo fmt --all --check`
Expected: PASS. Existing `ops` tests that hit `/ui/ops` for content now need
`/ui/insights`; update their URI and nothing else.

- [ ] **Step 8: Commit**

```bash
git add src/web/insights.rs src/web/templates/insights.html src/web/mod.rs \
        src/web/ui.rs src/web/templates/layout.html src/web/templates/capture.html
git rm src/web/templates/ops.html
git commit -m "feat(web): Housekeeping becomes Insights, and /ui/ops points at it"
```

---

## Task 2: Recent, Needs you and the gaps leave the workspace

**Files:**
- Modify: `src/web/insights.rs` (template struct gains the three sections)
- Modify: `src/web/templates/insights.html`
- Modify: `src/web/ui.rs` (`capture_page` sheds `pairs`, `more_pairs`, `gaps`, `loose`)
- Modify: `src/web/templates/capture.html` (drop the three includes and the aside)

**Interfaces:**
- Consumes: `insights::routes()` and `InsightsTemplate` from Task 1.
- Produces: `InsightsTemplate` gains `pairs: Vec<PairCluster>`, `more_pairs: i64`,
  `gaps: Vec<GapGroup>`, `loose: Vec<GapMember>`. Those four types stay declared
  in `src/web/ui.rs` and become `pub(crate)`. The `/ui/queue` endpoint is
  unchanged and is now loaded by `insights.html`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_three_maintenance_surfaces_are_on_insights_and_not_on_capture() {
    let (app, cookie, _core) = app_with_a_waiting_pair().await;

    let insights = get(&app, "/ui/insights", &cookie).await;
    assert!(insights.contains("Needs you"), "pairs are on Insights");
    assert!(insights.contains("Recent"), "the capture queue is on Insights");
    assert!(insights.contains("/ui/queue"), "and it loads the same fragment");

    let capture = get(&app, "/ui/capture", &cookie).await;
    assert!(!capture.contains("Needs you"), "pairs left the capture page");
    assert!(!capture.contains("/ui/queue"), "so did the queue");
}
```

Write `app_with_a_waiting_pair()` beside it, modelled on the existing
near-duplicate tests in `src/web/ui.rs` (search for `pair_rows` to find one):

```rust
/// A core holding one near-duplicate pair, so "Needs you" has something to say.
async fn app_with_a_waiting_pair() -> (axum::Router, String, crate::core::Core) {
    let core = crate::core::test_support::test_core().await;
    core.ingest_capture(crate::core::ingest::Capture::new(
        "The reindex job holds a file descriptor on the old mount.",
        ORIGIN_WEB,
    ))
    .await
    .unwrap();
    core.ingest_capture(crate::core::ingest::Capture::new(
        "The reindex job holds an fd on the old mount.",
        ORIGIN_WEB,
    ))
    .await
    .unwrap();
    let handle = core.clone();
    let (app, cookie) = app_with_cookie(core).await;
    (app, cookie, handle)
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --locked the_three_maintenance_surfaces`
Expected: FAIL — `Needs you` is on the capture page, not on Insights.

- [ ] **Step 3: Move the fields onto `InsightsTemplate`**

Cut the four fields, with their doc comments, from `CaptureTemplate` in
`src/web/ui.rs:478` onto `InsightsTemplate` in `src/web/insights.rs`. Cut the
loading code — the `pair_rows`/`group_pairs` call and the `gap_rows` block — out
of `capture_page` (`src/web/ui.rs:1097`) and into the Insights `page` handler
verbatim. Mark `PairCluster`, `GapGroup`, `GapMember` and the `gap_member`
helper `pub(crate)` so `insights.rs` can name them.

- [ ] **Step 4: Move the markup**

In `insights.html`, above the Housekeeping section:

```html
{% if !pairs.is_empty() %}
<h2>Needs you</h2>
{% include "_decide.html" %}
{% if more_pairs > 0 %}
<p class="muted">{{ more_pairs }} more waiting — the next comes up as you clear these.</p>
{% endif %}
{% endif %}

{% include "_gaps.html" %}

<h2>Recent</h2>
{# Loaded rather than rendered inline, so the first paint and every refresh
   afterwards go through exactly the same fragment. #}
<div hx-get="/ui/queue" hx-trigger="load" hx-swap="outerHTML"></div>
```

Delete the corresponding blocks from `capture.html`, including the whole
`<div class="region-aside">` and its quiet links, and change its
`{% block regions %}` to `regions-focus`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS. Existing tests asserting gaps or pairs on `/ui/capture` change
their URI to `/ui/insights`.

- [ ] **Step 6: Commit**

```bash
git add -A src/web
git commit -m "refactor(web): Recent, Needs you and the gaps move to Insights"
```

---

## Task 3: The workspace, search half

**Files:**
- Create: `src/web/workspace.rs`
- Create: `src/web/templates/workspace.html`
- Modify: `src/web/mod.rs`
- Modify: `src/web/ui.rs` (delete `search_page`, `SearchTemplate`, `sitting_rail`, `SittingItem`, `SITTING_RAIL`; move `search_results` and `UiSearchParams`)
- Delete: `src/web/templates/search.html`, `src/web/templates/_sitting.html`

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces: `pub fn routes() -> axum::Router<AppState>` in
  `src/web/workspace.rs`, serving `GET /ui`, `GET /ui/search`, and
  `GET /ui/search/results`. `WorkspaceTemplate` with the fields listed in Step 3
  — later tasks add to it, none rename its fields. `data-open-with` on the
  workspace root is `""` here; Tasks 4 and 5 give it `"capture"` and `"ask"`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_workspace_is_one_page_carrying_the_box_and_the_three_regions() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;

    for uri in ["/ui", "/ui/search"] {
        let html = get(&app, uri, &cookie).await;
        assert!(html.contains("regions-rail-focus-source"), "{uri}: the grid");
        assert!(html.contains("name=\"q\""), "{uri}: the box");
        assert!(
            html.contains("hx-get=\"/ui/search/results\""),
            "{uri}: typing still asks the same endpoint"
        );
    }

    // A deep link restores the box and asks for its results on load.
    let html = get(&app, "/ui/search?q=volume+move", &cookie).await;
    assert!(html.contains("volume move"), "the box comes back filled");
    assert!(html.contains("load"), "and the results are fetched without a keystroke");
}

#[tokio::test]
async fn the_read_just_now_list_is_gone_but_the_sitting_is_not() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
    let html = get(&app, "/ui", &cookie).await;
    assert!(!html.contains("Read just now"), "the list is retired");

    // The mechanism it read from is not. `sittings.touched()` still runs on
    // every artifact open, because Ask's carried citations depend on it.
    let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
    let _ = js; // the Rust side is asserted by the ask tests; this is the guard
    assert!(
        std::fs::read_to_string("src/web/ui.rs")
            .unwrap()
            .contains("st.core.sittings.touched("),
        "the sitting is still written on every artifact open"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --locked the_workspace_is_one_page the_read_just_now_list`
Expected: FAIL — `GET /ui` currently redirects to `/ui/search`, and
`Read just now` is still rendered.

- [ ] **Step 3: Create `src/web/workspace.rs`**

```rust
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
//! *keep this answer* flow working.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::web::state::AppState;
use crate::web::ui::{ensure_facet, HtmlTemplate, FACET_LIMIT};
use crate::web::{Identity, Result};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui", get(page))
        .route("/ui/search", get(page))
        .route("/ui/search/results", get(crate::web::ui::search_results))
}

#[derive(Template)]
#[template(path = "workspace.html")]
struct WorkspaceTemplate {
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`. The `Ask`
    /// button is absent where it is false — the door is simply not there.
    ask_enabled: bool,
    /// The box's contents on arrival: a deep link's `?q=`, or an answer kept
    /// from an ask. One field, because there is one box.
    q: String,
    /// What this collection can be narrowed by, as chips in the verb row.
    facets: crate::vector::Facets,
    /// The chip a deep link arrived with, so the row comes back selected.
    category: String,
    /// Whether the area under the box exists at all. See `Core::recommends`.
    recommend: bool,
    /// What app.js should do on first paint: `""`, `"ask"` or `"capture"`.
    /// Search needs no value — typing already covers it. Rendered into
    /// `data-open-with`, so the decision is made here and the template holds
    /// no logic.
    open_with: &'static str,
}

async fn page(
    State(st): State<AppState>,
    _id: Identity,
    Query(p): Query<crate::web::ui::UiSearchParams>,
) -> Result<Response> {
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
    let category = p.category.unwrap_or_default();
    // A deep link can name a value outside the top `FACET_LIMIT`, or one
    // nothing carries. The rail is narrowed by it either way, so the row has
    // to show it — otherwise the page reads as unfiltered while it is not,
    // and there is no chip to click to get back out.
    ensure_facet(&mut facets.categories, &category);
    Ok(HtmlTemplate(WorkspaceTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        q: p.q,
        facets,
        category,
        recommend: st.core.recommends(),
        open_with: "",
    })
    .into_response())
}
```

Mark `ensure_facet`, `FACET_LIMIT`, `HtmlTemplate`, `search_results` and
`UiSearchParams` `pub(crate)` in `src/web/ui.rs`. Delete `search_page`,
`SearchTemplate`, `sitting_rail`, `SittingItem` and `SITTING_RAIL` outright, and
remove `/ui`, `/ui/search` and `/ui/search/results` from `ui.rs`'s router.

- [ ] **Step 4: Create `src/web/templates/workspace.html`**

Start from `search.html`. Move its comment blocks with the markup they explain —
they record why the trigger is what it is, and those reasons have not changed.

```html
{% extends "layout.html" %}
{% block title %}engram{% endblock %}
{% block regions %}regions-rail-focus-source{% endblock %}
{% block content %}
<div class="region-bar" data-workspace data-open-with="{{ open_with }}">
{# One form around the box and the chips, so a chip carries the query with it
   and the query carries the chips. `change` fires for the chips, `keyup` only
   for the box: a radio does not need a debounce and the box does. `change` is
   scoped to the chip row because it bubbles, and the box fires its own on blur
   — which is fired by the very click that opens a result, so an unscoped
   `change` replaced the list out from under the pointer.

   120ms rather than 250. A quarter second is a pause you can feel, and the
   query path is one embedding and one vector search either way.

   `hx-sync="this:replace"` because of that halving: two requests from the same
   box are only ordered by luck, and the slower one lands last. Replace aborts
   the request in flight, so the answer shown is the answer to the last thing
   typed. #}
<form id="box-form" hx-get="/ui/search/results" hx-target="#results" hx-swap="innerHTML"
      hx-sync="this:replace"
      hx-trigger="change from:#kind-chips,
                  keyup changed delay:120ms from:textarea[name=q],
                  submit{% if !q.is_empty() %},
                  load{% endif %}"
      hx-indicator="#search-spinner">
  {# A textarea from the first keystroke to the last. It grows to a ten-line
     cap and then scrolls; it never switches element type and it never infers a
     verb from a length or a newline. What decides which of the three happens
     is a button.

     No `autofocus`: on a touch screen it opens the keyboard over the results
     the page was opened to show. app.js focuses it where a pointer says there
     is a hardware keyboard already. #}
  <textarea class="box" name="q" rows="1"
            placeholder="Describe the situation, ask a question, or paste anything worth keeping…"
            aria-describedby="box-hint">{{ q }}</textarea>
  <div class="verbs">
    {% if ask_enabled %}
    <button class="btn btn-accent" type="button" data-verb="ask" disabled>Ask</button>
    {% endif %}
    <button class="btn" type="button" data-verb="capture" disabled>Capture</button>
    <button id="ask-stop" class="btn btn-ghost btn-sm" type="button" hidden>Stop</button>
    <span id="search-spinner" class="spinner">searching…</span>
    <span class="verbs-spacer"></span>
    {% if !facets.categories.is_empty() %}
    <div class="chips" id="kind-chips" role="group" aria-label="Kind">
      <label class="chip">
        <input type="radio" name="category" value="" {% if category.is_empty() %}checked{% endif %}>
        <span>All</span>
      </label>
      {% for f in facets.categories %}
      <label class="chip">
        <input type="radio" name="category" value="{{ f.value }}"
               {% if category == f.value %}checked{% endif %}>
        <span>{{ f.value }}</span>
      </label>
      {% endfor %}
    </div>
    {% endif %}
  </div>
  <p id="box-hint" class="muted hint">A sentence or a whole paragraph finds more
    than keywords do — paste what you are looking at.</p>
</form>

{# The offer is for the state "no intent expressed yet". app.js removes it on
   the first keystroke and it does not come back when the box is cleared: once
   there is an intent the offer is wrong, and reappearing because someone
   corrected a typo is flicker. `q.is_empty()` as well, because that state is a
   claim about the page and not about the switch — a deep link renders the box
   filled and app.js never sees a keystroke. #}
{% if recommend && q.is_empty() %}
<div id="context-offer" class="offer" hx-post="/ui/context" hx-trigger="load"
     hx-vals='js:{bundle: engramContext()}' hx-swap="outerHTML"></div>
{% endif %}

<p class="keyhint" hidden>
  <kbd>/</kbd> box <kbd>↑</kbd><kbd>↓</kbd> move <kbd>↵</kbd> open
  <kbd>s</kbd> hide source <kbd>r</kbd> reading
  <button type="button" class="btn btn-ghost btn-sm" data-dismiss-hint>Got it</button>
</p>
</div>

<div id="rail" class="region-rail rail">
  {# The rail's heading belongs to the act: `3 results` while typing,
     `Written from · n` after an Ask. Rendered by the fragment that fills it,
     so this element only reserves the place. #}
  <div id="rail-head"></div>
  <div id="results" role="listbox" aria-label="Results"></div>
</div>

<div id="pane" class="region-focus pane">
  <p class="muted">Search to see an artifact here, beside the lines it came from.</p>
</div>
{% endblock %}
```

- [ ] **Step 5: Register and delete**

`src/web/mod.rs`: add `pub mod workspace;` and `.merge(crate::web::workspace::routes())`.
Delete `src/web/templates/search.html` and `src/web/templates/_sitting.html`, and
remove the `{% include "_sitting.html" %}` line from `ask.html`.

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS. `askama` fails the build if a deleted template is still
included, so a missed `_sitting.html` include is a compile error, not a
surprise at runtime.

- [ ] **Step 7: Commit**

```bash
git add -A src/web
git commit -m "feat(web): one workspace at /ui, and the search half moves into it"
```

---

## Task 4: Capture folds into the box

**Files:**
- Modify: `src/web/workspace.rs` (template gains four fields; `/ui/capture` GET)
- Modify: `src/web/templates/workspace.html` (staged file, note, receipt target)
- Modify: `src/web/ui.rs` (delete `capture_page`, `CaptureTemplate`, `CapturePrefill`; move `capture_submit` and `CaptureForm`)
- Modify: `assets/app.js` (verb buttons, on-demand staging)
- Delete: `src/web/templates/capture.html`

**Interfaces:**
- Consumes: `WorkspaceTemplate` from Task 3.
- Produces: `WorkspaceTemplate` gains `vision_enabled: bool`, `eager: bool`,
  `prefill_ask: String`, `prefill_question: String`. `GET /ui/capture` renders
  the workspace with `open_with: "capture"` and `q` set from the prefill.
  `POST /ui/capture` is unchanged and moves to `workspace.rs`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_capture_door_fills_the_one_box_and_keeps_its_provenance() {
    let (app, cookie, core) = app_with_a_recorded_ask().await;
    let ask_id = core.store.recent_ask_id().await.unwrap();

    let html = get(&app, &format!("/ui/capture?from_ask={ask_id}"), &cookie).await;
    assert!(html.contains("data-open-with=\"capture\""), "the page opens capturing");
    assert!(html.contains(&ask_id), "the ask rides the form as provenance");
    assert!(html.contains("Kept from"), "and the question it answered is named");

    // The plain door is the same page with an empty box.
    let plain = get(&app, "/ui/capture", &cookie).await;
    assert!(plain.contains("data-workspace"), "still the workspace");
}

#[tokio::test]
async fn the_file_control_offers_images_only_when_vision_is_configured() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
    let html = get(&app, "/ui", &cookie).await;
    assert!(html.contains("image/*"), "the picker accepts images");
    assert!(html.contains("name=\"note\""), "the context field is there");

    let (app, cookie) =
        app_for(crate::core::test_support::test_core_without_vision().await).await;
    let html = get(&app, "/ui", &cookie).await;
    assert!(!html.contains("image/*"));
    assert!(html.contains("accept=\".txt,text/plain,.pdf,application/pdf\""));
}
```

Write `app_with_a_recorded_ask()` beside it, modelled on the existing
`post_ask` helper at `src/web/ui.rs:4874`.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --locked the_capture_door_fills_the_one_box the_file_control_offers`
Expected: FAIL — `/ui/capture` still renders the old page.

- [ ] **Step 3: Extend the handler**

In `src/web/workspace.rs`, add the four fields to `WorkspaceTemplate` with their
doc comments carried over from `CaptureTemplate`, add a second handler, and
register the two routes:

```rust
.route("/ui/capture", get(capture_door).post(capture_submit))
```

```rust
#[derive(serde::Deserialize)]
struct CapturePrefill {
    from_ask: Option<String>,
}

/// The capture door, which is the workspace with the box already filled.
///
/// The extension posts here and so does *keep this answer*, and neither knows
/// anything about the page having folded into one. A prefill that names an ask
/// nobody recorded is not an error worth a page for: the box is simply empty,
/// which is what an ordinary visit looks like.
async fn capture_door(
    State(st): State<AppState>,
    id: Identity,
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
    let mut t = base_template(&st, &id, q, String::new()).await?;
    t.open_with = "capture";
    t.prefill_ask = prefill_ask;
    t.prefill_question = prefill_question;
    Ok(HtmlTemplate(t).into_response())
}
```

Extract the body of `page` into
`async fn base_template(st: &AppState, id: &Identity, q: String, category: String)
-> Result<WorkspaceTemplate>` so both doors build the same page, and have `page`
call it. Move `capture_submit` and `CaptureForm` from `ui.rs` verbatim.

- [ ] **Step 4: Add the staging markup**

Into `workspace.html`, inside `.region-bar` under the verb row. The staged box
and the note render only once something is staged — that is most of what
`capture.html` spent its height on.

```html
{# One box for the file, in both of its states. Outside the search form on
   purpose: that form is a GET and the file goes multipart to the API endpoint.
   The label is what makes the box clickable, and on a phone that is what opens
   the camera. A file waits here until Capture is pressed, which is the same
   deliberate act pasted text has always needed. #}
<div id="staged" class="staged" hidden>
  <label class="muted" id="drop">
    {% if vision_enabled %}
    <input type="file" name="file" accept=".txt,text/plain,.pdf,application/pdf,image/*" hidden>
    {% else %}
    <input type="file" name="file" accept=".txt,text/plain,.pdf,application/pdf" hidden>
    {% endif %}
  </label>
  <img id="staged-thumb" alt="" hidden>
  <span id="staged-name" class="mono"></span>
  <input class="input" type="text" name="note" maxlength="2000"
         placeholder="Note for the file (optional) — what is it, why keep it?">
  <button class="btn btn-ghost btn-sm" type="button" id="staged-clear">Remove</button>
</div>
{% if !prefill_ask.is_empty() %}
<p class="muted">Kept from: &ldquo;{{ prefill_question }}&rdquo;</p>
<input type="hidden" name="from_ask" value="{{ prefill_ask }}" form="capture-form">
{% endif %}
<p id="size-hint" class="muted" hidden></p>
<div id="capture-result"></div>
```

Carry the segment-count hint script from `capture.html:135-155` across
unchanged, including its `EAGER` switch — the hint must not price a model call
the mode will never make.

- [ ] **Step 5: Wire the verbs in `assets/app.js`**

```js
  // The two verbs. Typing is the third and needs no button: it is the
  // `hx-trigger` on the form. Both are disabled while the box is empty,
  // because neither has anything to act on.
  function verbs() {
    var form = document.getElementById('box-form');
    if (!form) return;
    var box = form.querySelector('textarea[name="q"]');
    var buttons = form.querySelectorAll('[data-verb]');

    function sync() {
      var empty = !box.value.trim();
      for (var i = 0; i < buttons.length; i++) buttons[i].disabled = empty;
    }
    box.addEventListener('input', sync);
    sync();

    // Grows to a ten-line cap and then scrolls inside itself. Measured off
    // scrollHeight rather than a line count, because a wrapped paste is more
    // lines than it has newlines.
    var cap = 10;
    function grow() {
      box.style.height = 'auto';
      var line = parseFloat(getComputedStyle(box).lineHeight) || 20;
      box.style.height = Math.min(box.scrollHeight, line * cap) + 'px';
    }
    box.addEventListener('input', grow);
    grow();
  }
```

Call `verbs()` from the same init block that calls `askDriver()` at
`assets/app.js:736`. Bind the Capture button to submit the multipart request
the old page's button did — same endpoint, same fields.

- [ ] **Step 6: Delete the old page and run**

```bash
git rm src/web/templates/capture.html
cargo test --locked && cargo clippy --all-targets --locked -- -D warnings
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A src assets
git commit -m "feat(web): the Capture verb folds into the one box"
```

---

## Task 5: Ask folds into the box

**Files:**
- Modify: `src/web/workspace.rs` (`GET /ui/ask` door; ask fragment routes move here)
- Modify: `src/web/templates/workspace.html` (answer target in the focus column)
- Modify: `src/web/ui.rs` (delete `ask_page`, `AskTemplate`, `AskPrefill`; move the rest)
- Modify: `assets/app.js` (`askDriver` targets the workspace ids)
- Delete: `src/web/templates/ask.html`

**Interfaces:**
- Consumes: `base_template` and `WorkspaceTemplate` from Tasks 3–4.
- Produces: `GET /ui/ask?q=` renders the workspace with `open_with: "ask"`.
  `POST /ui/ask`, `GET /ui/ask/{id}/stream` and the three verdict/carried/keep
  routes move to `workspace.rs` with their paths and bodies unchanged. The
  element ids `ask-live`, `ask-reasoning`, `ask-reasoning-box`, `ask-progress`,
  `ask-result`, `ask-status`, `ask-stop` and `ask-verdict` are kept **exactly**;
  `tests/browser/ask_stream.js` targets every one of them.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_ask_door_fills_the_one_box_and_asks_on_arrival() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
    let html = get(&app, "/ui/ask?q=why+did+the+reindex+fail", &cookie).await;
    assert!(html.contains("data-open-with=\"ask\""), "the page opens asking");
    assert!(html.contains("why did the reindex fail"), "the box carries the question");
    // Every id the stream driver writes into has to survive the move.
    for id in ["ask-live", "ask-result", "ask-status", "ask-stop", "ask-progress"] {
        assert!(html.contains(id), "the driver's target {id} is on the page");
    }
}

#[tokio::test]
async fn the_ask_door_is_absent_without_a_model() {
    let core = crate::core::test_support::test_core_without_ask().await;
    let (app, cookie) = app_for(core).await;
    let html = get(&app, "/ui", &cookie).await;
    assert!(!html.contains("data-verb=\"ask\""), "no button where there is no model");

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
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "and the door is not there");
}
```

If `test_core_without_ask()` does not exist, write it beside
`test_core_without_vision()` in `src/core/test_support.rs`, returning a core
with no chat role configured so `Core::asks()` is false.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --locked the_ask_door_fills_the_one_box the_ask_door_is_absent`
Expected: FAIL — `/ui/ask` still renders `ask.html`.

- [ ] **Step 3: Add the door**

```rust
/// The ask door, which is the workspace with the question already in the box
/// and an answer requested on first paint. A gap's "ask again" links here.
///
/// No ask model, no ask door: the route is not there. See `Core::asks`.
async fn ask_door(
    State(st): State<AppState>,
    id: Identity,
    Query(p): Query<AskPrefill>,
) -> Result<Response> {
    if !st.core.asks() {
        return Err(crate::web::Error::NotFound);
    }
    let mut t = base_template(&st, &id, p.q, String::new()).await?;
    t.open_with = "ask";
    Ok(HtmlTemplate(t).into_response())
}
```

The last-query fallback from `src/web/ui.rs:3153` is **not** carried over: it
existed to carry the box's contents between two pages, and there is one box now.

Move `ask_submit`, `ask_stream`, `sse_event`, `rail_fragment`,
`answer_fragment`, `verdict_label`, `ask_verdict_bar`, `ask_verdict`,
`ask_carried`, `ask_keep`, `AskForm` and the ask template structs to
`workspace.rs` verbatim, and move their route registrations with them.

- [ ] **Step 4: Move the answer markup into the focus column**

Replace the placeholder `<p class="muted">` inside `#pane` in `workspace.html`
with the ask targets, carrying `ask.html`'s comments across:

```html
<div id="pane" class="region-focus pane">
  {# What the model said on the way to the answer. Behind a disclosure and
     closed: it is not the answer, nothing in it is cited, and what it contains
     is the prompt's own constraints restated back at whoever is reading. #}
  <details id="ask-reasoning-box" class="reasoning-box" hidden>
    <summary>Reasoning</summary>
    <div id="ask-reasoning" class="reasoning"></div>
  </details>
  <p id="ask-progress" class="muted" hidden></p>
  {# The answer as it is written, in plain text; the markdown is rendered by
     the server and swapped in whole at the end. `aria-live="polite"` so a
     reader who cannot see it knows the answer is arriving. #}
  <pre id="ask-live" class="answer-live" aria-live="polite" hidden></pre>
  <p id="ask-status" class="sr-only" role="status"></p>
  <div id="ask-result"></div>
  <div id="capture-result"></div>
  <p id="pane-idle" class="muted">Search to see an artifact here, beside the
    lines it came from.</p>
</div>
```

Move the `#capture-result` div from Task 4's bar markup down into here; the
receipt is a focus-column thing.

- [ ] **Step 5: Repoint the driver**

In `assets/app.js`, `askDriver()` currently reads `document.getElementById('ask-form')`
and the form's `input[name="q"]`. Change both, and nothing else:

```js
    var form = document.getElementById('box-form');
    if (!form) return;
    var askBtn = form.querySelector('[data-verb="ask"]');
    if (!askBtn) return;   // no model, no button, no driver
    ...
    var rail = document.getElementById('results');
```

and replace the `form.addEventListener('submit', …)` binding with
`askBtn.addEventListener('click', …)`, reading
`form.querySelector('textarea[name="q"]').value`. Everything between — the
generation counter, `stop()`, `startTicking()`, `fail()`, `openStream()` — is
untouched.

- [ ] **Step 6: Delete the old page and run both suites**

```bash
git rm src/web/templates/ask.html
cargo test --locked
cargo test --test browser_ask -- --ignored   # needs node + headless Chrome
```
Update the two URLs in `tests/browser/ask_stream.js` (`/ui/ask` → `/ui`) and the
message string at `tests/eval.rs:171`. Expected: PASS, including the
stream-request count assertions.

- [ ] **Step 7: Commit**

```bash
git add -A src assets tests
git commit -m "feat(web): the Ask verb folds into the one box"
```

---

## Task 6: One act in flight, and the rail belongs to the act

**Files:**
- Modify: `assets/app.js` (disable on ask; rail heading; `← results`)
- Modify: `src/web/workspace.rs` (`rail_fragment` gains the heading)
- Modify: `src/web/templates/_ask_rail.html`
- Test: `src/web/ui.rs` tests module (source-scan), `tests/browser/ask_stream.js`

**Interfaces:**
- Consumes: the driver from Task 5.
- Produces: `stop()` and the ask-button handler own the enable/disable of
  `textarea[name="q"]` and `[data-verb]`. `rail_fragment` renders
  `Written from · n` plus a `[data-rerun]` anchor into `#rail-head`.

- [ ] **Step 1: Write the failing tests**

A source-scanning unit test, modelled on
`the_stream_driver_closes_the_event_source_on_every_exit` at `src/web/ui.rs:9979`:

```rust
/// One act in flight. Pressing Ask disables the box, which is what disables
/// search-while-type: a disabled input fires no `keyup`, so the `hx-trigger`
/// goes quiet with no second mechanism and no flag to keep in sync.
///
/// Every exit already runs through `stop()` — completion, the Stop button, and
/// the transport error that `fail()` funnels into it. That last one is why the
/// re-enable belongs there and nowhere else: put it on the done handler and a
/// dropped connection locks the page forever.
#[test]
fn the_ask_disables_the_surface_and_only_stop_gives_it_back() {
    let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
    let js = String::from_utf8(js.data.into_owned()).unwrap();

    let stop = js
        .split_once("function stop() {")
        .expect("the driver has no stop()")
        .1;
    let stop = &stop[..stop.find("\n  }").unwrap()];
    assert!(
        stop.contains("setBusy(false)"),
        "stop() does not give the surface back: {stop}"
    );

    let busy = js
        .split_once("function setBusy(")
        .expect("the driver has no setBusy()")
        .1;
    let busy = &busy[..busy.find("\n    }").unwrap()];
    assert!(
        busy.contains("box.disabled"),
        "setBusy does not disable the box, so typing still searches: {busy}"
    );
}
```

And in `tests/browser/ask_stream.js`, add a case to the existing harness: after
the ask is submitted, assert `document.querySelector('textarea[name="q"]').disabled`
is `true`; after the `done` event, assert it is `false`; and in the
transport-error case, assert it is `false` there too.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --locked the_ask_disables_the_surface`
Expected: FAIL — `setBusy` does not exist.

- [ ] **Step 3: Implement**

In `askDriver()`, beside `stop()`:

```js
    // The surface, while one act is in flight. Disabling the box is what
    // disables search-while-type: a disabled input fires no `keyup`, so the
    // form's `hx-trigger` goes quiet on its own.
    //
    // `aria-busy` rather than a spinner swap, and the box keeps readable
    // contrast in CSS rather than the browser's default grey — the question
    // you asked stays worth reading while you wait for it.
    function setBusy(busy) {
      box.disabled = busy;
      form.setAttribute('aria-busy', busy ? 'true' : 'false');
      var vs = form.querySelectorAll('[data-verb]');
      for (var i = 0; i < vs.length; i++) vs[i].disabled = busy;
      stopBtn.hidden = !busy;
    }
```

Call `setBusy(true)` in the ask-button handler beside `form.classList.add('asking')`,
and `setBusy(false)` inside `stop()`. Nothing else changes: completion, the Stop
button and `fail()` all already run through `stop()`.

Add to `40-workspace.css`:

```css
/* A disabled box is still a box someone is reading. The browser's default is
   a grey that says "broken" rather than "busy". */
.box:disabled { color: var(--color-fg-secondary); opacity: 1; cursor: wait; }
```

- [ ] **Step 4: The rail belongs to the act**

In `_ask_rail.html`, above the citations:

```html
{# The rail holds what the current act produced. Search results do not survive
   an Ask: they were produced by a different act and have nothing to do with
   the excerpts the answer was written from.

   The query is still in the box unedited, so nothing re-triggers the search.
   This anchor is the way back — it re-fires the same endpoint the form does,
   with whatever the box holds now. #}
<div class="rail-head" hx-swap-oob="innerHTML:#rail-head">
  <span class="pane-label">Written from · {{ citations.len() }}</span>
  <a href="#" data-rerun class="quiet-link">← results</a>
</div>
```

and in `app.js`:

```js
  // The way back to results after an Ask. Re-fires the form's own request
  // rather than storing the last result set: one anchor, no state to go stale.
  document.addEventListener('click', function (e) {
    var back = e.target.closest ? e.target.closest('[data-rerun]') : null;
    if (!back) return;
    e.preventDefault();
    var form = document.getElementById('box-form');
    if (form) htmx.trigger(form, 'submit');
  });
```

Have `search_results` render `{{ results.len() }} result…` into the same
`#rail-head` out-of-band, so the heading always names the act that filled the rail.

- [ ] **Step 5: Run both suites**

Run: `cargo test --locked && cargo test --test browser_ask -- --ignored`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A src assets tests
git commit -m "feat(web): one act in flight, and the rail belongs to it"
```

---

## Task 7: The command overlay goes

**Files:**
- Modify: `src/web/templates/layout.html` (delete the `.cmdk` block)
- Modify: `assets/app.js` (delete `commandBar()` and its call)
- Modify: `assets/css/30-components.css` (delete `.cmdk*` rules)

**Interfaces:**
- Consumes: the workspace box from Task 3.
- Produces: `/` focuses `textarea[name="q"]` on the workspace instead of opening
  an overlay.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_second_text_surface_is_gone() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
    let html = get(&app, "/ui", &cookie).await;
    assert!(!html.contains("cmdk"), "the overlay was a second box for a solved problem");

    let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
    let js = String::from_utf8(js.data.into_owned()).unwrap();
    assert!(!js.contains("commandBar"), "and its driver went with it");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --locked the_second_text_surface_is_gone`
Expected: FAIL — the overlay is in `layout.html`.

- [ ] **Step 3: Delete**

Remove the `<div class="cmdk" hidden>…</div>` block from `layout.html`, the
`commandBar()` function and its call site from `app.js`, and every `.cmdk` rule
from `30-components.css`. Keep the existing `/` handler that focuses the search
box — it already exists for the search page and now applies everywhere.

- [ ] **Step 4: Run and commit**

```bash
cargo test --locked && cargo clippy --all-targets --locked -- -D warnings
git add -A src assets
git commit -m "refactor(web): the command overlay was a second box for a solved problem"
```

---

## Task 8: Three stylesheets become one

**Files:**
- Create: `assets/css/40-workspace.css`
- Delete: `assets/css/40-search.css`, `41-capture.css`, `45-ask.css`
- Modify: `build.rs` if it enumerates the CSS layers

**Interfaces:**
- Consumes: the markup from Tasks 3–6.
- Produces: one stylesheet, sorting where `40-search.css` did. No selector that
  three pages used to disagree about survives in more than one form.

- [ ] **Step 1: Check how the layers are assembled**

Run: `grep -n "css" build.rs`
If `build.rs` lists the layer files explicitly, the new name goes in the list and
the three old ones come out. If it globs the directory, nothing to do.

- [ ] **Step 2: Concatenate, then cut**

```bash
cat assets/css/40-search.css assets/css/41-capture.css assets/css/45-ask.css \
    > assets/css/40-workspace.css
git rm assets/css/40-search.css assets/css/41-capture.css assets/css/45-ask.css
```

Then read it top to bottom and delete what three pages needed and one does not:
every rule scoped to a page that no longer exists, every duplicated `.rail`,
`.pane` and `.spinner` declaration, and the width rules the three pages used to
disagree about — the region grid in `20-layout.css` owns width now.

- [ ] **Step 3: Verify nothing lost its styling**

Run: `cargo run` and open `/ui`. Check each state by hand: idle, typing with
results, a result open with its source, an answer streaming, a staged file.
Then run `cargo test --locked` — the asset stamp is a hash of the bytes, and
`assets.rs` tests assert the stamp moves when they do.

- [ ] **Step 4: Record the numbers**

```bash
wc -l assets/css/40-workspace.css
```

Put the figure in the commit message beside the 813 it replaces. The spec's §9
claims a reduction; this is where it becomes checkable.

- [ ] **Step 5: Commit**

```bash
git add -A assets build.rs
git commit -m "refactor(css): three page stylesheets become one workspace stylesheet"
```

---

## Task 9: The V1 visual pass

**Files:**
- Modify: `assets/css/00-tokens.css` (nothing but comments — the palette is unchanged)
- Modify: `assets/css/40-workspace.css`
- Modify: `assets/css/30-components.css`
- Modify: `src/web/templates/_results.html` (the score meter)

**Interfaces:**
- Consumes: Task 8's stylesheet.
- Produces: `.score-meter` in `_results.html`, rendered from the existing
  `RenderedResult` score field. No new template struct field.

- [ ] **Step 1: One type scale, obeyed**

`00-tokens.css` already defines `--text-xs` through `--text-2xl` and its own
comment says why: sizes were chosen per component, so two things meaning the
same thing rarely looked the same size. Grep for hardcoded sizes and replace
each with the nearest step:

```bash
grep -rn "font-size: [0-9]" assets/css/ | grep -v "var(--text"
```

Three roles carry almost everything: `--text-lg` for a title, `--text-base` for
body, `--text-xs` for meta.

- [ ] **Step 2: The score gets a shape**

In `_results.html`, beside the existing score text:

```html
{# Rank, as a shape. The decimal is exact and unscannable; five cells read at a
   glance and the number stays for anyone who wants it. `aria-hidden` because
   the number beside it already says this to a reader who cannot see it. #}
<span class="score-meter" aria-hidden="true" style="--fill: {{ r.score_tenths }}"></span>
<span class="score mono">{{ r.score_text }}</span>
```

Add `score_tenths: u8` to `RenderedResult` where it is built in `src/web/ui.rs`
(`(score * 5.0).round() as u8`, clamped to `0..=5`), and:

```css
/* Five cells, filled from a custom property so the markup carries a number and
   the stylesheet carries the drawing. */
.score-meter {
  display: inline-block; width: 2.6rem; height: 0.4rem;
  background: linear-gradient(to right,
    var(--color-accent) calc(var(--fill) * 20%),
    var(--color-border) calc(var(--fill) * 20%));
  border-radius: var(--radius-sm);
}
```

- [ ] **Step 3: Region micro-labels**

```css
/* Uppercase, letterspaced, with a hairline running out from each. This is most
   of why the workspace reads as organised rather than as three lists that
   happen to be adjacent. */
.pane-label {
  display: flex; align-items: center; gap: 0.5rem;
  font-size: var(--text-xs); text-transform: uppercase; letter-spacing: 0.08em;
  color: var(--color-fg-muted); margin: 0 0 0.5rem;
}
.pane-label::after {
  content: ""; flex: 1; height: 1px; background: var(--color-border-subtle);
}
```

- [ ] **Step 4: Air, filled selection, focus rings**

Selected rail rows get `background: var(--color-bg-hover)` and a
`--color-border-strong` edge rather than an outline; the source strip gets a
`border-left: 2px solid var(--color-border)` and `--font-mono`; and every
focusable control gets one consistent ring:

```css
:where(a, button, input, textarea, select, [tabindex]):focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 2px;
}
```

- [ ] **Step 5: Check both themes**

Run `cargo run`, open `/ui`, and toggle the theme button through all three
states: system, explicit light, explicit dark. Every colour must come from a
token defined on bare `:root`; grep for a colour whose only definition is inside
a media or `[data-theme]` block:

```bash
grep -n "color: #" assets/css/40-workspace.css
```
Expected: no output.

- [ ] **Step 6: Commit**

```bash
git add -A assets src/web
git commit -m "feat(css): the V1 pass — one type scale, the score as a shape, labelled regions"
```

---

## Task 10: The phone

**Files:**
- Modify: `assets/css/50-phone.css`
- Modify: `src/web/templates/layout.html` (the tab bar)

**Interfaces:**
- Consumes: the workspace from Tasks 3–6.
- Produces: a three-entry tab bar — Search, Judge, Insights. The fixed bar at
  the thumb carries the box and the verb row and nothing else.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_tab_bar_points_at_the_three_places_there_are() {
    let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
    let html = get(&app, "/ui", &cookie).await;
    let bar = html
        .split_once("<nav class=\"tabbar\"")
        .expect("the tab bar is there")
        .1;
    let bar = &bar[..bar.find("</nav>").unwrap()];
    assert!(bar.contains("/ui/insights"), "Insights is a destination");
    assert!(!bar.contains("/ui/capture"), "Capture is not a place any more");
    assert!(!bar.contains("/ui/ask"), "and neither is Ask");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --locked the_tab_bar_points_at_the_three_places`
Expected: FAIL — the bar still lists Capture and Ask.

- [ ] **Step 3: Rewrite the tab bar**

In `layout.html`, replace the Capture and Ask entries with one Insights entry,
keeping the Judge entry and its `tab-dot` rule exactly as they are. Point the
Search entry at `/ui`.

- [ ] **Step 4: Update the fixed bar's rules**

In `50-phone.css`, the block scoped to `.regions-rail-focus-source .region-bar`
now applies to the workspace, which is the only page with that region set — so
the selector is already right. Add the verb row to what the bar keeps, and the
staged box and hint to what it hides:

```css
  /* Not thumb furniture. The bar has to stay one line plus its verbs; the
     hint, the chips and the staged file all belong to a page you are reading,
     not to a box you are typing into with one hand. */
  .regions-rail-focus-source .region-bar .hint,
  .regions-rail-focus-source .region-bar .chips,
  .regions-rail-focus-source .region-bar .keyhint,
  .regions-rail-focus-source .region-bar #staged { display: none; }
```

Leave every other rule in the file alone: the 16px inputs, the 44px targets, the
safe-area insets, `has-selection` and the container-query back link all keep
their reasons.

- [ ] **Step 5: Check it on a phone**

Run `cargo run --release`, then open the app from a phone on the same network.
Check: the box sits above the tab bar and does not scroll away; the software
keyboard does not zoom the page; a result opens full-width with a back link; the
answer streams into the same column.

- [ ] **Step 6: Commit**

```bash
git add -A assets src/web
git commit -m "feat(web): three destinations on the phone, and the box keeps the thumb"
```

---

## Task 11: The Insights measures — separable

> This is the half of spec §7 that `ROADMAP.md` lists as a branch of its own.
> Everything before this task ships without it. If it is cut, cut it whole:
> delete this task and change the Insights page's heading, nothing else.

**Files:**
- Modify: `src/web/insights.rs`
- Modify: `src/web/templates/insights.html`
- Modify: `src/store/` — one new read-only query module or additions to an existing one

**Interfaces:**
- Consumes: `InsightsTemplate` from Tasks 1–2.
- Produces: `InsightsTemplate` gains `measures: Measures`, where

```rust
/// What this memory is like. Aggregates over tables that already exist: no new
/// table, no sweep, no model call.
pub struct Measures {
    /// Artifacts, corpora, passages.
    pub held: Held,
    /// The distribution of accessibility across the base, bucketed, so what is
    /// decaying is visible before it is unreachable.
    pub fading: Vec<Bucket>,
    /// Read from the positions judged searches actually gave. Not a proxy
    /// score — the same computation the judge page reports as today's number,
    /// as a series rather than a figure.
    pub retrieval: Vec<RetrievalPoint>,
    /// Open gaps, by week.
    pub gaps_over_time: Vec<Bucket>,
}
```

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_measures_read_what_the_base_already_recorded() {
    let (app, cookie, core) = app_with_judged_searches().await;
    let html = get(&app, "/ui/insights", &cookie).await;

    assert!(html.contains("recall@10"), "the measure is named");
    assert!(html.contains("MRR"), "and so is the other one");
    assert!(html.contains("held"), "how much is held");

    // Read, never computed at request time: the page must not embed or call a
    // model. The constraint at the top of ROADMAP.md holds here too.
    let before = core.embedder.calls();
    let _ = get(&app, "/ui/insights", &cookie).await;
    assert_eq!(core.embedder.calls(), before, "the page embeds nothing");
}
```

Write `app_with_judged_searches()` beside it, using the same judging helpers the
existing `judge.rs` tests use.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --locked the_measures_read_what_the_base`
Expected: FAIL — no measures on the page.

- [ ] **Step 3: Write the queries**

The recall@10 and MRR computation already exists for the judge page — find it
(`grep -rn "recall\|mrr" src/`) and reuse it rather than writing a second one
that can disagree with the first. The remaining three are `COUNT`/`GROUP BY`
over `artifacts`, `corpora` and `gaps`.

- [ ] **Step 4: Render**

One section per measure, above the maintenance sections. Numbers in
`--font-mono`, labelled with the same `.pane-label` the workspace uses.

- [ ] **Step 5: Run and commit**

```bash
cargo test --locked && cargo clippy --all-targets --locked -- -D warnings
git add -A src
git commit -m "feat(web): what this memory is like, on the Insights page"
```

---

## Task 12: Close the loop

**Files:**
- Modify: `ROADMAP.md`
- Modify: `README.md` if it describes three pages
- Modify: `docs/superpowers/plans/2026-08-22-one-text-surface.md` (the numbers)

- [ ] **Step 1: Record the after-figures**

```bash
wc -l src/web/templates/workspace.html assets/css/40-workspace.css \
      src/web/workspace.rs src/web/insights.rs src/web/ui.rs
```

Write them into the table below, beside the before-figures from spec §9. The
spec claims a reduction in templates and CSS; this is the evidence.

| File | Before | After |
|---|---|---|
| templates (search + capture + ask + _sitting) | 604 | |
| CSS (40-search + 41-capture + 45-ask) | 813 | |
| `src/web/ui.rs` | 10,880 | |

- [ ] **Step 2: Strike the roadmap item**

`ROADMAP.md:476` — *One text surface for the whole web UI, as the panel now
has.* Move it to the built section with the spec's path, the way the other
completed items name theirs.

- [ ] **Step 3: Commit**

```bash
git add ROADMAP.md README.md docs/
git commit -m "docs(roadmap): one text surface, built"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| §3 route and surviving doors | 3 (`/ui`, `/ui/search`), 4 (`/ui/capture`), 5 (`/ui/ask`), 1 (`/ui/ops`) |
| §3 `.cmdk` deleted | 7 |
| §4 the box, auto-grow, verbs, files, chips | 3, 4 |
| §4 one act in flight, three exits | 6 |
| §5 rail belongs to the act, `← results` | 6 |
| §5 verdict bar under the answer | 5 (moves with the ask markup, unchanged) |
| §6 nav and what leaves | 1, 2, 10 |
| §6 sitting UI retired, mechanism kept | 3 |
| §7a Insights maintenance | 1, 2 |
| §7b Insights measures | 11 |
| §8 V1 visual pass | 8, 9 |
| §9 code shape, measured | 3, 4, 5, 8, 12 |
| §10 testing | in every task; browser suite in 5, 6 |
| §11 constraints | Global Constraints |

**Placeholder scan:** No TBDs. Task 8 Step 2 and Task 11 Step 3 direct the
engineer to read existing code rather than quoting it — in both cases the code
is a verbatim move or a reuse of an existing computation, and quoting it here
would invite a second copy that can drift from the first.

**Type consistency:** `WorkspaceTemplate` is defined once in Task 3 and only
gained fields in Tasks 4 and 5 — no field is renamed. `base_template` is
introduced in Task 4 and used by Task 5 with the same signature. `setBusy` is
named identically in the Task 6 test and implementation. The element ids
(`ask-live`, `ask-result`, `ask-status`, `ask-stop`, `ask-progress`,
`ask-verdict`, `results`, `rail-head`, `pane`) are used consistently across
Tasks 3, 5, 6 and the browser suite.
