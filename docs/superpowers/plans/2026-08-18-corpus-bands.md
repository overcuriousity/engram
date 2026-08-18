# Corpus Bands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render the corpus page as bands of source beside what was written from each, with unclaimed passages red and individually re-readable — and stop the nav changing width between pages.

**Architecture:** A pure `corpus_view::bands()` cuts the source wherever the set of artifacts claiming it changes; `corpus_detail` maps those bands onto view models and the template renders them as a two-column grid. The re-read endpoint narrows from "every gap" to "the window holding this line". The nav moves out of `.shell` into its own full-bleed header.

**Tech Stack:** Rust, axum, Askama templates (`src/web/templates/`), htmx, SQLite via sqlx.

**Spec:** `docs/superpowers/specs/2026-08-18-corpus-bands-design.md`

## Global Constraints

- Branch `fix/ui-corpus-view` in the worktree at `.claude/worktrees/ui-corpus`, based on PR #20's `fix/dedupe-false-disagreements`. Do not work in the main checkout.
- Run `cargo test` before every commit; `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before the final commit of each task.
- Web-layer tests live in `mod tests` inside `src/web/ui.rs` and use the helpers already there: `app_with_session()`, `app_session_and_core()`, `get_body(&app, &cookie, uri)`, `form(uri, &cookie, body)`, `body_of(res)`. Do not add a new harness.
- `corpus_view.rs` tests use its own local `a_corpus(raw)` helper (`src/web/corpus_view.rs:100`).
- The colour scheme is deliberate. Add the one red for unclaimed bands; change no existing palette variable.
- Never rewrite stored `raw_text` or stored artifact text.
- Comment style: this codebase explains *why* in prose above the code. Match it; never restate the code.
- Existing tests that assert on removed markup get updated, never deleted, unless the behaviour itself is gone.

---

### Task 1: The nav stops taking its width from the page

**Files:**
- Modify: `src/web/templates/layout.html:27-58`
- Modify: `assets/app.css:111-120`
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Produces: `nav.top` is wrapped in `<header class="topbar">` which sits **outside** `<div class="shell">`. `.topbar` is full-bleed; `.topbar > nav.top` is capped at `110rem`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/web/ui.rs`:

```rust
    #[tokio::test]
    async fn the_nav_is_the_same_width_on_every_page() {
        let (app, cookie) = app_with_session().await;
        for uri in ["/ui/capture", "/ui/search", "/ui/ops"] {
            let page = get_body(&app, &cookie, uri).await;
            let bar = page.find(r#"class="topbar""#).expect("a top bar");
            let shell = page.find(r#"class="shell"#).expect("a shell");
            assert!(
                bar < shell,
                "the nav must sit outside the shell, or it inherits that page's \
                 measure and moves as you navigate: {uri}"
            );
        }
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib web::ui::tests::the_nav_is_the_same_width -- --nocapture`
Expected: FAIL — there is no `topbar`, the nav is inside the shell.

- [ ] **Step 3: Move the nav out of the shell**

In `src/web/templates/layout.html`, the `<body>` currently opens with the shell and puts `{% block nav %}` inside it. Restructure so the header is a sibling that comes first:

```html
<body>
  {# Chrome, not content. Inside `.shell` the nav took whichever measure the
     page had chosen — 60rem on Capture, 110rem on Search — so it changed width
     as you navigated and its bottom border stopped at a different place on
     every page. Out here the border spans the window and the row inside it is
     one width everywhere. #}
  {% block nav %}
  <header class="topbar">
    <nav class="top">
      ... the existing nav contents, unchanged ...
    </nav>
  </header>
  {% endblock %}
  <div class="{% block shell_class %}shell{% endblock %}">
    {% block content %}{% endblock %}
  </div>
```

Move the whole existing `<nav class="top">…</nav>` inside the new `<header>` verbatim — the brand link, the three destinations, the Judge badge, the spacer and the sign-out form. Keep the `{% block nav %}` wrapper around the header so `login.html` (which overrides it) still works; check `grep -n "block nav" src/web/templates/*.html` before editing and keep every override valid.

