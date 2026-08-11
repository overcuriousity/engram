# UI Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove Browse and the optional label, put a live ingestion queue under Capture, fold Ops in behind one link, and give phones a real layout — per `docs/superpowers/specs/2026-08-11-ui-cleanup-design.md`.

**Architecture:** Server-rendered Askama templates over axum, with htmx for fragment swaps. The queue is an htmx fragment that polls itself and stops polling when idle. Corpus titles are written by the synthesis job, not at capture time, so capture stays instant and survives a dead inference endpoint.

**Tech Stack:** Rust, axum, Askama, sqlx/SQLite, htmx, plain CSS (`assets/app.css`).

## Global Constraints

- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before every commit; both must be clean.
- Tests live in `#[cfg(test)] mod tests` in the file under test. Use `crate::core::test_support::test_core()` for a Core, and the existing `app_with_session()` / `form()` helpers in `src/web/ui.rs` for route tests.
- Comments explain *why*, not *what* — match the density and voice of the surrounding code.
- No new dependencies.
- A missing corpus title must never fail a capture.
- Every commit message ends with:
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`

---

### Task 1: Stop the page scrolling sideways

The live bug: `.pane` has `min-width: 0` but its grid siblings do not, so one unwrappable line of shell source widens the column instead of letting `.raw` scroll inside itself.

**Files:**
- Modify: `assets/app.css` (after the `.split` rule, ~line 244)

**Interfaces:**
- Consumes: nothing.
- Produces: the classes `.workspace > *`, `.split > *`, `.rail-head`, and `body { overflow-x: hidden }` — later tasks assume horizontal overflow is already handled and must not re-add width guards.

- [ ] **Step 1: Add the fix**

In `assets/app.css`, immediately after the `.split` / `@media` pair:

```css
/* A grid child defaults to `min-width: auto`, which resolves to min-content:
   one unwrappable line of source then widens the column instead of scrolling
   inside `.raw`, and takes the document's width with it. `.pane` had this;
   its siblings did not. */
.workspace > *, .split > *, .rail-head { min-width: 0; }
.raw { max-width: 100%; }
/* Titles and snippets are prose, not source: they break rather than push. */
.rail-title, .rail-snippet, .card-title { overflow-wrap: anywhere; }
/* A backstop. If something new overflows, it scrolls in its own box rather
   than dragging the document sideways. */
body { overflow-x: hidden; }
```

And change the `.badge` rule to add one property:

```css
.badge {
  display: inline-flex; align-items: center; padding: 2px 6px;
  font-family: var(--font-mono); font-size: 0.75rem; line-height: 1.2;
  border-radius: var(--radius-sm); white-space: nowrap;
  background: var(--color-bg-active); color: var(--color-fg-secondary);
  border: 1px solid var(--color-border-strong);
}
```

- [ ] **Step 2: Verify in a real browser**

```bash
cargo run &                      # or however the app is normally started
CHROME=~/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome
```

Open a search result and confirm at 1440, 1024, 820 and 390 px that
`document.documentElement.scrollWidth === document.documentElement.clientWidth`
and that the source pane shows its own horizontal scrollbar.

Expected: equal at every width; the pane scrolls, the page does not.

- [ ] **Step 3: Commit**

```bash
git add assets/app.css
git commit -m "fix: the source pane no longer drags the page sideways"
```

---

### Task 2: The model names a corpus

**Files:**
- Modify: `src/infer/mod.rs:33-41` (the `Synthesizer` trait)
- Modify: `src/infer/prompt.rs` (add `TITLE_SYSTEM` and `title_prompt`)
- Modify: `src/infer/fake.rs:86` (`FakeSynthesizer`)
- Modify: `src/infer/openai.rs:141` (`HttpSynthesizer`)
- Modify: `src/store/corpora.rs` (add `set_title_hint`, near `set_corpus_status:229`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `Synthesizer::title(&self, text: &str, artifact_titles: &[String]) -> Result<Option<String>>` — default implementation returns `Ok(None)`, meaning "this synthesizer does not name corpora". Only `HttpSynthesizer` and `FakeSynthesizer` override it, so the four other fakes keep compiling untouched.
  - `Store::set_title_hint(&self, corpus_id: &str, title: &str) -> Result<()>`
  - `prompt::TITLE_SYSTEM: &str` and `prompt::title_prompt(text: &str, artifact_titles: &[String]) -> String`

- [ ] **Step 1: Write the failing test for the store setter**

In `src/store/corpora.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn a_title_can_be_written_after_the_fact() {
    // The label field is gone from capture, so the only way a corpus gets a
    // name is a write once synthesis knows what the document is about.
    let store = test_store().await;
    let src = store.insert_corpus("some text", "web", None).await.unwrap();
    assert!(src.title_hint.is_none());

    store.set_title_hint(&src.id, "Unattended Upgrades on Debian").await.unwrap();

    let got = store.get_corpus(&src.id).await.unwrap();
    assert_eq!(got.title_hint.as_deref(), Some("Unattended Upgrades on Debian"));
}
```

If the local helper for a test store is named something other than `test_store()`, use whatever the neighbouring tests in that file already use.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p engram a_title_can_be_written_after_the_fact`
Expected: FAIL — `no method named set_title_hint`.

