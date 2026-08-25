# Onboarding and Wording Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make an empty base teach a first-time user what engram is and what to do, close the captured-to-searchable blind spot, and settle the wording — before the application is opened to more than one person.

**Architecture:** Every change is at the web door. One new boolean on the workspace template (`held`) carries the whole first-run story; everything else is template copy, one `<details>` on Insights, one include on the capture receipt, and a field on the settings template. No schema migrations, no new routes, no new persistence, no per-user state.

**Tech Stack:** Rust, axum, askama templates (`src/web/templates/`), htmx plus one hand-written `assets/app.js`, layered CSS (`assets/css/00-…` through `50-phone.css`), SQLite via sqlx. Tests are inline `#[cfg(test)] mod tests` in the file under test, run with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-08-25-onboarding-polish-design.md`

## Global Constraints

- **Branch:** `feat/onboarding-polish`, already created off `master` and already holding the spec commit. Do not branch again.
- **Onboarding is a property of an empty base, not of a new user.** No welcome page, no tour, no scripted sequence, no per-user "has seen it" flag, no new table or column. If a task seems to want one, it is the wrong task.
- **"Artifact" stays.** It is deliberate and defended in the comments. Do not rename it anywhere — not in templates, not in Rust, not in tests.
- **Never change a URL, an API field name, or an MCP schema.** `/ui/corpora/…` and `/ui/artifacts/…` keep their paths. This is a copy change only.
- **Never render the word `Untitled`,** and never render `Chunk N` as a title. Existing tests enforce this.
- **Explanations are visible text, never a `title=` attribute.** A tooltip is unreachable on a touch screen. The established pattern is `_results.html`'s `rail-why`: the badge stays as the scannable form and one quiet sentence sits under it. `title=` may remain *in addition*, never *instead*.
- **Comment style:** this codebase explains *why* in prose above the code and expects it. Match it. A template comment that only restates the markup is worse than none.
- **Truncate by chars, never by bytes.** Slicing mid-codepoint panics; the corpus is largely German.
- **Do not change:** the phone hiding the KIND chips (`50-phone.css`), the phone badge dropping its number (`layout.html`), Judge's rank numbers stopping at nine (`judge.rs`), or the IR vocabulary on the judge page itself (recall@10, MRR).
- **Run `cargo fmt` before every commit.** The tree is formatted and the last commit on `master` was a formatting sweep.

---

### Task 1: An empty base does not offer what it cannot do

On a base with nothing in it, searching returns nothing and asking returns an
abstention. The box presently offers all three verbs anyway, names Capture last
in its placeholder, and prints seven keyboard shortcuts for moving through a
list with nothing in it.

**Files:**
- Modify: `src/web/workspace.rs` (add `held` to `WorkspaceTemplate`, set it in `base_template`)
- Modify: `src/web/templates/workspace.html` (Ask button, placeholder, keyhint bar)
- Test: `src/web/workspace.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `WorkspaceTemplate.held: bool` — true when the base holds at least one source. Read by Task 2's template changes.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `src/web/workspace.rs`. The module already
has a `workspace(uri)` helper that boots the app and returns the rendered HTML;
this test needs a second helper that seeds a capture first, because the
interesting assertion is the *difference* between the two states.

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::workspace::tests::an_empty_base_offers_only_the_verb_that_can_work`
Expected: FAIL — the empty render still contains `data-verb="ask"`.

- [ ] **Step 3: Add the field to the template struct**

In `src/web/workspace.rs`, add to `WorkspaceTemplate` immediately after `idle`:

```rust
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
```

- [ ] **Step 4: Set it in `base_template`**

In `base_template`, before the `Ok(WorkspaceTemplate { … })`:

```rust
    // The slimmest read there is, and the same one the idle rail takes. Asked
    // unconditionally because the deep-link path renders no idle rail and
    // still has to know: a search URL against an empty base is a page that
    // must not offer Ask either.
    let (corpora, _) = tenant.core.store.held_brief().await?;
```

and add `held: corpora > 0,` to the struct literal.

- [ ] **Step 5: Hide Ask on an empty base**

In `src/web/templates/workspace.html`, change the Ask button's guard:

```html
    {% if ask_enabled && held %}
    <button class="btn btn-accent" type="button" data-verb="ask" disabled>Ask</button>
    {% endif %}
```

- [ ] **Step 6: Swap the placeholder**

Replace the `placeholder` and `data-placeholder-narrow` attributes on the
`textarea` with a conditional pair. The long form names all three verbs; the
empty-base form names the one that works.

```html
            placeholder="{% if held %}Describe the situation, ask a question, or paste anything worth keeping…{% else %}Paste anything worth keeping — a note, an article, a chunk of a chat.{% endif %}"
            data-placeholder-narrow="{% if held %}Ask, search, or paste to keep…{% else %}Paste anything worth keeping…{% endif %}"