- [ ] **Step 4: Give it a constant measure**

In `assets/app.css`, immediately before the `.shell` rule at line 111:

```css
/* Full-bleed, so the rule under the header runs the width of the window
   instead of stopping at whatever measure the page below happens to use. The
   row inside it takes the wider of the two measures: it lines up with content
   on the pages that are that wide, and simply runs wider than the reading
   column on the pages that are not. Always the same width beats sometimes
   lining up. */
.topbar { border-bottom: 1px solid var(--color-border); margin-bottom: 1.5rem; }
.topbar > nav.top { max-width: 110rem; margin: 0 auto; padding: 0.75rem 1rem; border-bottom: none; }
```

The existing `nav.top` rule at `assets/app.css:112-115` carries `border-bottom` and `padding: 0.75rem 0` and `margin-bottom: 1.5rem`; those move to `.topbar` as above, so delete them from `nav.top` and leave its `display:flex; gap; align-items` alone.

- [ ] **Step 5: Run the test and the suite**

Run: `cargo test`
Expected: PASS. If a test asserted the nav appears after `class="shell"`, update it — that ordering is exactly what changed.

- [ ] **Step 6: Commit**

```bash
git add src/web/templates/layout.html assets/app.css src/web/ui.rs
git commit -m "fix(ui): stop the nav taking its width from the page under it

Inside .shell the nav inherited whichever measure the page had chosen, so it
was 60rem on Capture and 110rem on Search and moved as you navigated — and the
rule under it stopped at a different place each time. It is chrome; it now has
its own width and its border spans the window."
```

---

### Task 2: `bands()`

**Files:**
- Modify: `src/web/corpus_view.rs` (add to the module and its `mod tests`)
- Test: `src/web/corpus_view.rs` (`mod tests`)

**Interfaces:**
- Consumes: `CorpusLine { number: i64, text: String, in_span: bool }` (`corpus_view.rs:13`), `crate::store::artifacts::CorpusSpan { start_line, end_line }`.
- Produces:

```rust
pub struct Band {
    pub from: i64,
    pub to: i64,
    pub lines: Vec<CorpusLine>,
    /// Ids of the artifacts claiming these lines, in the order given. Empty
    /// means nothing claims them, which is what makes this a gap.
    pub artifact_ids: Vec<String>,
}
impl Band { pub fn gap(&self) -> bool }

pub fn bands(
    raw_text: &str,
    spans: &[(String, CorpusSpan)],
    highlight: Option<(i64, i64)>,
) -> Vec<Band>
```