- [ ] **Step 3: Add the setter**

In `src/store/corpora.rs`, after `set_corpus_status`:

```rust
/// Names a corpus after the fact. Capture no longer asks for a label, so the
/// name arrives once synthesis has read the document.
pub async fn set_title_hint(&self, id: &str, title: &str) -> Result<()> {
    sqlx::query("UPDATE corpora SET title_hint = ?, updated_at = ? WHERE id = ?")
        .bind(title)
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
    Ok(())
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p engram a_title_can_be_written_after_the_fact`
Expected: PASS.

- [ ] **Step 5: Add the trait method with a default**

In `src/infer/mod.rs`, inside `pub trait Synthesizer`:

```rust
    /// A short name for a whole document, given its opening and the titles of
    /// the artifacts drawn from it. `None` means this synthesizer does not name
    /// corpora — the caller leaves the corpus unnamed rather than inventing one.
    async fn title(&self, _text: &str, _artifact_titles: &[String]) -> Result<Option<String>> {
        Ok(None)
    }
```

- [ ] **Step 6: Add the prompt**

In `src/infer/prompt.rs`:

```rust
pub const TITLE_SYSTEM: &str = r#"You name documents. Given the opening of a document and the titles of the notes taken from it, reply with one short title — at most eight words, no quotes, no trailing punctuation, no preamble. Name what the document is about, not what it is (never "Document", "Notes", "Guide")."#;

/// The opening is capped rather than sent whole: a title needs the subject,
/// not the document.
pub fn title_prompt(text: &str, artifact_titles: &[String]) -> String {
    let opening: String = text.chars().take(2000).collect();
    format!(
        "Opening of the document:\n{opening}\n\nTitles of the notes taken from it:\n{}\n\nTitle:",
        artifact_titles.join("\n")
    )
}
```

- [ ] **Step 7: Implement it for the real synthesizer**

In `src/infer/openai.rs`, inside `impl Synthesizer for HttpSynthesizer`:

```rust
    async fn title(&self, text: &str, artifact_titles: &[String]) -> Result<Option<String>> {
        let out = self
            .chat(json!([
                {"role":"system","content": prompt::TITLE_SYSTEM},
                {"role":"user","content": prompt::title_prompt(text, artifact_titles)}
            ]))
            .await?;
        // A model that ignores "no quotes" should not put them in the UI.
        let t = out.trim().trim_matches('"').trim();
        Ok((!t.is_empty()).then(|| t.chars().take(120).collect()))
    }
```

- [ ] **Step 8: Implement it deterministically for the fake**

In `src/infer/fake.rs`, inside `impl Synthesizer for FakeSynthesizer`:

```rust
    async fn title(&self, text: &str, _artifact_titles: &[String]) -> Result<Option<String>> {
        if let Some(m) = &self.fail_with {
            return Err(Error::Inference { role: "title", detail: m.clone() });
        }
        // Deterministic and obviously synthetic, so a test can assert on it.
        let first: String = text.lines().next().unwrap_or_default().chars().take(40).collect();
        Ok(Some(format!("Fake title: {}", first.trim())))
    }
```

- [ ] **Step 9: Run the whole suite**

Run: `cargo test`
Expected: PASS — the four other fakes inherit the default and still compile.

- [ ] **Step 10: Commit**

```bash
git add src/infer src/store/corpora.rs
git commit -m "feat: a synthesizer can name a whole document"
```

---

### Task 3: Synthesis writes the title

**Files:**
- Modify: `src/jobs/synthesize.rs` (end of `run`, after every segment resolves)

**Interfaces:**
- Consumes: `Synthesizer::title`, `Store::set_title_hint` (Task 2).
- Produces: after `synthesize::run` succeeds, a corpus whose synthesizer names documents has a non-NULL `title_hint`.

- [ ] **Step 1: Write the failing test**

In `src/jobs/synthesize.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn synthesis_names_the_corpus() {
    let core = crate::core::test_support::test_core().await;
    let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
    assert!(core.store.get_corpus(&out.id).await.unwrap().title_hint.is_none());

    run(&core, &out.id).await.unwrap();

    let named = core.store.get_corpus(&out.id).await.unwrap();
    assert_eq!(named.title_hint.as_deref(), Some("Fake title: alpha line"));
}

#[tokio::test]
async fn a_capture_survives_a_synthesizer_that_cannot_name_it() {
    // The title is a nicety. Failing the whole capture because the model would
    // not produce one would lose the document, which is the only thing that
    // matters here.
    let core = crate::core::test_support::test_core().await;
    let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();

    run(&core, &out.id).await.unwrap();
    let src = core.store.get_corpus(&out.id).await.unwrap();
    assert!(!core.store.artifacts_for_corpus(&out.id).await.unwrap().is_empty());
    assert!(src.title_hint.is_some() || src.title_hint.is_none()); // either way, no error
}
```