```

- [ ] **Step 7: Hide the keyboard hints on an empty base**

Change the opening tag of the `keyhint` paragraph:

```html
{% if held %}
<p class="keyhint" hidden>
```

and close the block with `{% endif %}` after its `</p>`.

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo test --lib web::workspace::tests::an_empty_base_offers_only_the_verb_that_can_work`
Expected: PASS

- [ ] **Step 9: Run the whole web suite for regressions**

Run: `cargo test --lib web::`
Expected: PASS. If an existing test asserted `data-verb="ask"` against an
unseeded core, seed it with `ingest_capture` as above rather than weakening the
new guard.

- [ ] **Step 10: Commit**

```bash
cargo fmt
git add src/web/workspace.rs src/web/templates/workspace.html
git commit -m "feat: an empty base offers only the verb that can work"
```

---

### Task 2: The empty base says what engram is, and that it is yours

The tagline lives on `login.html`, which a user arriving through an identity
provider never sees. Nothing on the workspace says what the application does,
and nothing anywhere says the base is theirs alone — which is the exact
hesitation a person has before pasting their own notes onto somebody else's
server. The pane meanwhile gives an instruction that cannot be followed:
"Search to see an artifact here", to a person with nothing to search.

**Files:**
- Modify: `src/web/templates/workspace.html` (box hint, pane idle text)
- Test: `src/web/workspace.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `WorkspaceTemplate.held: bool` from Task 1.

- [ ] **Step 1: Write the failing test**

```rust
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
            html.contains("Nobody else can search it"),
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::workspace::tests::an_empty_base_says_what_this_is_and_whose_it_is`
Expected: FAIL — "finds it again by meaning" is not in the page.

- [ ] **Step 3: Make the box hint conditional**

Replace the `box-hint` paragraph in `src/web/templates/workspace.html`:

```html
  {# Two hints, because the two states need different sentences. With something
     held, the useful thing to say is how to phrase a query — a whole sentence
     embeds better than the two nouns from it a reader would otherwise type.
     With nothing held there is no query to phrase, and the useful thing to say
     is what this is and whose it is. The second half is not decoration: an
     operator arriving through an identity provider never sees the login card,
     so this is the only place the application introduces itself, and the
     boundary is the question a person actually has before pasting their own
     notes onto a server somebody else runs. #}
  <p id="box-hint" class="muted hint">
    {%- if held -%}
    A sentence or a whole paragraph finds more than keywords do — paste what you
    are looking at.
    {%- else -%}
    engram keeps what you paste in your own words and finds it again by meaning,
    not by keywords. This base is yours: nobody else can search it.
    {%- endif -%}
  </p>
```

- [ ] **Step 4: Make the pane idle text conditional**

Replace the `pane-idle` paragraph:

```html
    {# With something held this names where an artifact will appear. With
       nothing held it named a search that cannot be run, which is an
       instruction to do the one thing the page has just been arranged to
       prevent. It says what will happen to the first paste instead. #}
    <p id="pane-idle" class="muted">
      {%- if held -%}
      Search to see an artifact here, beside the lines it came from.
      {%- else -%}
      Whatever you paste is split into passages and embedded so it can be found
      by meaning. The original is kept exactly as you wrote it, and every result
      can be read beside it.
      {%- endif -%}
    </p>
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --lib web::workspace::tests::an_empty_base_says_what_this_is_and_whose_it_is`
Expected: PASS

- [ ] **Step 6: Run the web suite**

Run: `cargo test --lib web::`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/web/templates/workspace.html src/web/workspace.rs
git commit -m "feat: the empty base introduces itself and states the boundary"
```

---

### Task 3: The capture receipt shows the work

`_captured.html` answers a paste with one dead line — "Captured — view source"
— while embedding runs as a background job. A person who immediately searches
for what they just pasted gets nothing and concludes the application is broken.
This is the most likely abandonment moment in the app.

The fragment that answers it already exists. `_queue.html` reports per-source
progress, polls itself every three seconds, stops polling when nothing is
moving, and already listens for the `captured` event the box fires. It is
rendered only on Insights, and `_captured.html`'s own comment records the cost:
"nothing else on the workspace says the paste landed."

**Files:**
- Modify: `src/web/templates/_captured.html`
- Test: `src/web/workspace.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: the existing `GET /ui/queue` route and `_queue.html` fragment. Neither changes.

- [ ] **Step 1: Write the failing test**

```rust
    /// The gap between "captured" and "searchable" is a background job, and it
    /// was invisible: a one-line receipt, then silence, then a search that
    /// finds nothing. The queue fragment already reports the work and already
    /// stops polling when it settles — it was only ever rendered on Insights.
    #[tokio::test]
    async fn the_capture_receipt_shows_the_work_that_is_still_running() {
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
            html.contains(r#"hx-get="/ui/queue""#),
            "the receipt fetches the queue that reports the work"
        );
        assert!(
            html.contains(r#"hx-trigger="load""#),
            "on load, so the progress is there without a second press"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::workspace::tests::the_capture_receipt_shows_the_work_that_is_still_running`
Expected: FAIL — the receipt contains no `hx-get`.

- [ ] **Step 3: Include the queue on the receipt**

In `src/web/templates/_captured.html`, add immediately after the existing
`{% endif %}` that closes the duplicate/near-duplicate branch, so it renders on
every path — a parked capture has a queue row too, and its row is the thing
that says it is parked:

```html
{# The work the press started, reported by the fragment that already knows how.
   The gap between a paste landing and that paste being searchable is a
   background job, and it was invisible from here: this receipt, then silence,
   then a search that finds nothing and reads as data loss. `_queue.html`
   carries its own polling trigger and drops it the moment nothing is moving,
   so an idle receipt costs one request and then none — the same contract it
   already honours on Insights, which is why it is included rather than
   reimplemented. #}
<div hx-get="/ui/queue" hx-trigger="load" hx-swap="outerHTML"></div>
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib web::workspace::tests::the_capture_receipt_shows_the_work_that_is_still_running`
Expected: PASS

- [ ] **Step 5: Check the receipt in the running app**

Run: `cargo run -- serve` (or the project's usual invocation), sign in, paste a
paragraph, and confirm the row appears under the receipt, shows a status, and
stops updating once the capture settles. The polling stopping is the half a
unit test cannot see.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/web/templates/_captured.html src/web/workspace.rs
git commit -m "feat: the capture receipt reports the work still running"
```

---

### Task 4: Insights on an empty base is one line

A new user opening Insights is shown Held 0, an empty Reach, a Retrieval panel
explaining there is nothing to measure, and "0 artifacts, 0 embedded. No jobs
queued." A page of zeros reads as a system with something wrong with it.

**Files:**
- Modify: `src/web/templates/insights.html`
- Test: `src/web/insights.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `InsightsTemplate.held: crate::store::insights::Held`, which already carries `corpora` and `artifacts`.
- **Not** the `held: bool` Task 1 adds to `WorkspaceTemplate`. Two different fields on two different structs that happen to share a name; this one predates the plan and is a struct, not a boolean. Do not unify them.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `src/web/insights.rs`, following the request
pattern already used there.

```rust
    /// Five headings answered with a zero make a base with nothing wrong with
    /// it look like a backlog — the same reasoning the housekeeping summary
    /// already gives for collapsing its own empties into one sentence.
    #[tokio::test]
    async fn insights_over_an_empty_base_is_one_line_and_a_way_back() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = crate::web::test_support::body_of(res).await;
        assert!(
            html.contains("Nothing is held yet"),
            "one honest line about an empty base"
        );
        assert!(
            !html.contains("What this memory is like"),
            "no measures over nothing"
        );
        assert!(
            !html.contains("Housekeeping"),
            "and no housekeeping over no work"
        );
        assert!(
            html.contains(r#"href="/ui""#),
            "and a way back to the one place there is anything to do"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::insights::tests::insights_over_an_empty_base_is_one_line_and_a_way_back`
Expected: FAIL — the page still renders "What this memory is like".

- [ ] **Step 3: Wrap the page**

In `src/web/templates/insights.html`, immediately after `{% block content %}`,
open the branch:

```html
{# A base with nothing in it has nothing to measure, nothing hidden, nothing
   waiting and nothing retrying, and saying all four in a column of zeros makes
   a healthy empty base read as a broken full one. The same argument the
   housekeeping summary at the foot of this page already makes for itself,
   applied to the page. #}
{% if held.corpora == 0 %}
<div class="empty">
  <h2>Nothing is held yet</h2>
  <p class="muted">This page measures what the base holds and lists what needs
    you. Both wait on there being something in it.</p>
  <p><a class="btn btn-ghost btn-sm" href="/ui">Back to the box</a></p>
</div>
{% else %}
```

and immediately before `{% endblock %}` at the foot of the file, close it:

```html
{% endif %}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib web::insights::tests::insights_over_an_empty_base_is_one_line_and_a_way_back`
Expected: PASS

- [ ] **Step 5: Run the insights suite for regressions**

Run: `cargo test --lib web::insights::`
Expected: PASS. Any existing test that asserts on Insights content must seed a
capture first; several already do.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/web/templates/insights.html src/web/insights.rs
git commit -m "feat: insights over an empty base is one line, not a page of zeros"
```

---

### Task 5: Insights halves in place

Insights answers two different questions with one page: *what is in my memory
and what needs me*, and *what is the machine doing*. The second is
operator-grade — sweep history with stage identifiers, retrying jobs with target
identifiers and raw error strings, offer rates by rung — and after #52 every
user sees it.

A disclosure achieves the separation outright. A new page would need a handler,
a route, a link and an empty state of its own to achieve the same thing.

**Files:**
- Modify: `src/web/templates/insights.html`
- Test: `src/web/insights.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    /// Two questions, one page: what is in my memory and what needs me, versus
    /// what is the machine doing. The second is operator-grade — stage ids,
    /// target ids, raw error strings — and every user sees this page now.
    #[tokio::test]
    async fn the_machines_own_readout_is_behind_a_disclosure() {
        let core = crate::core::test_support::test_core().await;
        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = crate::web::test_support::body_of(res).await;

        let (above, inside) = html
            .split_once(r#"<details class="machine">"#)
            .expect("the disclosure exists");

        assert!(
            above.contains("What this memory is like"),
            "what is held stays above the fold"
        );
        assert!(
            !above.contains("Housekeeping"),
            "the machine's own readout does not"
        );
        assert!(
            inside.contains("Housekeeping"),
            "it is inside the disclosure"
        );
        assert!(
            !html.contains(r#"<details class="machine" open"#),
            "and closed: nobody opened this page to read job counts"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::insights::tests::the_machines_own_readout_is_behind_a_disclosure`
Expected: FAIL — panics on `expect("the disclosure exists")`.

- [ ] **Step 3: Open the disclosure before the housekeeping heading**

In `src/web/templates/insights.html`, replace the line `<h2>Housekeeping</h2>`
with:

```html
{# Everything below is about the instance rather than about what is in it, and
   it is the same readout for every user of it. Stage identifiers, target
   identifiers and raw error strings are what an operator opens a page for and
   what everybody else scrolls past, so the page states the memory above and
   offers the machine here, closed. A disclosure rather than a second page: a
   route would need a handler, a link and an empty state of its own to draw the
   same line. #}
<details class="machine">
  <summary><h2>What the machine is doing</h2></summary>
```

- [ ] **Step 4: Close it after the last operator section**

The sections that belong inside are, in file order: the housekeeping counts
paragraph, "The last day" with its sweep-history table, "What was offered", and
"Retrying". "Merged", "Generated", "Pursuits", "Hidden as stale", "Hidden as
near-identical", "Worth a second look" and "Captures waiting on a decision" all
stay above — they are about what is held and can be acted on by the person
reading.

Move the "Retrying" block and the "What was offered" block so they sit directly
after the sweep-history block, then close the disclosure after "Retrying" and
before the final "Nothing hidden, nothing waiting…" summary line:

```html
</details>
```

Leave the summary line outside the disclosure: it is a statement about what is
held, and it must be visible without opening anything.

- [ ] **Step 5: Style the disclosure**

Append to `assets/css/40-pages.css` (or whichever layer already carries the
Insights table styles — match the file the `.grid` rules live in):

```css
/* The machine's own readout. A summary that carries an <h2> so the heading
   level is honest, with the marker aligned to it rather than to the text
   baseline it would otherwise sit under. */
.machine { margin-top: 2rem; border-top: 1px solid var(--color-rule); padding-top: 0.5rem; }
.machine > summary { cursor: pointer; }
.machine > summary > h2 { display: inline; }
.machine > summary::marker { color: var(--color-muted); }
```

Check the variable names against the file you are editing and use whatever that
layer already calls the rule colour and the muted colour.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --lib web::insights::tests::the_machines_own_readout_is_behind_a_disclosure`
Expected: PASS

- [ ] **Step 7: Run the insights suite**

Run: `cargo test --lib web::insights::`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
cargo fmt
git add src/web/templates/insights.html src/web/insights.rs assets/css/
git commit -m "feat: the machine's own readout moves behind a disclosure"
```

---

### Task 6: One word for a source

The UI already uses two words for the same thing and switches between them
without a rule: Insights says "from N sources", the idle rail says "sources",
`corpus.html` says "this capture's wording", the URL says `corpora`. This picks
the one already doing most of the work. It is a copy change: no URL, no API
field, no Rust identifier, no test fixture name.

`Mint` becomes `Create`, and two templates carry a stale cross-reference to a
page that no longer exists.

**Files:**
- Modify: `src/web/templates/corpus.html`
- Modify: `src/web/templates/insights.html`
- Modify: `src/web/templates/settings.html`
- Modify: `src/web/templates/extension.html`
- Modify: `src/web/templates/pair.html`
- Modify: `src/web/templates/_artifact_detail.html`
- Test: `src/web/ui.rs` (inline `mod tests`)

- [ ] **Step 1: Find every user-visible occurrence**

Run:

```bash
grep -rn "corpus\|corpora\|Corpus\|Corpora\|Mint\|Housekeeping" src/web/templates/
```

Sort the hits into three piles. **Change:** words rendered as visible text.
**Leave:** anything inside `href=`, `hx-get=`, `hx-post=`, `action=`, or a
`{{ }}` expression — those are URLs and Rust identifiers. **Leave:** anything
inside a `{# … #}` comment, which is stripped before render and is the
codebase's own record of its reasoning; a comment that says "corpus" is
describing the type, not addressing the reader.

- [ ] **Step 2: Write the failing test**

Add to the existing `mod tests` in `src/web/ui.rs`. Assert on the exact strings
rather than on the bare word, because `/ui/corpora/` is a URL and must survive.

```rust
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
            !corpus.contains("Written from this corpus"),
            "the page a person reads does not say corpus"
        );
        assert!(
            corpus.contains("Written from this source"),
            "it says source"
        );
        assert!(
            corpus.contains("/ui/corpora/"),
            "and the URL is untouched, because a path is not addressed to anyone"
        );

        let settings = get(&app, "/ui/settings", &cookie).await;
        assert!(!settings.contains(">Mint<"), "Mint is a word about coins");
        assert!(settings.contains(">Create<"), "the button says what it does");

        let ext = get(&app, "/ui/extension", &cookie).await;
        assert!(
            !ext.contains("Housekeeping → API tokens"),
            "Housekeeping is a heading on Insights; tokens are on Settings"
        );
        assert!(ext.contains("Settings → API tokens"), "named where they are");
    }
```

Confirm the route for the extension page before running: it is registered in
`src/web/extension.rs`. If it is `/extension/install` rather than
`/ui/extension`, use that URI.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib web::ui::tests::a_source_is_called_a_source_everywhere_a_person_reads_it`
Expected: FAIL

- [ ] **Step 4: Make the replacements**

Work the pile from Step 1. The visible-text changes are:

- `corpus.html`: "Written from this corpus" → "Written from this source"; "this capture's wording" stays, it is already plain.
- `insights.html`: any visible "corpora" → "sources".
- `settings.html`: the `Mint` button label → `Create`; the placeholder "Token name, e.g. claude-code" stays.
- `extension.html` and `pair.html`: "Housekeeping → API tokens" → "Settings → API tokens".
- `_artifact_detail.html`: any visible "corpus" → "source".

Do not touch `/ui/corpora/` in any attribute, and do not rename `corpus_label`,
`CorpusTemplate`, `corpus_view.rs`, or any other Rust identifier.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --lib web::ui::tests::a_source_is_called_a_source_everywhere_a_person_reads_it`
Expected: PASS

- [ ] **Step 6: Run the whole suite**

Run: `cargo test --lib`
Expected: PASS. Existing tests that assert on the old copy get their strings
updated; that is the change landing, not a regression.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/web/templates/ src/web/ui.rs
git commit -m "feat: one word for a source, and two stale cross-references fixed"
```

---

### Task 7: Judge is a task, not a role

"Judge" in the navigation names a role rather than a task, and the badge invites
a click into a page whose purpose is never stated. The page itself asks a real
cognitive task — twenty unordered candidates, "which of these was the one you
needed?" — with no statement of why it is worth doing.

The duplicate-pair queue takes "decisions", so the two stop competing for the
word "review".

**Files:**
- Modify: `src/web/templates/layout.html` (top row and tabbar labels)
- Modify: `src/web/templates/judge.html` (the standing line)
- Modify: `src/web/templates/_judge_card.html` (empty-queue copy)
- Test: `src/web/judge.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `src/web/judge.rs`, matching the request
pattern already in that module.

```rust
    /// The nav named a role. The page asks a real cognitive task and never
    /// said why it was worth doing — and the number on Insights is exactly
    /// what it is worth: recall@10 and MRR are read off these verdicts.
    #[tokio::test]
    async fn the_nav_names_the_task_and_the_page_says_why_it_matters() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/judge")
                    .header("cookie", &cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = crate::web::test_support::body_of(res).await;
        assert!(
            html.contains("Review searches"),
            "the nav names the task, not a role"
        );
        assert!(
            html.contains("your own searches, coming back unlabelled"),
            "and the page says what it is asking for"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::judge::tests::the_nav_names_the_task_and_the_page_says_why_it_matters`
Expected: FAIL

- [ ] **Step 3: Rename the nav entries**

In `src/web/templates/layout.html`, the top row:

```html
      <a href="/ui/judge">Review searches{% if n > &0 %}
        <span class="badge badge-accent">{{ n }}</span>{% endif %}</a>
```

and the tabbar, where the width is 52px and the shorter form is the only one
that fits:

```html
    <a href="/ui/judge">
      <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M4 10.5l4 4 8-9"/></svg>Review{% if n > &0 %}
      <span class="tab-dot" aria-label="{{ n }} waiting"></span>{% endif %}</a>
```

- [ ] **Step 4: Say what the page is asking for**

In `src/web/templates/judge.html`, immediately after `{% block content %}` and
before the `_judge_tune.html` include:

```html
{# What the page is for, said once and standing. The card asks a genuine
   cognitive task — twenty unordered candidates and a question — and never
   said why it was worth answering. The answer is the one number on Insights
   that is not a proxy: recall@10 and MRR are read off exactly these verdicts,
   which is why an unlabelled, shuffled pool is the only honest way to ask. #}
<p class="muted hint">These are your own searches, coming back unlabelled and
  shuffled. Saying which result you actually wanted is what turns the retrieval
  figure on Insights into a measurement rather than a guess.</p>
```

- [ ] **Step 5: Say it on the empty queue too**

In `src/web/templates/_judge_card.html`, the `{% when None %}` arm currently
reads "Nothing to judge." Replace that paragraph:

```html
<p class="muted">Nothing to review — every recorded search has a verdict.
  <a href="/ui">Back to the box.</a></p>
```

- [ ] **Step 6: Retire the collision on "review"**

Run `grep -rn "review queue\|review" src/web/templates/` and change any visible
text calling the duplicate-pair queue a "review queue" to "decisions". The
Insights heading for it is already "Needs you" and stays.

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test --lib web::judge::tests::the_nav_names_the_task_and_the_page_says_why_it_matters`
Expected: PASS

- [ ] **Step 8: Run the whole suite**

Run: `cargo test --lib`
Expected: PASS. Existing tests asserting the nav label `Judge` get updated.

- [ ] **Step 9: Commit**

```bash
cargo fmt
git add src/web/templates/ src/web/judge.rs
git commit -m "feat: the nav names the task, and the page says what it is for"
```

---

### Task 8: Badges say in words what they mean

The application has a precise private vocabulary that a new user does not
share. Several terms already have the right treatment: `_results.html` puts the
badge on the row as the scannable form and one quiet `rail-why` sentence under
it. Others leave the whole meaning in a `title=` attribute, which no touch
screen can reach.

Terms in scope for a visible gloss: `superseded`, `deprecated`, `parked`,
`model-written`, and the three icon-only buttons on the artifact pane.

**Files:**
- Modify: `src/web/templates/_artifact_detail.html`
- Modify: `src/web/templates/corpus.html`
- Test: `src/web/ui.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    /// A tooltip is unreachable on a touch screen, and the terms that most
    /// need explaining are the ones a phone meets first. The badge stays as
    /// the scannable form; the sentence under it is what makes it readable —
    /// the pattern `_results.html` already sets with `rail-why`.
    #[tokio::test]
    async fn the_icon_controls_say_in_words_what_they_do() {
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
        let html = get(&app, &format!("/ui/corpora/{id}"), &cookie).await;
        assert!(
            html.contains("Hiding keeps it and takes it out of results"),
            "the hide control says what it does where a finger can read it"
        );
        assert!(
            !html.contains(r#"title="Hide from results"#)
                || html.contains("Hiding keeps it and takes it out of results"),
            "a title may stay in addition, never instead"
        );
    }
```

Adjust the asserted page to whichever of the corpus page or the artifact pane
actually renders the icon row; run `grep -n "btn-icon" src/web/templates/` to
confirm before writing the URI.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::ui::tests::the_icon_controls_say_in_words_what_they_do`
Expected: FAIL

- [ ] **Step 3: Add the visible line under the icon row**

In `src/web/templates/_artifact_detail.html`, under the row holding the three
`btn-icon` controls:

```html
{# The row is three icons and the meaning of all three was in `title`
   attributes, which a finger cannot reach. One line under them rather than
   three labels beside them: the icons stay as the compact form, and the
   sentence is what makes them legible the first time. `title` stays on each
   button as well — it is what a pointer already reads. #}
<p class="muted hint">Confirm marks it still accurate. Hiding keeps it and takes
  it out of results, and can be undone. Deleting cannot.</p>
```

- [ ] **Step 4: Gloss the remaining badges**

Run `grep -rn "superseded\|deprecated\|parked" src/web/templates/`. Wherever one
of these appears as a badge or a heading with no adjacent plain sentence, add
one, in the voice the surrounding page already uses:

- superseded — "kept, but taken out of results because something near-identical won"
- deprecated — "flagged stale and taken out of results; still stored and still readable"
- parked — "stored, but nothing is spent on it until you decide"

Where a heading already carries the explanation — Insights says "Hidden as
near-identical" and "Still stored and still readable; kept out of results only"
— leave it: the gloss is already there and a second one is noise.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --lib web::ui::tests::the_icon_controls_say_in_words_what_they_do`
Expected: PASS

- [ ] **Step 6: Run the suite**

Run: `cargo test --lib`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/web/templates/ src/web/ui.rs
git commit -m "feat: the vocabulary says itself in words, not in tooltips"
```

---

### Task 9: Settings is reachable, and says whose base this is

Settings is a quiet link at the foot of Insights, which is to say unreachable on
a phone for anyone who does not already know where it is. And after #52 there is
nowhere at all that says which account is looking at this base.

Sign out stays in the top row: making it a two-click operation to gain a nav
slot is the wrong trade. Settings joins it, and the tabbar.

**Files:**
- Modify: `src/web/templates/layout.html` (top row link, tabbar entry)
- Modify: `src/web/templates/settings.html` (the account line)
- Modify: `src/web/ui.rs` (`SettingsTemplate` gains `account`)
- Modify: `assets/css/50-phone.css` (tabbar sizing for a fourth entry)
- Test: `src/web/ui.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `SettingsTemplate.account: String` — the signed-in subject, or the email where the identity provider supplied one.

- [ ] **Step 1: Write the failing test**

```rust
    /// After #52 every user has their own base, and nothing anywhere said
    /// which account was looking at one. Settings is where account things
    /// live; it was also the one page a phone could not reach.
    #[tokio::test]
    async fn settings_is_reachable_and_names_the_account() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core).await;

        let html = get(&app, "/ui", &cookie).await;
        assert_eq!(
            html.matches(r#"href="/ui/settings""#).count(),
            2,
            "the top row and the tabbar, so a phone can reach it too"
        );

        let settings = get(&app, "/ui/settings", &cookie).await;
        assert!(
            settings.contains("Signed in as"),
            "and the page that holds account things says which account"
        );
        assert!(
            settings.contains(crate::store::TEST_SUBJECT),
            "named, not merely alluded to"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib web::ui::tests::settings_is_reachable_and_names_the_account`
Expected: FAIL — the count is 0.

- [ ] **Step 3: Add Settings to both navigation rows**

In `src/web/templates/layout.html`, in the top row before the spacer:

```html
      <a href="/ui/settings">Settings</a>
```

and in the tabbar, after the Insights entry:

```html
    {# A fourth entry. Settings was a quiet link at the foot of Insights, which
       is unreachable on a phone by anyone who does not already know it is
       there — and after #52 it is where a person goes to see whose base this
       is and to revoke what is reading it. #}
    <a href="/ui/settings">
      <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><circle cx="10" cy="10" r="2.5"/><path d="M10 3v2M10 15v2M3 10h2M15 10h2M5.4 5.4l1.4 1.4M13.2 13.2l1.4 1.4M14.6 5.4l-1.4 1.4M6.8 13.2l-1.4 1.4"/></svg>Settings</a>
```

- [ ] **Step 4: Make room for a fourth tab**

In `assets/css/50-phone.css`, find the `.tabbar` rule. If it sizes entries by a
fixed count, change it to distribute evenly:

```css
.tabbar a { flex: 1 1 0; min-width: 0; }
```

Open the page at 375px wide and confirm four labels fit without wrapping. If
they do not, shorten "Settings" to a gear alone with an `aria-label`, rather
than letting a label wrap.

- [ ] **Step 5: Carry the account to the settings template**

In `src/web/ui.rs`, add to `SettingsTemplate`:

```rust
    /// Who is looking at this base.
    ///
    /// Every user has held their own database and their own collection since
    /// #52, and no page said so. The email where the identity provider gave
    /// one, because that is the name a person recognises as theirs; the
    /// subject otherwise, because a stable identifier beats no answer.
    account: String,
```

and in the handler that builds it, alongside the existing `judge_pending`:

```rust
        account: identity
            .email
            .clone()
            .unwrap_or_else(|| identity.subject.clone()),
```

Confirm the handler's extractor name — it may take a `Tenant` rather than an
`Identity` directly; if so, read the identity from the tenant, and if the tenant
does not carry one, add `identity: crate::auth::Identity` to the handler
signature. The extractor is already implemented and every other authenticated
handler can take it.

- [ ] **Step 6: Say it on the page**

In `src/web/templates/settings.html`, directly under the crumb:

```html
{# Said plainly, on the page where account things live. Every user has held
   their own database and their own collection since #52; a person pasting
   their own notes into a server somebody else runs has no way to know that
   from anywhere else in the application. #}
<p class="muted">Signed in as <span class="mono">{{ account }}</span>. This base
  is yours alone — no other account can search it.</p>
```

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test --lib web::ui::tests::settings_is_reachable_and_names_the_account`
Expected: PASS

- [ ] **Step 8: Run the suite**

Run: `cargo test --lib`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
cargo fmt
git add src/web/templates/ src/web/ui.rs assets/css/
git commit -m "feat: settings reaches the phone and names the account"
```

---

### Task 10: A dead control says why, and a long confirmation gets shorter

Capture and Ask disable themselves on an empty box and say nothing about it; a
permanently dead button with no stated reason is indistinguishable from a broken
one. The box never says that typing is already searching — the hint discusses
phrasing, which is not the thing a first-time user does not know. Attach names
its accepted types only in a `title`. And the feedback purge asks one forty-word
question, which is a sentence nobody finishes before pressing a button.

**Files:**
- Modify: `src/web/templates/workspace.html` (verb buttons, Attach, the hint)
- Modify: `src/web/templates/settings.html` (purge confirmation)
- Test: `src/web/workspace.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `WorkspaceTemplate.held: bool` from Task 1.

- [ ] **Step 1: Write the failing test**

```rust
    /// A button that is dead on arrival and says nothing about why reads as a
    /// broken application rather than as a control waiting for its input.
    #[tokio::test]
    async fn a_disabled_verb_says_what_it_is_waiting_for() {
        let html = workspace("/ui").await;
        assert!(
            html.contains(r#"title="Type or attach something first""#),
            "the disabled verb names what it wants"
        );
        assert!(
            html.contains("A .txt file, a PDF"),
            "and Attach names its types where a finger can read them, not only \
             in a tooltip"
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
        assert!(
            html.contains("Typing searches"),
            "the one thing a first-time user cannot deduce from the page"
        );
    }
```

- [ ] **Step 2: Run both tests to verify they fail**

Run: `cargo test --lib web::workspace::tests::a_disabled_verb_says_what_it_is_waiting_for web::workspace::tests::the_box_says_that_typing_is_already_searching`
Expected: FAIL, both.

- [ ] **Step 3: Give the verbs a reason**

In `src/web/templates/workspace.html`, add to both verb buttons:

```html
    {% if ask_enabled && held %}
    <button class="btn btn-accent" type="button" data-verb="ask" disabled
            title="Type or attach something first">Ask</button>
    {% endif %}
    <button class="btn" type="button" data-verb="capture" disabled
            title="Type or attach something first">Capture</button>
```

app.js already clears `disabled` when the box has content; leaving the `title`
in place is harmless and correct — it describes the disabled state, which is the
only state that renders it.

- [ ] **Step 4: Let Attach name its types visibly**

Under the verb row, add a line that renders the same sentence the `title`
carries, so it survives on a touch screen:

```html
    {# The accepted types, in the page rather than only on hover. The `title`
       above stays for a pointer; this is the half a finger can read, and the
       phone is where the camera path lives — the one door most likely to be
       used by someone who has never seen this page on a desktop. #}
    <span class="muted hint attach-types">{% if vision_enabled %}A .txt file, a PDF or an image{% else %}A .txt file or a PDF{% endif %} — or drop one anywhere on the page.</span>
```

- [ ] **Step 5: Say that typing searches**

Extend the `held` branch of the `box-hint` paragraph from Task 2:

```html
    {%- if held -%}
    Typing searches as you go. A sentence or a whole paragraph finds more than
    keywords do — paste what you are looking at.
    {%- else -%}
```

- [ ] **Step 6: Shorten the purge confirmation**

In `src/web/templates/settings.html`, replace the `onsubmit` string:

```html
<form method="post" action="/ui/ops/feedback/purge"
      onsubmit="return confirm('Delete every recorded search and question, and every verdict given on one? The judged ones cannot be recovered — they are what the retrieval figure is measured from.')">
```

- [ ] **Step 7: Run both tests to verify they pass**

Run: `cargo test --lib web::workspace::tests::a_disabled_verb_says_what_it_is_waiting_for web::workspace::tests::the_box_says_that_typing_is_already_searching`
Expected: PASS

- [ ] **Step 8: Run the whole suite**

Run: `cargo test --lib`
Expected: PASS

- [ ] **Step 9: Walk the app end to end**

Run the server against a scratch database with nothing in it. Confirm, in order:
the empty workspace offers Capture alone and says what engram is and whose the
base is; a paste produces a receipt with a live queue row that stops moving when
the work settles; the reloaded page now offers Ask and the shortcuts; searching
for the paste in different words finds it; Insights shows the memory above and
the machine behind a closed disclosure; Settings is reachable from the phone
tabbar and names the account.

- [ ] **Step 10: Commit**

```bash
cargo fmt
git add src/web/templates/ src/web/workspace.rs
git commit -m "feat: dead controls say what they want, and the box admits it is searching"
```