Task 3 calls `bands()` and maps `artifact_ids` onto `ArtifactView`s.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/web/corpus_view.rs`:

```rust
    fn span(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan { start_line: a, end_line: b }
    }

    /// `(from, to, ids)` for each band, which is what every case below asserts.
    fn shape(bs: &[Band]) -> Vec<(i64, i64, Vec<&str>)> {
        bs.iter()
            .map(|b| {
                (
                    b.from,
                    b.to,
                    b.artifact_ids.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn adjacent_spans_are_two_bands() {
        let out = bands(
            "a\nb\nc\nd",
            &[("x".into(), span(1, 2)), ("y".into(), span(3, 4))],
            None,
        );
        assert_eq!(shape(&out), vec![(1, 2, vec!["x"]), (3, 4, vec!["y"])]);
    }

    #[test]
    fn an_overlap_is_its_own_band() {
        // Three bands, not a merge and not a tie-break: the middle really is
        // claimed by both, and saying so is the only honest arrangement.
        let out = bands(
            "a\nb\nc\nd\ne",
            &[("x".into(), span(1, 3)), ("y".into(), span(3, 5))],
            None,
        );
        assert_eq!(
            shape(&out),
            vec![(1, 2, vec!["x"]), (3, 3, vec!["x", "y"]), (4, 5, vec!["y"])]
        );
    }

    #[test]
    fn a_run_nothing_claims_is_a_gap_band() {
        let out = bands("a\nb\nc\nd", &[("x".into(), span(1, 2))], None);
        assert_eq!(shape(&out), vec![(1, 2, vec!["x"]), (3, 4, vec![])]);
        assert!(!out[0].gap());
        assert!(out[1].gap());
    }

    #[test]
    fn a_gap_at_the_head_is_an_ordinary_band() {
        let out = bands("a\nb\nc", &[("x".into(), span(3, 3))], None);
        assert_eq!(shape(&out), vec![(1, 2, vec![]), (3, 3, vec!["x"])]);
    }

    #[test]
    fn blank_lines_between_two_spans_join_the_band_before_them() {
        // A red sliver between two paragraphs would be noise with nothing to
        // re-read: there is no content there to have missed.
        let out = bands(
            "a\n\n\nb",
            &[("x".into(), span(1, 1)), ("y".into(), span(4, 4))],
            None,
        );
        assert_eq!(shape(&out), vec![(1, 3, vec!["x"]), (4, 4, vec!["y"])]);
    }

    #[test]
    fn a_corpus_nothing_was_written_from_is_one_gap() {
        let out = bands("a\nb\nc", &[], None);
        assert_eq!(shape(&out), vec![(1, 3, vec![])]);
    }

    #[test]
    fn one_artifact_over_the_whole_document_is_one_band() {
        let out = bands("a\nb\nc", &[("x".into(), span(1, 3))], None);
        assert_eq!(shape(&out), vec![(1, 3, vec!["x"])]);
    }

    #[test]
    fn a_span_past_the_end_does_not_invent_lines() {
        let out = bands("a\nb", &[("x".into(), span(1, 9))], None);
        assert_eq!(shape(&out), vec![(1, 2, vec!["x"])]);
        assert_eq!(out[0].lines.len(), 2);
    }

    #[test]
    fn the_highlight_marks_its_lines_wherever_they_fall() {
        // The `?from=&to=` deep link an artifact's "open at these lines" uses.
        let out = bands("a\nb\nc", &[("x".into(), span(1, 3))], Some((2, 3)));
        let marked: Vec<i64> = out[0]
            .lines
            .iter()
            .filter(|l| l.in_span)
            .map(|l| l.number)
            .collect();
        assert_eq!(marked, vec![2, 3]);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::corpus_view`
Expected: FAIL to compile — `Band` and `bands` do not exist.

- [ ] **Step 3: Implement**

Add to `src/web/corpus_view.rs`, after `slice()`:

```rust
/// One stretch of the source, and which artifacts claim to have been written
/// from it.
pub struct Band {
    pub from: i64,
    pub to: i64,
    pub lines: Vec<CorpusLine>,
    /// Ids of the artifacts claiming these lines, in the order given. Empty
    /// means nothing claims them, which is what makes this a gap.
    pub artifact_ids: Vec<String>,
}

impl Band {
    /// Nothing was written from these lines. The whole point of banding: a
    /// passage the base cannot answer a question about, and can be told to
    /// read again.
    pub fn gap(&self) -> bool {
        self.artifact_ids.is_empty()
    }
}

/// Cut the source wherever the set of artifacts claiming it changes.
///
/// The page's central arrangement: a band of source beside what came of it.
/// Overlaps are their own bands rather than merged — where two artifacts both
/// claim a line, both are shown against it, which is the truth and needs no
/// tie-break.
///
/// `highlight` is the `?from=&to=` deep link, marked on whatever lines it
/// falls across; banding must not cost the page its "open at these lines".
pub fn bands(
    raw_text: &str,
    spans: &[(String, CorpusSpan)],
    highlight: Option<(i64, i64)>,
) -> Vec<Band> {
    let all: Vec<&str> = raw_text.lines().collect();
    if all.is_empty() {
        return Vec::new();
    }

    let claiming = |n: i64| -> Vec<String> {
        spans
            .iter()
            .filter(|(_, s)| s.start_line <= n && n <= s.end_line)
            .map(|(id, _)| id.clone())
            .collect()
    };

    let mut out: Vec<Band> = Vec::new();
    for (i, text) in all.iter().enumerate() {
        let number = i as i64 + 1;
        let mut ids = claiming(number);

        // A blank line claims nothing and means nothing: left to itself it
        // would cut a band in two, or open a red sliver between two paragraphs
        // with no content in it to have missed. It continues whatever band it
        // follows, and at the head of a document it starts one.
        if text.trim().is_empty()
            && let Some(last) = out.last()
        {
            ids = last.artifact_ids.clone();
        }

        let line = CorpusLine {
            number,
            text: (*text).to_string(),
            in_span: highlight.is_some_and(|(f, t)| number >= f && number <= t),
        };

        match out.last_mut() {
            Some(b) if b.artifact_ids == ids => {
                b.to = number;
                b.lines.push(line);
            }
            _ => out.push(Band {
                from: number,
                to: number,
                lines: vec![line],
                artifact_ids: ids,
            }),
        }
    }
    out
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::corpus_view`
Expected: PASS, including the pre-existing `slice()` tests, which this does not touch.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo test
git add src/web/corpus_view.rs
git commit -m "feat(corpus): cut the source where the artifacts claiming it change

The arrangement the page needs: a stretch of source beside what came of it,
and where nothing came of it, a stretch that says so. Overlaps are their own
bands rather than merged — two artifacts claiming one line is the truth, and
showing both needs no tie-break."
```

---

### Task 3: The corpus page renders bands

**Files:**
- Modify: `src/web/ui.rs` (`CorpusTemplate`, `corpus_detail` at line 1153, add `BandView`)
- Modify: `src/web/templates/corpus.html:89-134`
- Modify: `assets/app.css` (add the band grid rules)
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Consumes: `corpus_view::bands()` and `corpus_view::Band` from Task 2.
- Produces: `CorpusTemplate` loses `lines`, `artifacts` and `uncovered`, and gains:

```rust
struct BandView {
    from: i64,
    to: i64,
    lines: Vec<crate::web::corpus_view::CorpusLine>,
    artifacts: Vec<ArtifactView>,
    gap: bool,
    /// For a gap band, the lines a re-read would actually cover: the whole
    /// window holding this passage, which is wider than the passage. `None`
    /// when no window holds it and there is nothing to offer.
    reread: Option<String>,
}
```

plus `bands: Vec<BandView>` and `coverage: Option<String>`. Task 4 relies on the gap band's form posting `from={{ b.from }}`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn the_corpus_page_puts_each_passage_beside_what_came_of_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("band"), "the page is banded: {page}");
        // The old two-lists arrangement is gone.
        assert!(!page.contains("Raw corpus"), "{page}");
        assert!(!page.contains("<h3>Artifacts</h3>"), "{page}");
        // Every line keeps the anchor an artifact's "open at these lines" uses.
        assert!(page.contains(r#"id="L1""#), "{page}");
    }

    #[tokio::test]
    async fn an_unclaimed_passage_is_a_gap_band_with_its_own_button() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        // Pull every span back onto line 1, leaving the rest of the document
        // claimed by nobody. Written straight to the column because nothing in
        // the store edits a span — synthesis computes it and that is the only
        // writer, which is right everywhere except here.
        sqlx::query(r#"UPDATE artifacts SET corpus_span = '{"start_line":1,"end_line":1}' WHERE corpus_id = ?"#)
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("band-gap"), "the unclaimed run is red: {page}");
        assert!(
            page.contains(r#"name="from""#),
            "a gap band carries a re-read button naming its first line: {page}"
        );
    }

    #[tokio::test]
    async fn a_restored_corpus_is_not_banded() {
        // Its "source" is its own artifacts joined back together, so a span
        // into it is a claim the arrangement cannot support.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        sqlx::query("UPDATE corpora SET restored_at = 1 WHERE id = ?")
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains("Placeholder source"), "{page}");
        assert!(!page.contains("band-gap"), "{page}");
    }

    #[tokio::test]
    async fn the_page_states_the_coverage_the_recent_list_warned_about() {
        // The two measures answer different questions and can disagree: every
        // line claimed, and still only half the wording carried. Following the
        // warning must not land on a page with nothing to see.
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        core.store.set_corpus_coverage(&out.id, Some(0.55)).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains(r#"id="uncovered""#), "the anchor still lands: {page}");
        assert!(page.contains("55%"), "{page}");
    }
```

Both fixtures write SQL directly rather than through a store method, because
neither edit has a caller outside these tests: synthesis is the only writer of
a span, and only the vector-store repair marks a corpus restored. `Store.pool`
is `pub` and the migrate tests in `src/store/mod.rs` set up fixtures the same
way. `corpus_span` is stored as the JSON of `CorpusSpan` (`artifacts.rs:186`).

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::ui::tests::the_corpus_page_puts_each_passage`
Expected: FAIL — the page still renders "Raw corpus" and a separate artifact list.

- [ ] **Step 3: Build the view models in the handler**

In `src/web/ui.rs`, replace the body of `corpus_detail` (lines 1159-1213) so it bands instead of listing. Delete the `uncovered` block and the flat `lines` block:

```rust
    let s = st.core.store.get_corpus(&cid).await?;
    let chunks = st.core.store.artifacts_for_corpus(&cid).await?;
    let restored = s.restored_at.is_some();

    // A restored placeholder's text is its own artifacts joined back together,
    // so a span into it points at the artifact rather than at a source. It
    // keeps the flat rendering, and the warning above already says why.
    let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = if restored {
        Vec::new()
    } else {
        chunks
            .iter()
            .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
            .collect()
    };

    let by_id: std::collections::HashMap<&str, &crate::store::artifacts::Chunk> =
        chunks.iter().map(|c| (c.id.as_str(), c)).collect();
    let segments = st.core.store.segments_for_corpus(&cid).await?;

    let bands: Vec<BandView> = crate::web::corpus_view::bands(
        &s.raw_text,
        &spans,
        range.from.map(|f| (f, range.to.unwrap_or(f))),
    )
    .into_iter()
    .map(|b| BandView {
        // What pressing the button would actually read: the window holding
        // this passage, which is wider than it. Saying only "lines 51–53" over
        // a button that reads 1–120 is a promise the button does not keep —
        // and two red bands in one window really are read together.
        reread: b.gap().then(|| {
            segments
                .iter()
                .find(|w| w.start_line <= b.from && b.from <= w.end_line)
                .map(|w| format!("reads lines {}–{}", w.start_line, w.end_line))
        }).flatten(),
        from: b.from,
        to: b.to,
        gap: b.gap(),
        artifacts: b
            .artifact_ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).map(|c| artifact_view(c)))
            .collect(),
        lines: b.lines,
    })
    .collect();

    // Stated whether or not anything is red, because the warning on Recent is
    // computed the other way: a corpus can be 55% covered with every line
    // claimed, and following that warning has to land somewhere that explains
    // itself rather than on a page with nothing marked.
    let coverage = s.coverage.map(|c| format!("{:.0}%", c * 100.0));
```

Then the remaining locals (`image`, `unread`, `note`, `meta_rows`, `exif_rows`) stay exactly as they are, and the template is constructed with `bands`, `coverage`, `lines_empty` and `restored` in place of `lines`, `artifacts` and `uncovered`.

Add `BandView` beside `UncoveredRange` in the view-model block, and update `CorpusTemplate`'s fields to match. Delete `UncoveredRange` — Task 5 removes the rest of its machinery, but the struct goes with its last use.

For a restored corpus `spans` is empty, so `bands()` returns one gap band over the whole text. That would offer a re-read of a placeholder. Guard it: when `restored`, pass `bands: Vec::new()` and let the template fall back to the flat `.raw` table it renders today.

- [ ] **Step 4: Rewrite the template**

In `src/web/templates/corpus.html`, replace the whole "Raw corpus" card, the "Never reached an artifact" section and the `<h3>Artifacts</h3>` loop (lines 89 to the end of the block) with:

```html
{% if let Some(c) = coverage %}
{# Both measures, plainly. The bands say what nothing was written from; this
   says how much of the wording survived into what was. They answer different
   questions and can disagree, and the anchor lands here when nothing is red. #}
<p class="muted" id="uncovered">{{ c }} of this capture's wording survived into an artifact.</p>
{% endif %}

{% if bands.is_empty() %}
{# A restored placeholder, or a photo not read yet: nothing to band. #}
<div class="card">
  <div class="card-head">
    <span class="card-title">{% if restored %}Restored artifacts{% else if image %}Transcription{% else %}Raw corpus{% endif %}</span>
  </div>
  {% if image && lines_empty %}
  <p class="muted">Not read yet — the photo is queued for the vision model.</p>
  {% endif %}
</div>
{% else %}
<div class="bands">
  {% for b in bands %}
  <div class="band{% if b.gap %} band-gap{% endif %}">
    <div class="band-src">
      <table>
        {% for l in b.lines %}
        <tr id="L{{ l.number }}" class="{% if l.in_span %}in{% endif %}">
          <td class="ln">{{ l.number }}</td><td>{{ l.text }}</td>
        </tr>
        {% endfor %}
      </table>
    </div>
    <div class="band-out">
      {% if b.gap %}
      <div class="band-gap-head">
        <span class="band-gap-label">lines {{ b.from }}–{{ b.to }} · nothing was written from these</span>
        {% if let Some(w) = b.reread %}
        {# The button names the window, not the band: it is wider than the red
           lines, which is what lets the model read them in context — and it is
           what actually happens, including to any other red band inside it. #}
        <form method="post" action="/ui/corpora/{{ id }}/reread"
              onsubmit="return confirm('Read this passage again? One model call. Nothing already written from this capture is replaced — what comes back is added to it.')">
          <input type="hidden" name="from" value="{{ b.from }}">
          <button class="btn btn-sm" type="submit" title="{{ w }}">Read this again — {{ w }}</button>
        </form>
        {% endif %}
      </div>
      {% endif %}
      {% for c in b.artifacts %}{% include "_artifact.html" %}{% endfor %}
    </div>
  </div>
  {% endfor %}
</div>
{% endif %}
```

`_artifact.html` iterates a variable named `c`, which the loop above binds — the same contract the old `<h3>Artifacts</h3>` loop used. Add a `lines_empty: bool` field to `CorpusTemplate` set from
`s.raw_text.trim().is_empty()`, since the flat branch no longer has a `lines`
list to test. `can_reread` is not a field: whether a band can be re-read is a
per-band fact and lives on `BandView::reread`.

- [ ] **Step 5: Style the grid**

In `assets/app.css`, beside the `.uncovered` rule added earlier (delete that rule — nothing renders it now):

```css
/* ── The corpus, band by band ────────────────────────────────────────────── */

/* One row per band: the source on the left, what was written from it on the
   right. Heights are ragged on purpose — a band is as tall as its source, and
   an eight-line passage that yielded one short artifact leaves the right cell
   mostly empty. Folding it would hide source on the page that exists to show
   source. */
.bands { display: flex; flex-direction: column; gap: 0.75rem; }
.band { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; align-items: start; }
.band > * { min-width: 0; }
.band-src {
  background: var(--color-bg-elevated); border: 1px solid var(--color-border);
  border-radius: var(--radius-md); overflow-x: auto;
}
.band-src table { border-collapse: collapse; width: 100%;
                  font-family: var(--font-mono); font-size: 0.8125rem; }
.band-src td { padding: 1px 0.5rem 1px 2rem; vertical-align: top;
               white-space: pre-wrap; overflow-wrap: anywhere; text-indent: -1.5rem; }
.band-src td.ln {
  width: 3.5rem; text-align: right; color: var(--color-fg-muted); user-select: none;
  border-right: 1px solid var(--color-border-subtle); font-variant-numeric: tabular-nums;
  padding: 1px 0.5rem; text-indent: 0;
}
.band-src tr.in td { background: var(--color-accent-dim); }

/* Red is one claim and one only: no artifact says it was written from these
   lines. Not "the wording changed" — that is the percentage above. */
.band-gap .band-src { border-color: var(--color-danger); background: var(--color-danger-dim); }
.band-gap-head {
  display: flex; gap: 0.5rem; align-items: center; justify-content: space-between;
  flex-wrap: wrap;
}
.band-gap-label { font-size: 0.8125rem; color: var(--color-danger); }

@media (max-width: 60rem) { .band { grid-template-columns: 1fr; } }
```

`--color-danger` and `--color-danger-dim` are already defined for both themes
(`assets/app.css:44` and `:79`), so this introduces no new colour.

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS. `a_corpus_shows_which_lines_were_missed_and_offers_to_read_them_again` asserts on `#uncovered`, `Read these again` and `#L3` — rewrite it against the new markup rather than deleting it; its subject (a gap is visible and re-readable) still holds.

- [ ] **Step 7: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/web/ui.rs src/web/templates/corpus.html assets/app.css
git commit -m "feat(ui): the corpus beside what was written from it

The page was two lists that said the same things in places you could not read
together — the whole source, then every artifact, then a hundred line numbers
between them naming what had been missed.

One grid now: each passage beside what came of it, and where nothing came of
it, a red band saying so with a button to read that passage again."
```

---

### Task 4: Re-read one band

**Files:**
- Modify: `src/web/ui.rs` (`reread_uncovered_ui` at line 1085)
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Consumes: the gap band's `<input name="from">` from Task 3; `crate::jobs::window::unit_target(cid, idx)`; `Store::reset_segment`, `Store::live_job`, `Store::enqueue`.
- Produces: `POST /ui/corpora/{id}/reread` takes `Form<RereadForm { from: i64 }>` and resets exactly the window holding that line.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn re_reading_one_passage_leaves_the_other_windows_alone() {
        let (app, cookie, core) = app_session_and_core().await;
        // Long enough to be several windows, so "one of them" is meaningful.
        let body = (1..=400)
            .map(|i| format!("line {i} of the document"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let segments = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(segments.len() > 1, "the fixture must span several windows");
        let target = &segments[0];

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                &format!("from={}", target.start_line),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);

        let pending = core.store.pending_segments(&out.id).await.unwrap();
        assert_eq!(
            pending.iter().map(|w| w.idx).collect::<Vec<_>>(),
            vec![target.idx],
            "exactly the window holding that line, and no other"
        );
    }

    #[tokio::test]
    async fn re_reading_a_line_in_no_window_is_not_a_500() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/corpora/{}/reread", out.id),
                &cookie,
                "from=99999",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "nothing to do is not an error");
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::ui::tests::re_reading_one_passage`
Expected: FAIL — the handler takes no form and re-reads every window holding a gap.

- [ ] **Step 3: Narrow the handler**

In `src/web/ui.rs`, replace `reread_uncovered_ui` (lines 1085 onward) with:

```rust
#[derive(serde::Deserialize)]
struct RereadForm {
    /// The first line of the band the button sits in.
    from: i64,
}

/// Read one passage again.
///
/// The window holding that line, not the line itself: a window is wider than
/// the passage, which is what lets the model read it in its surroundings
/// rather than stripped of them. One model call.
///
/// Nothing already written from this capture is replaced. What comes back is
/// added, and anything it repeats is folded by the dedupe sweep like any other
/// near duplicate.
async fn reread_uncovered_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
    Form(f): Form<RereadForm>,
) -> Result<Response> {
    let segments = st.core.store.segments_for_corpus(&cid).await?;
    let Some(w) = segments
        .iter()
        .find(|w| w.start_line <= f.from && f.from <= w.end_line)
    else {
        // A line in no window belongs to a corpus segmented before per-window
        // synthesis, or to a stale page. Nothing to read again, and nothing
        // went wrong.
        return Ok(Redirect::to(&format!("/ui/corpora/{cid}#L{}", f.from)).into_response());
    };

    // A window something is already going to read is left alone. `enqueue`
    // re-arms a conflicting row whatever state it is in, running included, so
    // pressing this twice handed the same window to a second worker: two paid
    // model calls and two sets of artifacts for one passage.
    if !st
        .core
        .store
        .live_job(
            crate::store::jobs::Stage::SegmentWindow,
            &crate::jobs::window::unit_target(&cid, w.idx),
        )
        .await?
    {
        st.core.store.reset_segment(&cid, w.idx).await?;
        st.core
            .store
            .enqueue(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(&cid, w.idx),
            )
            .await?;
    }
    Ok(Redirect::to(&format!("/ui/corpora/{cid}#L{}", f.from)).into_response())
}
```

The redirect lands on the band the button was in rather than at the top of the page — on a 978-line document, coming back to the top after pressing a button two thirds of the way down loses your place.

- [ ] **Step 4: Run the tests**

Run: `cargo test`
Expected: PASS. `a_corpus_shows_which_lines_were_missed_and_offers_to_read_them_again` posts to `/reread` with an empty body; give it a `from=` or fold it into the Task 3 rewrite.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo test
git add src/web/ui.rs
git commit -m "feat(ui): read one passage again, not all of them

One button re-read every window holding a gap: on a document with forty of
them that is forty model calls behind one click, and no way to say 'this
passage, not the others'. The button now belongs to the band it sits in and
reads the one window holding it, then comes back to that band rather than to
the top of the page."
```

---

### Task 5: Retire the token-recall ranges

**Files:**
- Modify: `src/infer/verify.rs` (remove `uncovered_ranges` and its tests)
- Modify: `src/web/ui.rs` (remove `uncovered_for`)
- Test: `src/infer/verify.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing. This task only removes what Tasks 3 and 4 stopped calling.
- Produces: `verify::content_coverage` and the private `verify::line_coverage` remain, unchanged.

- [ ] **Step 1: Prove it is dead**

Run:

```bash
grep -rn "uncovered_ranges\|uncovered_for\|UncoveredRange" src/ tests/
```

Expected: hits only in `src/infer/verify.rs` (the function and its four tests) and nothing in `src/web/`. If `src/web/ui.rs` still names any of them, Task 3 or 4 is incomplete — finish that before continuing rather than deleting a live caller.

- [ ] **Step 2: Delete the function and its tests**

In `src/infer/verify.rs`, delete `pub fn uncovered_ranges` with its doc comment, and these four tests: `the_ranges_name_the_lines_no_artifact_carried`, `adjacent_uncovered_lines_become_one_range`, `a_line_outside_every_segment_is_uncovered`, `the_ranges_and_the_fraction_agree_on_the_same_input`, `a_fully_covered_source_has_no_ranges`.

Keep `line_coverage` and `content_coverage`. Update `line_coverage`'s doc comment, which currently justifies itself as the shared pass behind the fraction *and the ranges*:

```rust
/// Which non-empty lines survived, in order, as `(line number, covered)`.
///
/// The pass behind `content_coverage`. It asks whether a line's wording
/// survived the rewrite, which is a different question from whether any
/// artifact claims the line — the corpus page asks that one, off the spans,
/// and marks its answer in the source itself.
```

In `src/web/ui.rs`, delete `uncovered_for` if Task 3 left it behind.

- [ ] **Step 3: Run the suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS with no dead-code warnings.

- [ ] **Step 4: Commit**

```bash
git add src/infer/verify.rs src/web/ui.rs
git commit -m "refactor(verify): drop the ranges nothing reads any more

Token recall calls a faithfully paraphrased line missed, which is what made
the old list a hundred single-line entries. The page marks unclaimed passages
off the spans now. The fraction stays — it answers the other question, and the
Recent list still asks it."
```

---

## Final verification

- [ ] **Everything**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

- [ ] **Walk it by hand**

Most of this is markup no test can see. With Qdrant running (`podman compose up -d qdrant`) and a corpus captured:

1. Navigate Capture → Search → Housekeeping → Corpus. The nav must not move or change width, and the rule under it must span the window on every page.
2. Open a corpus with a known gap: red bands, each with its own button, the source on the left and its artifacts beside it.
3. Press one button. It should return you to that band, mark one window pending, and leave the others alone.
4. Follow an artifact's "open at these lines" link and confirm it still lands on the right lines with them highlighted.
5. Open a corpus whose coverage is below the threshold but which has no red band — the sentence under the heading must explain why there is nothing marked.

- [ ] **Push**

```bash
git push -u origin fix/ui-corpus-view
```