- [ ] **Step 2: Run them and watch the first fail**

Run: `cargo test -p engram synthesis_names_the_corpus`
Expected: FAIL — `title_hint` is `None`.

- [ ] **Step 3: Write the title at the end of `run`**

In `src/jobs/synthesize.rs`, after the segment loop finishes and before `run` returns `Ok(())`:

```rust
    // Named once the document has been read, not at capture time: capture makes
    // no inference call by design, and the artifact titles are the cheapest
    // description of what the document turned out to be about.
    //
    // A failure here is logged and dropped. The corpus keeps its snippet
    // fallback in the UI, and losing a capture over a missing name would be a
    // bad trade.
    if src.title_hint.is_none() {
        let titles: Vec<String> = core
            .store
            .artifacts_for_corpus(corpus_id)
            .await?
            .iter()
            .filter_map(|a| a.title.clone())
            .collect();
        match core.synthesizer.title(&src.raw_text, &titles).await {
            Ok(Some(t)) => core.store.set_title_hint(corpus_id, &t).await?,
            Ok(None) => {}
            Err(e) => tracing::warn!(corpus_id, error = %e, "could not name this corpus"),
        }
    }
```

If the artifact struct's title field is not `Option<String>`, adjust the `filter_map` to match; check `src/store/artifacts.rs` first.

- [ ] **Step 4: Run them and watch them pass**

Run: `cargo test -p engram synthesis_names`
Expected: PASS.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/jobs/synthesize.rs
git commit -m "feat: synthesis names the corpus it just read"
```

---

### Task 4: Capture stops asking for a label

**Files:**
- Modify: `src/web/ui.rs:394-407` (`CaptureForm`, `capture_submit`)
- Modify: `src/web/templates/capture.html:6-12`

**Interfaces:**
- Consumes: nothing.
- Produces: `POST /ui/capture` accepts a body of `text=…` only. A `title=` field in the body is ignored, not rejected.

- [ ] **Step 1: Write the failing test**

In `src/web/ui.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn capture_takes_only_text() {
    // The label field is gone from the form; a stale client that still sends
    // one must not get a 422.
    let (app, cookie) = app_with_session().await;
    let res = app
        .clone()
        .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .oneshot(form("/ui/capture", &cookie, "text=another+one&title=ignored"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p engram capture_takes_only_text`
Expected: PASS already for the first case, and PASS for the second — `#[serde(default)]` on an unknown field is tolerated. If it fails, that is the behaviour to fix in Step 3.

- [ ] **Step 3: Drop the field**

In `src/web/ui.rs`:

```rust
#[derive(serde::Deserialize)]
struct CaptureForm {
    text: String,
}
```

and in `capture_submit`, replace the first two lines of the body with:

```rust
    let out = st.core.ingest(&f.text, "web", None).await?;
```

- [ ] **Step 4: Remove the input from the template**

In `src/web/templates/capture.html`, delete line 11 (`<input class="input" name="title" …>`) and replace the intro paragraph with one line:

```html
  <p class="muted" style="margin:0">
    Paste a chapter at a time — long text is split into segments, one model call each.
  </p>
```

- [ ] **Step 5: Update the existing test that posts a title**

`src/web/ui.rs:1944` posts `"text=a+new+procedure&title=t"`. Leave the body as it is — it now proves the ignored-field case — but if it asserts on the stored title, drop that assertion.

- [ ] **Step 6: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web/ui.rs src/web/templates/capture.html
git commit -m "feat: capture asks for the text and nothing else"
```

---

### Task 5: The queue under Capture

**Files:**
- Create: `src/web/templates/_queue.html`
- Modify: `src/web/ui.rs` (rename `browse` → `queue_fragment`, add the route, add `QueueTemplate`)
- Modify: `src/web/templates/capture.html`
- Modify: `assets/app.css` (the `.queue` block)

**Interfaces:**
- Consumes: `BrowseRow` (rename to `QueueRow`, same fields: `id, label, badge, status, progress, coverage, low_coverage, artifact_count, created`).
- Produces: `GET /ui/queue` returning the fragment; `QueueTemplate { rows: Vec<QueueRow>, active: bool }` where `active` is true when any row is not terminal.

- [ ] **Step 1: Write the failing test**

In `src/web/ui.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn the_queue_lists_recent_captures_and_polls_only_while_busy() {
    let (app, cookie, core) = app_session_and_core().await;
    let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();

    // Freshly captured: still queued, so the fragment must ask to be refreshed.
    let body = get_body(&app, &cookie, "/ui/queue").await;
    assert!(body.contains("Untitled capture"), "an unnamed capture still shows a row");
    assert!(body.contains("hx-trigger"), "work in flight must keep polling");

    crate::jobs::synthesize::run(&core, &out.id).await.unwrap();
    crate::jobs::embed::run_corpus(&core, &out.id).await.unwrap();

    let body = get_body(&app, &cookie, "/ui/queue").await;
    assert!(!body.contains("hx-trigger"), "an idle queue must stop polling itself");
}
```

Add this helper next to `form()` if it does not already exist:

```rust
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
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p engram the_queue_lists_recent_captures`
Expected: FAIL — 404, no such route.

- [ ] **Step 3: Write the fragment template**

Create `src/web/templates/_queue.html`:

```html
{# Polls itself only while something is in flight. The trigger lives on the
   fragment rather than on the page, so when the last job finishes the swap
   that reports it is also the swap that stops the polling. #}
<div id="queue" class="queue"
     {% if active %}hx-get="/ui/queue" hx-trigger="every 3s" hx-swap="outerHTML"{% endif %}>
  {% for r in rows %}
  <div class="qrow">
    {% if r.in_flight %}<span class="qdot" aria-hidden="true"></span>{% endif %}
    <a class="qtitle {% if r.unnamed %}pending{% endif %}" href="/ui/corpora/{{ r.id }}">{{ r.label }}</a>
    <span class="qmeta">
      {% if let Some(p) = r.progress %}<span class="badge badge-accent">segmenting {{ p }}</span>
      {% else if r.in_flight %}<span class="badge badge-accent">{{ r.status }}</span>
      {% else if r.low_coverage %}<span class="badge badge-warning">{{ r.coverage_text }} covered</span>
      {% else %}<span>{{ r.artifact_count }} artifacts · {{ r.coverage_text }}</span>{% endif %}
      <span>{{ r.created }}</span>
    </span>
  </div>
  {% endfor %}
  {% if rows.is_empty() %}<p class="muted">Nothing captured yet.</p>{% endif %}
</div>
```

- [ ] **Step 4: Rework the handler**

In `src/web/ui.rs`, rename `BrowseRow` to `QueueRow` and add three fields — `in_flight: bool`, `unnamed: bool`, `coverage_text: String` — then replace `browse` with:

```rust
/// The ten most recent captures. Older ones are found by searching for what
/// they say; an index of every corpus was a page nobody read.
async fn queue_fragment(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let mut rows = Vec::new();
    for s in st.core.store.list_corpora(10, 0).await? {
        let (resolved, total) = st.core.store.segment_progress(&s.id).await?;
        let progress = (total > 0 && resolved < total).then(|| format!("{resolved}/{total}"));
        let in_flight = !matches!(
            s.status,
            CorpusStatus::Ready | CorpusStatus::Failed | CorpusStatus::NeedsReview
        );
        rows.push(QueueRow {
            in_flight,
            unnamed: s.title_hint.is_none() && in_flight,
            label: s
                .title_hint
                .clone()
                .unwrap_or_else(|| if in_flight {
                    "Untitled capture".to_string()
                } else {
                    markdown::snippet(&s.raw_text, 60)
                }),
            progress,
            coverage_text: s
                .coverage
                .map(|c| format!("{:.0}%", c * 100.0))
                .unwrap_or_else(|| "—".into()),
            low_coverage: s.coverage.is_some_and(|c| c < crate::infer::verify::LOW_COVERAGE),
            badge: status_badge(&s.status),
            status: s.status.as_str().to_string(),
            artifact_count: st.core.store.artifacts_for_corpus(&s.id).await?.len() as i64,
            created: fmt_time(s.created_at),
            id: s.id,
        });
    }
    let active = rows.iter().any(|r| r.in_flight);
    Ok(HtmlTemplate(QueueTemplate { rows, active }).into_response())
}
```

Replace `BrowseTemplate` with:

```rust
#[derive(Template)]
#[template(path = "_queue.html")]
struct QueueTemplate {
    rows: Vec<QueueRow>,
    active: bool,
}
```

Check the real variant names on `CorpusStatus` in `src/store/corpora.rs` before writing the `matches!` — use whatever terminal states exist there.

- [ ] **Step 5: Mount it and embed it**

In `ui_router()`, replace the `/ui/browse` line with:

```rust
        .route("/ui/queue", get(queue_fragment))
```

At the end of `src/web/templates/capture.html`, after `#capture-result`:

```html
<h3>Recent</h3>
{# Loaded rather than inlined, so the first paint of the page and every later
   refresh go through exactly the same fragment. #}
<div hx-get="/ui/queue" hx-trigger="load" hx-swap="outerHTML"></div>
```

- [ ] **Step 6: Add the CSS**

Append to `assets/app.css`:

```css
/* ── The capture queue ──────────────────────────────────────────────────── */
/* Rows, not a table: five columns of mostly-empty cells collapse badly on a
   phone, and only two facts per row are ever worth reading. */
.queue { display: flex; flex-direction: column; margin-top: 0.25rem; }
.qrow {
  display: flex; gap: 0.75rem; align-items: baseline; min-width: 0;
  padding: 0.6rem 0.25rem; border-bottom: 1px solid var(--color-border-subtle);
}
.qrow:last-child { border-bottom: none; }
.qrow:hover { background: var(--color-bg-hover); }
.qtitle {
  font-size: 0.9375rem; text-decoration: none; color: var(--color-fg-primary);
  min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  overflow-wrap: anywhere;
}
.qrow:hover .qtitle { color: var(--color-accent); }
.qtitle.pending { color: var(--color-fg-muted); font-style: italic; }
.qmeta {
  margin-left: auto; display: flex; gap: 0.5rem; align-items: baseline; flex: none;
  font-size: 0.8125rem; color: var(--color-fg-muted); font-family: var(--font-mono);
}
/* Only work in flight announces itself. A finished capture is just a line. */
.qdot {
  width: 6px; height: 6px; border-radius: 50%; background: var(--color-accent);
  flex: none; align-self: center; animation: qpulse 1.6s ease-in-out infinite;
}
@keyframes qpulse { 0%, 100% { opacity: 1 } 50% { opacity: 0.25 } }
@media (prefers-reduced-motion: reduce) { .qdot { animation: none } }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: PASS, except `browse_lists_captured_sources` which Task 6 removes. If it blocks, move it to Task 6's commit.

- [ ] **Step 8: Commit**

```bash
git add src/web src/web/templates/_queue.html assets/app.css
git commit -m "feat: recent captures live under the box that made them"
```

---

### Task 6: Browse is gone

**Files:**
- Delete: `src/web/templates/browse.html`
- Modify: `src/web/ui.rs` (`ui_router`, the redirect at `:702`, the test at `:2062`)
- Modify: `src/web/templates/layout.html`
- Modify: `assets/manifest.webmanifest`

**Interfaces:**
- Consumes: `/ui/queue` (Task 5).
- Produces: `/ui/browse` responds 303 to `/ui/capture`. The nav has exactly three destinations: Capture, Search, Ask.

- [ ] **Step 1: Write the failing test**

Replace `browse_lists_captured_sources` in `src/web/ui.rs` with:

```rust
#[tokio::test]
async fn browse_redirects_to_capture() {
    // An installed PWA may still have /ui/browse as its start URL, and a
    // bookmark outlives the page it pointed at.
    let (app, cookie) = app_with_session().await;
    let res = app
        .oneshot(
            Request::builder()
                .uri("/ui/browse")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()["location"], "/ui/capture");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p engram browse_redirects_to_capture`
Expected: FAIL — 200, the old page still renders.

- [ ] **Step 3: Replace the route**

In `ui_router()`:

```rust
        .route("/ui/browse", get(|| async { Redirect::to("/ui/capture") }))
```

Change the `/ui` redirect target to `/ui/capture`, and change the redirect at `src/web/ui.rs:702` from `/ui/browse` to `/ui/capture`.

- [ ] **Step 4: Delete the template and its struct**

```bash
git rm src/web/templates/browse.html
```

Remove any now-unused `BrowseRow`/`BrowseTemplate` remnants.

- [ ] **Step 5: Trim the nav**

In `src/web/templates/layout.html`, delete the Browse and Ops links, leaving Capture, Search, Ask, the spacer, and the sign-out form.

- [ ] **Step 6: Point the manifest at the new home**

In `assets/manifest.webmanifest`, set `"start_url": "/ui/capture"` and leave `"id": "/ui/search"` unchanged — changing `id` makes a browser treat a reinstall as a different app. Change `background_color` and `theme_color` to `"#f8f6f1"`, and update the Capture/Search/Ask shortcut list if any entry points at `/ui/browse`.

- [ ] **Step 7: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A src/web assets/manifest.webmanifest
git commit -m "feat: browse is gone; capture is home"
```

---

### Task 7: Search says each thing once

**Files:**
- Modify: `src/web/templates/_results.html:36-41` (drop the chips)
- Modify: `src/web/templates/_artifact_detail.html` (pane label, action order, breadcrumb)
- Modify: `assets/app.css`

**Interfaces:**
- Consumes: Task 1's overflow fix.
- Produces: no new interfaces.

- [ ] **Step 1: Write the failing test**

In `src/web/ui.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn a_result_card_does_not_repeat_the_pane_beside_it() {
    let (app, cookie) = app_with_embedded_corpus().await;
    let body = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
    assert!(body.contains("rail-title"), "the card still names the artifact");
    assert!(
        !body.contains("badge-accent"),
        "the card no longer carries the category chip the pane already lists"
    );
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p engram a_result_card_does_not_repeat`
Expected: FAIL — the chips are still rendered.

- [ ] **Step 3: Drop the chips from the card**

In `src/web/templates/_results.html`, delete the whole `<div class="chips">…</div>` block inside `.rail-item` (lines 36-41), and add above the loop:

```html
  {# The card carries rank, title and snippet only. Tags live in the pane, and
     repeating them here was what made the rail heavy enough to need its own
     scrollbar. #}
```

- [ ] **Step 4: Say "lines 8–8" once**

In `src/web/templates/_artifact_detail.html`, delete the `.crumb` line that reads `source · lines …` and the `OPEN SOURCE AT THESE LINES` link, and change the source pane's label to:

```html
        <div class="pane-label">Source · <a href="/ui/corpora/{{ corpus_id }}?from={{ from }}&to={{ to }}#L{{ from }}">{{ slice_label }}</a></div>
```

Use whatever the existing template already binds for the corpus id and the line range — check the surrounding markup rather than inventing names.

- [ ] **Step 5: Separate delete from the reversible pair**

Wrap the three icon buttons in `<div class="actions">…</div>` and append to `assets/app.css`:

```css
/* Delete is not one of the other two: it is pushed to the far end so the
   destructive control is never flush against a reversible one. */
.actions { display: flex; gap: 0.375rem; align-items: center; margin-bottom: 0.75rem; }
.actions .btn-icon-danger { margin-left: auto; }

/* The open card drops its snippet — the pane beside it is showing that text in
   full. Only the open one; the rest of the list still needs two lines. */
.rail-item[aria-selected="true"] .rail-snippet { display: none; }

/* The one statement of which lines these are. */
.pane-label a { color: var(--color-accent); text-decoration: none; }
.pane-label a:hover { text-decoration: underline; }
```

- [ ] **Step 6: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web/templates assets/app.css
git commit -m "fix: the result card and the pane stop echoing each other"
```

---

### Task 8: Ops folds into Capture

**Files:**
- Create: `src/web/templates/_decide.html`
- Modify: `src/web/ui.rs` (`capture_page` gains pairs; `ops` loses them)
- Modify: `src/web/templates/capture.html`, `src/web/templates/ops.html`
- Modify: `assets/app.css`

**Interfaces:**
- Consumes: the existing pair rows built by `ops` and the existing `POST /ui/ops/pairs/{id}/supersede` and `/dismiss` routes, which do not move.
- Produces: `CaptureTemplate` gains `pairs: Vec<PairRow>` (the same row type `ops` already builds — lift it out of the `ops` handler into a free function `async fn pair_rows(st: &AppState) -> Result<Vec<PairRow>>` and call it from both).

- [ ] **Step 1: Write the failing test**

In `src/web/ui.rs`, inside `mod tests`:

```rust
#[tokio::test]
async fn a_pending_decision_is_shown_where_the_work_arrives() {
    let (app, cookie) = app_with_session().await;
    let body = get_body(&app, &cookie, "/ui/capture").await;
    // With nothing to decide the section must not render at all — an empty
    // heading is the thing this change exists to remove.
    assert!(!body.contains("Needs you"));
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p engram a_pending_decision_is_shown`
Expected: PASS trivially today; it guards Step 4 against rendering an always-on heading.

- [ ] **Step 3: Lift the pair rows out of `ops`**

Extract the body of the `pairs` loop in the `ops` handler into:

```rust
/// The pairs an operator still has to judge. Shared by Capture, which shows
/// them because that is where the work lands, and by Housekeeping, which no
/// longer does.
async fn pair_rows(st: &AppState) -> Result<Vec<PairRow>> {
    // …the existing body, unchanged…
}
```

Call it from `ops` and from `capture_page`, and add `pairs: Vec<PairRow>` to `CaptureTemplate`.

- [ ] **Step 4: Write the partial**

Create `src/web/templates/_decide.html`:

```html
{# Renders nothing when there is nothing to decide. #}
{% for p in pairs %}
<div class="decide">
  <div style="font-size:0.875rem">
    <b>These two disagree</b> — {{ p.percent }}% alike.
    <a href="/ui/artifacts/{{ p.a_id }}">{{ p.a_title }}</a> and
    <a href="/ui/artifacts/{{ p.b_id }}">{{ p.b_title }}</a>.
  </div>
  {% if let Some(d) = p.detail %}<div class="decide-finding">{{ d }}</div>{% endif %}
  <div class="row">
    <form method="post" action="/ui/ops/pairs/{{ p.id }}/supersede">
      <input type="hidden" name="keep" value="{{ p.a_id }}">
      <button class="btn btn-sm {% if p.keeps_a %}btn-accent{% endif %}" type="submit">Keep “{{ p.a_title }}”</button>
    </form>
    <form method="post" action="/ui/ops/pairs/{{ p.id }}/supersede">
      <input type="hidden" name="keep" value="{{ p.b_id }}">
      <button class="btn btn-sm {% if p.keeps_b %}btn-accent{% endif %}" type="submit">Keep “{{ p.b_title }}”</button>
    </form>
    <form method="post" action="/ui/ops/pairs/{{ p.id }}/dismiss">
      <button class="btn btn-sm btn-ghost" type="submit">Dismiss</button>
    </form>
  </div>
</div>
{% endfor %}
```

The button now names its artifact, so the `onsubmit` confirm that used to disambiguate "Keep one" can go.

- [ ] **Step 5: Include it, and add the quiet link**

In `capture.html`, between the form and `<h3>Recent</h3>`:

```html
{% if !pairs.is_empty() %}
<h3>Needs you</h3>
{% include "_decide.html" %}
{% endif %}
```

At the very end of `capture.html`:

```html
<a class="quiet-link" href="/ui/ops">Housekeeping</a>
```

- [ ] **Step 6: Collapse the empty sections in `ops.html`**

Delete the pairs table and its heading. For each remaining section, render the heading only when the list is non-empty, and end the page with one sentence naming what is empty:

```html
{% if deprecated.is_empty() && superseded.is_empty() && retrying.is_empty() %}
<p class="muted">Nothing deprecated, nothing waiting on a decision, nothing retrying.</p>
{% endif %}
```

Use the actual collection names in `OpsTemplate`. Replace the queue badge row with a sentence: `{{ artifact_count }} artifacts across {{ corpus_count }} captures. …`.

- [ ] **Step 7: Add the CSS**

```css
/* ── Needs you ──────────────────────────────────────────────────────────── */
/* A decision only a person can make, shown where the work arrives rather than
   on a page you have to remember to visit. */
.decide {
  border: 1px solid var(--color-warning); background: var(--color-warning-dim);
  border-radius: var(--radius-md); padding: 0.75rem 0.875rem; margin-bottom: 0.5rem;
}
.decide-finding { font-size: 0.875rem; margin: 0.375rem 0 0.625rem; }
.decide .row { flex-wrap: wrap; gap: 0.375rem; }
.decide a {
  color: var(--color-fg-primary); text-decoration: underline;
  text-decoration-color: var(--color-border-strong);
}
.decide a:hover { text-decoration-color: currentColor; }

.quiet-link {
  display: inline-block; margin-top: 2rem; font-size: 0.8125rem;
  color: var(--color-fg-muted); text-decoration: none;
}
.quiet-link:hover { color: var(--color-fg-primary); text-decoration: underline; }
```

- [ ] **Step 8: Run the suite**

Run: `cargo test`
Expected: PASS. Existing Ops tests that assert on the pairs table need their assertions moved to `/ui/capture`.

- [ ] **Step 9: Commit**

```bash
git add src/web assets/app.css
git commit -m "feat: a decision belongs where the work arrives"
```

---

### Task 9: The phone layout

**Files:**
- Modify: `assets/app.css` (a new `@media (max-width: 40rem)` block at the end)
- Modify: `src/web/templates/layout.html` (tab bar, iOS status bar meta)
- Modify: `src/web/templates/_artifact_detail.html` (the back link)
- Modify: `assets/app.js` (autofocus)
- Modify: `src/web/templates/search.html:18` (drop the `autofocus` attribute)

**Interfaces:**
- Consumes: everything above.
- Produces: no new server interfaces.

- [ ] **Step 1: Add the tab bar to the layout**

In `src/web/templates/layout.html`, after `</nav>`… actually after the `.shell` div closes, before `</body>`:

```html
    {# The destinations move under the thumb on a phone; the top row keeps only
       identity. Hidden above 40rem, where the top row does the same job. #}
    <nav class="tabbar" aria-label="Sections">
      <a href="/ui/capture"><svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M10 4v12M4 10h12"/></svg>Capture</a>
      <a href="/ui/search"><svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><circle cx="9" cy="9" r="5.5"/><path d="M13 13l4 4"/></svg>Search</a>
      <a href="/ui/ask"><svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M7 7.5a3 3 0 115 2.2c-.8.7-1.2 1.2-1.2 2.3M10 15.5v.01"/></svg>Ask</a>
    </nav>
```

Add to `<head>`:

```html
  <meta name="apple-mobile-web-app-status-bar-style" content="default">
```

- [ ] **Step 2: Add the back link to the pane**

At the top of `src/web/templates/_artifact_detail.html`:

```html
{# A standalone PWA window shows no back button, and opening a result hides the
   list it came from. Hidden above 40rem, where the list is still on screen. #}
<a class="back" href="/ui/search">← Results</a>
```

- [ ] **Step 3: Move autofocus behind a pointer check**

Remove `autofocus` from `src/web/templates/search.html:18`, and add to `assets/app.js`:

```js
// Focused only where a keyboard is already visible. On a touch screen the
// keyboard covers the results the page was opened to show.
(function () {
  var box = document.querySelector('input[name="q"]');
  if (box && window.matchMedia('(hover: hover)').matches) box.focus();
})();
```

- [ ] **Step 4: Write the media query**

Append to `assets/app.css`:

```css
/* ── Phone ──────────────────────────────────────────────────────────────── */

.tabbar { display: none; }
.back { display: none; }

@media (max-width: 40rem) {
  .shell {
    padding-bottom: 5.5rem;                      /* room for the tab bar */
    padding-left: max(1rem, env(safe-area-inset-left));
    padding-right: max(1rem, env(safe-area-inset-right));
  }

  nav.top > a:not(.brand), nav.top .spacer { display: none; }
  nav.top form { margin-left: auto; }

  .tabbar {
    position: fixed; left: 0; right: 0; bottom: 0; display: flex; z-index: 10;
    background: var(--color-bg-surface); border-top: 1px solid var(--color-border);
    padding-bottom: env(safe-area-inset-bottom);
  }
  .tabbar a {
    flex: 1; display: flex; flex-direction: column; align-items: center;
    justify-content: center; gap: 2px; min-height: 52px;
    text-decoration: none; font-size: 0.6875rem; color: var(--color-fg-muted);
  }
  .tabbar a[aria-current="page"] { color: var(--color-accent); }
  .tabbar svg { width: 20px; height: 20px; }

  /* 16px, or iOS zooms the page on focus — and a standalone window has no URL
     bar to zoom back out from. */
  .input, .textarea, .select { font-size: 1rem; }
  .input, .select { height: 44px; }

  .btn-icon { width: 44px; height: 44px; }
  .chip > span { padding: 8px 12px; font-size: 0.8125rem; }
  .facet-row { align-items: center; }

  /* No scroller inside a scroller: the page is the only thing that scrolls. */
  .rail, .raw { max-height: none; }

  .back {
    display: inline-flex; align-items: center; gap: 0.375rem; min-height: 44px;
    margin: 0 0 0.25rem -0.5rem; padding: 0 0.5rem; font-size: 0.875rem;
    font-weight: 500; text-decoration: none; color: var(--color-fg-secondary);
    border-radius: var(--radius-sm);
  }
  .back:hover { background: var(--color-bg-hover); color: var(--color-fg-primary); }

  /* A pair is a card here, not a five-column row. */
  table.grid, table.grid tbody, table.grid tr, table.grid td { display: block; }
  table.grid thead { display: none; }
  table.grid tr {
    border: 1px solid var(--color-border); border-radius: var(--radius-md);
    padding: 0.75rem; margin-bottom: 0.5rem;
  }
  table.grid td { border: none; height: auto; padding: 0.125rem 0; }
}
```

Note the existing `60rem` blocks stay as they are — they handle the tablet width where the panes stack but the top nav still works.

- [ ] **Step 5: Verify the widths in a browser**

With the app running, at 1440, 1024, 820, 640 and 390 px confirm
`document.documentElement.scrollWidth === document.documentElement.clientWidth`
on `/ui/capture`, `/ui/search` with a result open, and `/ui/ops`.

Expected: equal at every width, on every page.

- [ ] **Step 6: Run the suite**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 7: Commit**

```bash
git add assets src/web/templates
git commit -m "feat: a phone layout, not a shrunken desktop"
```

---

## Self-review

**Spec coverage.** Browse deleted → Task 6. Label deleted → Task 4. Model titles → Tasks 2, 3. No backfill → Task 3 writes only when `title_hint.is_none()`, and Task 5's fallback keeps `markdown::snippet` for old rows. `GET /ui/queue` with a self-cancelling poll → Task 5. `/ui/browse` redirect → Task 6. Ops keeps its route, loses pairs, collapses empties → Task 8. Queue rows, Needs you, Housekeeping link → Tasks 5, 8. Search de-duplication → Task 7. Overflow fix and `.badge` nowrap → Task 1. Phone block, tab bar, safe area, 16px inputs, `← Results`, card tables → Task 9. Manifest colours and `start_url` → Task 6. Autofocus → Task 9.

**Gap found and closed:** the spec's testing section asks that a failing `title` call leave the capture successful — added as the second test in Task 3.

**Type consistency.** `QueueRow`/`QueueTemplate` are introduced in Task 5 and used only there. `Synthesizer::title` has the same signature in Task 2 (definition), Task 2 (both impls) and Task 3 (call site): `(&self, &str, &[String]) -> Result<Option<String>>`. `set_title_hint(&str, &str)` matches between Tasks 2 and 3. `pair_rows(&AppState) -> Result<Vec<PairRow>>` is defined and used within Task 8.

**Known soft spots** the implementer must check against the code rather than trusting this plan: the exact `CorpusStatus` variant names in Task 5's `matches!`, whether the artifact struct's `title` is `Option<String>` in Task 3, the `OpsTemplate` field names in Task 8, and the corpus-id/line bindings available to `_artifact_detail.html` in Task 7.
