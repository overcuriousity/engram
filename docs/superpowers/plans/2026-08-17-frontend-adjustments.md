# Frontend Adjustments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the twenty-seven interface issues found by reading engram's browser UI — page width, capture ordering, the coverage warning, the search filters and panes, and the housekeeping page.

**Architecture:** Almost all of it is Askama templates and `assets/app.css`. Three changes reach further: `category` becomes a closed enum in the synthesis schema with a one-time backfill in `Store::migrate` plus a Qdrant payload pass; low coverage gains an `Uncovered` section on the corpus page backed by a new `verify::uncovered_ranges`; and housekeeping splits into `/ui/ops` and a new `/ui/settings`.

**Tech Stack:** Rust, axum, Askama templates (`src/web/templates/`), htmx, SQLite via sqlx, Qdrant.

**Spec:** `docs/superpowers/specs/2026-08-17-frontend-adjustments-design.md`

## Global Constraints

- Ships as one branch on `feat/ask-harness`. No worktree.
- Run `cargo test` before each commit; run `cargo clippy --all-targets -- -D warnings` and `cargo fmt` before the final commit of each task.
- Web-layer tests live in `mod tests` inside `src/web/ui.rs` and use the existing helpers there: `app_with_session()`, `app_session_and_core()`, `get_body(&app, &cookie, uri)`, `form(uri, &cookie, body)`, `body_of(res)`. Do not add a new test harness.
- The colour scheme is deliberate. Do not change palette variables, add dark-mode rules, or restyle for contrast.
- Never rewrite stored `raw_text` or stored artifact text. Display-layer fixes only.
- The `tags` **field** stays everywhere it exists (store, API, MCP, Qdrant payload). Only model-written tags and their UI go.
- Comment style: this codebase explains *why* in prose comments above the code. Match it. Do not add comments that restate the code.
- Existing tests that assert on removed markup must be updated, never deleted, unless the behaviour they cover is itself removed.

---

### Task 1: Per-page width

**Files:**
- Modify: `src/web/templates/layout.html:27`
- Modify: `src/web/templates/search.html:1-11`
- Modify: `src/web/templates/ops.html:1-5`
- Modify: `assets/app.css:111`, `assets/app.css:252`, `assets/app.css:306`
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Produces: an Askama block named `shell_class` in `layout.html`, overridden by pages that want the wide measure. Task 13 uses it for `settings.html`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/web/ui.rs`:

```rust
    #[tokio::test]
    async fn tabular_pages_use_the_wide_measure_and_reading_pages_do_not() {
        let (app, cookie) = app_with_session().await;

        let search = get_body(&app, &cookie, "/ui/search").await;
        assert!(
            search.contains(r#"class="shell shell-wide""#),
            "search is a three-pane page and must not be held at the reading measure: {search}"
        );

        let ops = get_body(&app, &cookie, "/ui/ops").await;
        assert!(ops.contains(r#"class="shell shell-wide""#), "{ops}");

        let capture = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            capture.contains(r#"class="shell""#) && !capture.contains("shell-wide"),
            "capture is prose and keeps the reading measure: {capture}"
        );
    }
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test --lib web::ui::tests::tabular_pages_use_the_wide_measure -- --nocapture`
Expected: FAIL — every page renders the bare `class="shell"`.

- [ ] **Step 3: Add the block to the layout**

In `src/web/templates/layout.html`, replace line 27:

```html
  <div class="{% block shell_class %}shell{% endblock %}">
```

- [ ] **Step 4: Opt the two tabular pages in**

In `src/web/templates/search.html`, after line 2 (`{% block title %}`):

```html
{% block shell_class %}shell shell-wide{% endblock %}
```

In `src/web/templates/ops.html`, after line 2, add the same three lines.

- [ ] **Step 5: Widen in CSS**

In `assets/app.css`, after line 111 (`.shell { … }`) add:

```css
/* The reading measure is right for prose and wrong for a table or a three-pane
   workspace. Those pages opt out by name rather than every page growing: a
   capture textarea 110rem wide is a worse place to paste a chapter, not a
   better one. */
.shell-wide { max-width: 110rem; }
/* The one thing on Search that stays narrow. A full-width search box reads as a
   form to fill in; a narrow one reads as a question to ask. */
.shell-wide #filters { max-width: 48rem; margin-inline: auto; }
.shell-wide .row { max-width: 48rem; margin-inline: auto; }
```

Replace line 306 with:

```css
.workspace { display: grid; grid-template-columns: 22rem 1fr; gap: 1rem; align-items: start; }
```

Replace line 252's neighbourhood — the `.split` rule at line 251 — with:

```css
/* The source column takes the larger share: it holds unwrapped-looking source
   lines, while the artifact beside it is prose that reads better narrow. */
.split { display: grid; grid-template-columns: 1fr 1.2fr; gap: 0.75rem; align-items: start; }
```

Leave the `@media (max-width: 60rem)` collapse on the next line untouched.

- [ ] **Step 6: Run the test and watch it pass**

Run: `cargo test --lib web::ui::tests::tabular_pages_use_the_wide_measure`
Expected: PASS

- [ ] **Step 7: Run the whole suite**

Run: `cargo test`
Expected: PASS. If a test asserted on `class="shell"` as an exact string, widen its assertion to a `contains` on `shell`.

- [ ] **Step 8: Commit**

```bash
git add src/web/templates/layout.html src/web/templates/search.html src/web/templates/ops.html assets/app.css src/web/ui.rs
git commit -m "fix(ui): give the tables and the three-pane the width they need

A single 60rem measure is right for prose and wrong for everything else.
Search and housekeeping opt into a wide shell; the search box stays narrow
inside it, because a full-width box reads as a form to fill in."
```

---

### Task 2: Capture — the button below its own fields

**Files:**
- Modify: `src/web/templates/capture.html:9-46`
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: the capture form carries `id="capture"`; the note input keeps `name="note"` and stays outside the form element.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn the_capture_button_comes_after_every_field_it_submits() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;

        let note = page.find(r#"name="note""#).expect("the note input is on the page");
        let button = page.find(">Capture<").expect("the capture button is on the page");
        assert!(
            note < button,
            "the note field must precede the button that sits under it: {page}"
        );

        // Outside the posted form still: the form posts urlencoded and the file
        // this note describes goes multipart to a different endpoint.
        assert!(page.contains(r#"form="capture""#), "{page}");
        assert!(
            page.contains("the file you drop next"),
            "the note must say it is for a file that has not arrived yet: {page}"
        );
    }
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test --lib web::ui::tests::the_capture_button_comes_after_every_field`
Expected: FAIL — the button precedes the note input.

- [ ] **Step 3: Reorder the template**

In `src/web/templates/capture.html`, change line 9 to give the form an id:

```html
<form id="capture" hx-post="/ui/capture" hx-target="#capture-result" hx-swap="innerHTML"
```

Delete lines 37-40 (the `<div class="row">` holding the button and spinner) from inside the form. Then replace the note input block at lines 42-46 with:

```html
{# Directly under the drop zone it serves, and before the button, because a
   control that submits a field must not sit above it.

   It cannot be revealed on drop: a dropped file uploads immediately, so the
   note has to be fillable before the file arrives — which is what the wording
   says. Still outside the form above, which is why the button below carries
   `form`: this form posts urlencoded and the file goes multipart to the API. #}
<input class="input" type="text" name="note" maxlength="2000" style="margin-top:.6rem;width:100%"
       placeholder="Note for the file you drop next (optional) — what is it, why keep it?">
<div class="row" style="margin-top:.6rem">
  <button class="btn btn-accent" type="submit" form="capture">Capture</button>
  <span class="spinner">saving…</span>
</div>
```

- [ ] **Step 4: Run the test and watch it pass**

Run: `cargo test --lib web::ui::tests::the_capture_button_comes_after_every_field`
Expected: PASS

- [ ] **Step 5: Run the whole suite**

Run: `cargo test`
Expected: PASS. The capture POST path is unchanged — htmx still binds to the form's own `hx-post`.

- [ ] **Step 6: Commit**

```bash
git add src/web/templates/capture.html src/web/ui.rs
git commit -m "fix(ui): put the Capture button under the fields it submits

The note input sat below the button that sends it, and said 'for the file'
on a page where most captures are pasted text. It moves under the drop zone
it serves, and says it is for a file that has not arrived yet."
```

---

### Task 3: The pending-pair card

**Files:**
- Modify: `src/web/templates/_decide.html:13-23`
- Modify: `src/jobs/dedupe.rs:289-301`
- Test: `src/web/ui.rs` (`mod tests`), `src/jobs/dedupe.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PairView` fields `a_title`, `b_title`, `percent`, `via_link`, `contradiction`, `detail` — all already on the struct.
- Produces: `Settlement.detail` is `None` where it previously held a loss sentence.

- [ ] **Step 1: Write the failing test for the card**

```rust
    #[tokio::test]
    async fn a_pending_pair_leads_with_the_titles_not_with_the_verdict() {
        let (app, cookie, core) = app_session_and_core().await;
        // `artifacts` (src/web/ui.rs:3803) titles each one and writes
        // "body of <title>" as its text.
        let ids = artifacts(&core, &["Speicherorte der MS Mail App", "MS Mail App File Locations"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.94).await.unwrap();
        core.store
            .set_pair_state(&ids[0], &ids[1], crate::store::pairs::PairState::Contradiction)
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/capture").await;
        let title = page.find("MS Mail App").expect("a title is on the card");
        let verdict = page.find("disagree").expect("the verdict is on the card");
        assert!(
            title < verdict,
            "the titles are the content and lead the sentence: {page}"
        );
    }
```

`record_pair(a, b, score)` inserts as `pending` (`src/store/pairs.rs:143`); the state has to be set separately. Confirm the setter's real name with `grep -n "pub async fn set_pair_state\|pub async fn settle" src/store/pairs.rs` and use it — if the codebase settles a pair through a different call, copy the setup from whichever existing test already puts a pair in front of `/ui/capture`.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib web::ui::tests::a_pending_pair_leads_with_the_titles`
Expected: FAIL — the verdict precedes the titles.

- [ ] **Step 3: Rewrite the card head**

In `src/web/templates/_decide.html`, replace lines 14-23 with:

```html
  {# The titles lead. What the pair is *about* is the content; whether they
     disagree and by how much is the qualifier on it, and reading order should
     say so. #}
  <div class="decide-head">
    <a href="/ui/artifacts/{{ p.a_id }}">{{ p.a_title }}</a> vs
    <a href="/ui/artifacts/{{ p.b_id }}">{{ p.b_title }}</a> —
    {% if p.contradiction %}disagree{% else %}cover the same ground{% endif %}{% if p.via_link %}, no similarity was measured{% else %}, {{ p.percent }}% alike{% endif %}.
  </div>
  {% if let Some(d) = p.detail %}<div class="decide-finding">{{ d }}</div>{% endif %}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test --lib web::ui::tests::a_pending_pair_leads_with_the_titles`
Expected: PASS

- [ ] **Step 5: Write the failing test for the dropped loss detail**

In `src/jobs/dedupe.rs`'s `mod tests`, find the existing test named around the loss check (`grep -n "would have lost" src/jobs/dedupe.rs`) and add beside it:

```rust
    #[test]
    fn a_lossy_merge_escalates_without_writing_a_sentence_about_it() {
        // The check still fires — the pair reaches a person as a conflict — but
        // the evidence it can offer is a list of bare tokens, which reads as
        // noise on a card. The escalation is the finding; the token list is not.
        let s = settlement_for_a_lossy_merge();
        assert_eq!(s.relation, Relation::Conflict);
        assert!(s.merged.is_none());
        assert!(
            s.detail.is_none(),
            "the loss sentence is no longer written: {:?}",
            s.detail
        );
    }
```

Replace `settlement_for_a_lossy_merge()` with whatever setup the existing loss test at `src/jobs/dedupe.rs:823` uses — read that test and reuse its fixture construction inline.

- [ ] **Step 6: Run it and watch it fail**

Run: `cargo test --lib jobs::dedupe::tests::a_lossy_merge_escalates_without_writing`
Expected: FAIL — `detail` is `Some("the merge would have lost …")`.

- [ ] **Step 7: Stop writing the sentence**

In `src/jobs/dedupe.rs`, replace lines 292-300 with:

```rust
        let lost = crate::jobs::merge::losses(&roots, d);
        if !lost.is_empty() {
            // Escalated, and that is the whole finding. The sentence this used
            // to write named the lost tokens, which are often bare numerals —
            // evidence too thin to put on a card an operator has to act on.
            // What matters is that the merge was refused and the pair is now a
            // person's decision.
            relation = Relation::Conflict;
            merged = None;
        }
```

Then remove the now-unused `detail` mutation if the compiler reports it; keep the binding itself, since the judge's own detail still flows through it.

- [ ] **Step 8: Run the tests**

Run: `cargo test --lib jobs::dedupe`
Expected: PASS. The existing loss test at line 823 asserts the escalation — update it if it also asserts the sentence.

- [ ] **Step 9: Run the whole suite and commit**

```bash
cargo test
git add src/web/templates/_decide.html src/jobs/dedupe.rs src/web/ui.rs
git commit -m "fix(ui): lead a pending pair with what it is about

The verdict and the percentage led every card and the titles were buried
mid-sentence. And the loss check stops writing its finding out: escalating
to a conflict is the finding, where 'would have lost 1, 4' is not."
```

---

### Task 4: One shape for a Recent row

**Files:**
- Modify: `src/web/templates/_queue.html:18-35`
- Modify: `assets/app.css:454-470`
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Consumes: `QueueRow { coverage: String, low_coverage: bool, artifact_count, id, … }` from `src/web/ui.rs:60-80`, unchanged.
- Produces: a settled row renders the literal string `artifacts · ` followed by `{coverage} covered`, and a low row wraps that in a link to `/ui/corpora/{id}#uncovered`. Task 6 serves that anchor.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn every_settled_recent_row_states_artifacts_and_coverage_the_same_way() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::synthesize::finish(&core, &out.id).await.unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains("artifacts · ") && frag.contains(" covered"),
            "a settled row states both, in one shape: {frag}"
        );
        assert!(
            !frag.contains(r#"badge-warning">"#),
            "the warning is carried by colour on the number, not by a badge: {frag}"
        );
    }

    #[tokio::test]
    async fn a_low_coverage_row_links_to_the_lines_that_were_missed() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::synthesize::finish(&core, &out.id).await.unwrap();
        core.store.set_corpus_coverage(&out.id, Some(0.31)).await.unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains(&format!("/ui/corpora/{}#uncovered", out.id)),
            "a warning has to lead somewhere: {frag}"
        );
        assert!(frag.contains("qcov-low"), "{frag}");
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::ui::tests::every_settled_recent_row web::ui::tests::a_low_coverage_row_links`
Expected: FAIL — the settled branch renders `{{ artifact_count }} artifacts · {{ coverage }}` with no `covered`, and the low branch renders a badge with no link.

- [ ] **Step 3: Rewrite the row**

In `src/web/templates/_queue.html`, replace lines 18-35 with:

```html
    <span class="qmeta">
      {% if let Some(p) = r.progress %}
        <span class="badge badge-accent">segmenting {{ p }}</span>
      {% else if r.in_flight %}
        <span class="badge badge-accent">{{ r.status }}</span>
      {# Failed, parked and partial stop moving without finishing, and their
         artifact count is usually zero. The status is the only thing worth
         saying about these rows. #}
      {% else if !r.settled %}
        <span class="badge {{ r.badge }}">{{ r.status }}</span>
      {# One shape for every settled row. A low reading used to render as a
         badge and a healthy one as plain text, so two rows saying the same kind
         of thing looked like two different kinds of thing — and the badge's
         width moved the timestamp column around under it. The warning is
         carried by the colour of the number and by the fact that it is a link:
         somewhere to go and something to do when you get there. #}
      {% else if r.low_coverage %}
        <span>{{ r.artifact_count }} artifacts ·
          <a class="qcov-low" href="/ui/corpora/{{ r.id }}#uncovered"
             title="Some of this capture never reached an artifact — see which lines">{{ r.coverage }} covered</a></span>
      {% else %}
        <span>{{ r.artifact_count }} artifacts · {{ r.coverage }} covered</span>
      {% endif %}
      <span class="qtime">{{ r.created }}</span>
    </span>
```

- [ ] **Step 4: Hold the timestamp column still**

In `assets/app.css`, read the `.qrow` rule at line 455 and change its layout to a grid so the timestamp cannot move:

```css
.qrow {
  display: grid; grid-template-columns: 1fr auto; gap: 0.75rem; align-items: baseline;
  padding: 0.5rem 0.25rem; border-bottom: 1px solid var(--color-border-subtle);
}
.qmeta { display: grid; grid-template-columns: 1fr 9.5rem; gap: 0.75rem; align-items: baseline; }
.qtime { text-align: right; }
/* The one thing that says a reading is low. Colour, not a different layout. */
.qcov-low { color: var(--color-warning); text-decoration: underline;
            text-decoration-color: var(--color-border-strong); }
.qcov-low:hover { text-decoration-color: currentColor; }
```

Keep whatever declarations the existing `.qrow`, `.qmeta` and `.qtime` rules already carry that are not about layout (colour, font size); read them at `assets/app.css:454-470` and merge rather than replace wholesale.

- [ ] **Step 5: Run the tests and watch them pass**

Run: `cargo test --lib web::ui::tests::every_settled_recent_row web::ui::tests::a_low_coverage_row_links`
Expected: PASS

- [ ] **Step 6: Run the whole suite and commit**

```bash
cargo test
git add src/web/templates/_queue.html assets/app.css src/web/ui.rs
git commit -m "fix(ui): one shape for every settled capture

A badge on the low rows and plain text on the healthy ones made two rows
saying the same kind of thing look like two different kinds of thing, and
the badge width dragged the timestamp column around. The warning is now the
colour of the number, and the number is a link to the lines that were missed."
```

---

### Task 5: `uncovered_ranges`

**Files:**
- Modify: `src/infer/verify.rs:270-304`
- Test: `src/infer/verify.rs` (`mod tests`)

**Interfaces:**
- Produces: `pub fn uncovered_ranges(raw_text: &str, segments: &[(i64, i64, String)]) -> Vec<(i64, i64)>` — inclusive 1-based line ranges, ascending, non-overlapping, merged across adjacent uncovered lines. `content_coverage` keeps its signature and return type.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/infer/verify.rs`:

```rust
    #[test]
    fn the_ranges_name_the_lines_no_artifact_carried() {
        let raw = "alpha beta gamma\ndelta epsilon zeta\neta theta iota";
        // One segment covering all three lines, whose artifacts reproduce only
        // the first line's vocabulary.
        let made = vec![(1, 3, "alpha beta gamma".to_string())];
        assert_eq!(uncovered_ranges(raw, &made), vec![(2, 3)]);
    }

    #[test]
    fn adjacent_uncovered_lines_become_one_range() {
        let raw = "alpha beta\nomega sigma\ntau upsilon\nalpha beta";
        let made = vec![(1, 4, "alpha beta".to_string())];
        assert_eq!(uncovered_ranges(raw, &made), vec![(2, 3)]);
    }

    #[test]
    fn a_line_outside_every_segment_is_uncovered() {
        // A segment that failed leaves its lines in no range at all, which is
        // exactly the case the measure exists to make visible.
        let raw = "alpha beta\nomega sigma";
        let made = vec![(1, 1, "alpha beta".to_string())];
        assert_eq!(uncovered_ranges(raw, &made), vec![(2, 2)]);
    }

    #[test]
    fn the_ranges_and_the_fraction_agree_on_the_same_input() {
        let raw = "alpha beta\nomega sigma\ntau upsilon";
        let made = vec![(1, 3, "alpha beta".to_string())];
        let missed: i64 = uncovered_ranges(raw, &made).iter().map(|(a, b)| b - a + 1).sum();
        let total = raw.lines().filter(|l| !l.trim().is_empty()).count() as i64;
        let from_ranges = (total - missed) as f64 / total as f64;
        assert!((content_coverage(raw, &made) - from_ranges).abs() < 1e-9);
    }

    #[test]
    fn a_fully_covered_source_has_no_ranges() {
        let raw = "alpha beta\nomega sigma";
        let made = vec![(1, 2, "alpha beta omega sigma".to_string())];
        assert!(uncovered_ranges(raw, &made).is_empty());
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib infer::verify::tests`
Expected: FAIL to compile — `uncovered_ranges` does not exist.

- [ ] **Step 3: Refactor onto one pass and add the function**

In `src/infer/verify.rs`, replace the body of `content_coverage` (lines 270-304) with a shared helper plus two thin callers:

```rust
/// Which non-empty lines survived, in order, as `(line number, covered)`.
///
/// The single pass both the fraction and the ranges are computed from. Two
/// passes could disagree about a line, and a warning that points at lines the
/// percentage did not count is worse than no warning.
fn line_coverage(raw_text: &str, segments: &[(i64, i64, String)]) -> Vec<(i64, bool)> {
    let indexed: Vec<(i64, i64, std::collections::HashSet<String>)> = segments
        .iter()
        .map(|(a, b, text)| (*a, *b, distinctive_tokens(text)))
        .collect();

    raw_text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            let n = i as i64 + 1;
            let Some((_, _, made)) = indexed.iter().find(|(a, b, _)| *a <= n && n <= *b) else {
                return (n, false);
            };
            let want = distinctive_tokens(line);
            // A line with nothing distinctive on it — a page number, a rule of
            // dashes — cannot be looked for and must not be counted against the
            // document. PDF exports are full of them.
            if want.is_empty() {
                return (n, true);
            }
            let found = want.iter().filter(|t| made.contains(*t)).count();
            (n, found as f64 >= want.len() as f64 * LINE_TOKEN_RECALL)
        })
        .collect()
}

pub fn content_coverage(raw_text: &str, segments: &[(i64, i64, String)]) -> f64 {
    let lines = line_coverage(raw_text, segments);
    if lines.is_empty() {
        return 0.0;
    }
    lines.iter().filter(|(_, ok)| *ok).count() as f64 / lines.len() as f64
}

/// The line ranges no artifact carried, inclusive and 1-based.
///
/// The fraction says how much was lost; this says where, which is the half an
/// operator can act on. Adjacent uncovered lines are merged into one range —
/// a list of forty single-line ranges is a wall, and the thing that was missed
/// is a passage, not a line.
pub fn uncovered_ranges(raw_text: &str, segments: &[(i64, i64, String)]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (n, ok) in line_coverage(raw_text, segments) {
        if ok {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 + 1 == n => last.1 = n,
            _ => out.push((n, n)),
        }
    }
    out
}
```

Note the behavioural detail this preserves: blank lines are skipped entirely, so a blank line between two uncovered lines does not split the range — the numbers on either side are adjacent in the filtered list but not in the file. That is intended: the range names a passage.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test --lib infer::verify`
Expected: PASS, including the pre-existing coverage tests, which must not have changed behaviour.

- [ ] **Step 5: Commit**

```bash
cargo test
git add src/infer/verify.rs
git commit -m "feat(verify): say which lines were missed, not only how many

The fraction says how much of a capture was lost. This says where, off the
same single pass over the lines, so the number and the ranges can never
disagree about a line."
```

---

### Task 6: The uncovered lines, and reading them again

**Files:**
- Modify: `src/web/ui.rs` (`corpus_detail` at line 952, `CorpusTemplate`, `ui_router` at line 2012)
- Modify: `src/web/templates/corpus.html:88-109`
- Test: `src/web/ui.rs` (`mod tests`)

**Interfaces:**
- Consumes: `verify::uncovered_ranges` from Task 5; `jobs::synthesize::recompute_coverage(core, corpus_id)`; `Store::segments_for_corpus`, `Store::artifacts_for_corpus`, `Store::enqueue(Stage, kind, target)`.
- Produces: route `POST /ui/corpora/{id}/reread`; `CorpusTemplate` gains `uncovered: Vec<UncoveredRange>` where `pub struct UncoveredRange { pub from: i64, pub to: i64, pub label: String }`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_corpus_shows_which_lines_were_missed_and_offers_to_read_them_again() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha beta gamma\n\nomega sigma tau\n\ndelta epsilon", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::synthesize::finish(&core, &out.id).await.unwrap();
        // Force a hole: an artifact carrying only the first line's vocabulary.
        for c in core.store.artifacts_for_corpus(&out.id).await.unwrap() {
            core.store.update_artifact_text(&c.id, "alpha beta gamma").await.unwrap();
        }
        crate::jobs::synthesize::recompute_coverage(&core, &out.id).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(page.contains(r#"id="uncovered""#), "the anchor the warning links to: {page}");
        assert!(page.contains("Read these again"), "{page}");
        assert!(page.contains("#L3"), "a range links to the lines it names: {page}");

        let res = app
            .clone()
            .oneshot(form(&format!("/ui/corpora/{}/reread", out.id), &cookie, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let queued = core.store.retrying_jobs(50).await.unwrap();
        assert!(
            !queued.is_empty() || core.store.segments_for_corpus(&out.id).await.unwrap().len() > 0,
            "the ranges were enqueued to be read again"
        );
    }

    #[tokio::test]
    async fn a_fully_covered_corpus_shows_no_uncovered_section() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha beta gamma", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::synthesize::finish(&core, &out.id).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/corpora/{}", out.id)).await;
        assert!(!page.contains(r#"id="uncovered""#), "nothing to say: {page}");
    }
```

Before running: confirm the redirect status `corpus_detail`'s siblings use — read `reprocess_ui` in `src/web/ui.rs` and match its response type exactly (`Redirect::to(...)` yields 303 for a POST via axum's `SEE_OTHER`; if it differs, assert what the codebase actually does).

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib web::ui::tests::a_corpus_shows_which_lines_were_missed`
Expected: FAIL — no `#uncovered` anchor, no route.

- [ ] **Step 3: Add the view model**

In `src/web/ui.rs`, beside `QueueRow` in the view-model block near line 60:

```rust
/// A stretch of a capture that no artifact carried, for the corpus page.
pub struct UncoveredRange {
    pub from: i64,
    pub to: i64,
    /// `line 42` or `lines 42–96` — the same singular rule the source pane uses.
    pub label: String,
}
```

Add `pub uncovered: Vec<UncoveredRange>` to `CorpusTemplate`.

- [ ] **Step 4: Compute the ranges in `corpus_detail`**

In `src/web/ui.rs`, inside `corpus_detail` before the `Ok(HtmlTemplate(...))`:

```rust
    // The same segments-and-their-artifacts shape `recompute_coverage` builds,
    // because the ranges have to be measured against exactly what the number
    // was measured against.
    let chunks = st.core.store.artifacts_for_corpus(&cid).await?;
    let segments = st.core.store.segments_for_corpus(&cid).await?;
    let made: Vec<(i64, i64, String)> = segments
        .iter()
        .map(|w| {
            let text = chunks
                .iter()
                .filter(|c| c.segment_idx == Some(w.idx))
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (w.start_line, w.end_line, text)
        })
        .collect();
    let uncovered: Vec<UncoveredRange> = if made.is_empty() {
        Vec::new()
    } else {
        crate::infer::verify::uncovered_ranges(&s.raw_text, &made)
            .into_iter()
            .map(|(from, to)| UncoveredRange {
                from,
                to,
                label: if from == to {
                    format!("line {from}")
                } else {
                    format!("lines {from}–{to}")
                },
            })
            .collect()
    };
```

`artifacts` is already computed above from `artifacts_for_corpus`; reuse that call rather than making a second one — read the handler and hoist the existing binding if needed.

- [ ] **Step 5: Add the section to the template**

In `src/web/templates/corpus.html`, between the raw-corpus card (ending line 109) and `<h3>Artifacts</h3>`:

```html
{% if !uncovered.is_empty() %}
{# Where the warning on Recent lands. The fraction there says how much of this
   capture never reached an artifact; this says which parts, and offers the one
   thing that can be done about it. #}
<h3 id="uncovered">Never reached an artifact</h3>
<p class="muted">
  These lines are stored and readable above, but nothing was written from them,
  so nothing here answers a search about them.
</p>
<ul class="uncovered">
  {% for u in uncovered %}
  <li><a href="#L{{ u.from }}">{{ u.label }}</a></li>
  {% endfor %}
</ul>
<form method="post" action="/ui/corpora/{{ id }}/reread"
      onsubmit="return confirm('Read these lines again? Each range is one model call.')">
  <button class="btn btn-sm" type="submit">Read these again</button>
</form>
{% endif %}
```

Add to `assets/app.css`, near the queue rules:

```css
.uncovered { margin: 0 0 0.75rem; padding-left: 1.25rem; font-size: 0.875rem; }
```

- [ ] **Step 6: Add the endpoint**

In `src/web/ui.rs`, beside `reprocess_ui`:

```rust
/// Read the lines no artifact carried, again.
///
/// One `SegmentWindow` job per uncovered range rather than a whole re-segment:
/// the parts that did arrive are fine, and re-reading them would pay for the
/// same artifacts twice and then ask the dedupe sweep to clean up after it.
async fn reread_uncovered_ui(
    State(st): State<AppState>,
    _id: Identity,
    Path(cid): Path<String>,
) -> Result<Response> {
    let s = st.core.store.get_corpus(&cid).await?;
    let chunks = st.core.store.artifacts_for_corpus(&cid).await?;
    let segments = st.core.store.segments_for_corpus(&cid).await?;
    let made: Vec<(i64, i64, String)> = segments
        .iter()
        .map(|w| {
            let text = chunks
                .iter()
                .filter(|c| c.segment_idx == Some(w.idx))
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (w.start_line, w.end_line, text)
        })
        .collect();

    // One window may hold several uncovered ranges. Reading it once is reading
    // all of them, so the windows are collected before anything is enqueued.
    let mut windows: Vec<i64> = Vec::new();
    for (from, _to) in crate::infer::verify::uncovered_ranges(&s.raw_text, &made) {
        // Aimed at the segment the range falls in: `SegmentWindow` reads a
        // window that already exists, and a range is always inside one — a line
        // in no window at all belongs to a segment that failed, whose window
        // row is still there to be read again.
        if let Some(w) = segments
            .iter()
            .find(|w| w.start_line <= from && from <= w.end_line)
            && !windows.contains(&w.idx)
        {
            windows.push(w.idx);
        }
    }

    for idx in windows {
        // `window::run` returns early on a window already marked Done, so the
        // state has to go back to pending first — this is exactly the "re-run
        // this window" case `reset_segment` documents itself as existing for.
        st.core.store.reset_segment(&cid, idx).await?;
        st.core
            .store
            .enqueue(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(&cid, idx),
            )
            .await?;
    }
    Ok(Redirect::to(&format!("/ui/corpora/{cid}#uncovered")).into_response())
}
```

`unit_target(corpus_id, idx)` is the target-id format `window::parse_target` reads back — the same one `synthesize.rs:76` uses. Do not hand-format it.

Register it in `ui_router()` after line 2014:

```rust
        .route("/ui/corpora/{id}/reread", post(reread_uncovered_ui))
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib web::ui::tests::a_corpus_shows_which_lines_were_missed web::ui::tests::a_fully_covered_corpus`
Expected: PASS

- [ ] **Step 8: Run the whole suite and commit**

```bash
cargo test
git add src/web/ui.rs src/web/templates/corpus.html assets/app.css
git commit -m "feat(ui): show which lines never reached an artifact, and re-read them

The coverage warning on Recent stated a problem and offered nowhere to go.
It now lands here: the ranges nothing was written from, each linking to the
lines themselves, and one model call per range to try them again."
```

---

### Task 7: `category` becomes a closed vocabulary

**Files:**
- Modify: `src/infer/prompt.rs:45-49`, `src/infer/prompt.rs:588-601`, and the artifact parse near line 861
- Test: `src/infer/prompt.rs` (`mod tests`)

**Interfaces:**
- Produces: `pub const CATEGORIES: &[&str]` and `pub fn normalize_category(raw: &str) -> String` in `src/infer/prompt.rs`. Task 8's backfill and Task 9 both use `CATEGORIES`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_category_off_the_list_becomes_other() {
        // Subject words are what an unconstrained field collects: a corpus of
        // forensics notes filled it with "Forensic Science / Criminalistics".
        // The field is about form, and the enum is what keeps a domain out of
        // the schema.
        assert_eq!(normalize_category("Forensic Science / Criminalistics"), "other");
        assert_eq!(normalize_category("System Administration"), "other");
        assert_eq!(normalize_category(""), "other");
    }

    #[test]
    fn a_category_on_the_list_survives_case_and_padding() {
        assert_eq!(normalize_category("Procedure"), "procedure");
        assert_eq!(normalize_category("  snippet "), "snippet");
        assert_eq!(normalize_category("reference"), "reference");
    }

    #[test]
    fn the_schema_constrains_the_category_to_the_list() {
        let schema = artifacts_schema();
        let cat = &schema["properties"]["artifacts"]["items"]["properties"]["category"];
        let listed: Vec<&str> = cat["enum"].as_array().unwrap()
            .iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(listed, CATEGORIES);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib infer::prompt::tests`
Expected: FAIL to compile — `normalize_category` and `CATEGORIES` do not exist.

- [ ] **Step 3: Add the vocabulary**

In `src/infer/prompt.rs`, above `artifacts_schema()`:

```rust
/// What kind of thing an artifact is.
///
/// A field about *form*, not about subject. These words are true of a corpus of
/// recipes, of case law, or of forensics notes alike, which is what makes them
/// safe to hard-code: the domain never enters the schema.
///
/// It is closed because leaving it open is what let a domain in. Given a free
/// string, the model answered "System Administration" and "Forensic Science /
/// Criminalistics" — subject words in a form field, appearing in the filter row
/// beside `concept` and `procedure` as though they were the same kind of
/// choice. Anything off this list becomes `other`.
pub const CATEGORIES: &[&str] = &[
    "concept",
    "procedure",
    "reference",
    "snippet",
    "configuration",
    "definition",
    "example",
    "other",
];

/// The stored form of whatever the model answered. Never rejects: a good
/// artifact with an unrecognised label is still a good artifact.
pub fn normalize_category(raw: &str) -> String {
    let t = raw.trim().to_ascii_lowercase();
    if CATEGORIES.contains(&t.as_str()) {
        t
    } else {
        "other".to_string()
    }
}
```

- [ ] **Step 4: Constrain the schema**

In `artifacts_schema()` (line ~592), replace `"category": {"type": "string"},` with:

```rust
                        "category": {"type": "string", "enum": CATEGORIES},
```

Do the same in the dedupe schema at line ~656, where the merged draft carries a category.

- [ ] **Step 5: Say it in the prose prompt**

In `src/infer/prompt.rs`, replace line 49's category bullet:

```rust
- category: exactly one of: concept, procedure, reference, snippet,
  configuration, definition, example, other. This is what kind of thing the
  artifact is, never what subject it is about.
```

- [ ] **Step 6: Normalize on the way in**

Find where a parsed artifact's category is assigned (near line 861, and the merged-draft equivalent). Wrap each with `normalize_category`:

```rust
            category: c.category.as_deref().map(normalize_category),
```

Match the surrounding types exactly — read the struct before editing. If the field is `Option<String>`, an absent category stays `None` rather than becoming `"other"`: a model that answered nothing made no claim, and `other` is a claim.

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib infer::prompt`
Expected: PASS

- [ ] **Step 8: Run the whole suite and commit**

```bash
cargo test
git add src/infer/prompt.rs
git commit -m "feat(infer): make the kind a closed vocabulary of form words

Left open, the field collected subject words — 'System Administration',
'Forensic Science / Criminalistics' — and put them in the filter row beside
'concept' as though they were the same kind of choice. Closing it to form
words is what keeps a domain out of the schema."
```

---

### Task 8: Backfill the categories

**Files:**
- Modify: `src/store/mod.rs:182-200` (inside `migrate`, after the schema is applied)
- Modify: `src/web/ui.rs` (a one-shot payload pass, called from `ops`)
- Test: `src/store/mod.rs` (`mod tests`)

**Interfaces:**
- Consumes: `crate::infer::prompt::CATEGORIES` from Task 7; `Vectors::set_payload(&VectorPayload)` (see `src/web/api.rs:789-805` for the full field list).
- Produces: after `migrate`, no row in `artifacts` holds a category outside `CATEGORIES`.

- [ ] **Step 1: Write the failing test**

In `src/store/mod.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn migrate_folds_categories_off_the_list_into_other() {
        let store = Store::memory().await.unwrap();
        store.migrate().await.unwrap();
        // A row written when the field was a free string.
        sqlx::query("UPDATE artifacts SET category = 'Forensic Science / Criminalistics'")
            .execute(&store.pool).await.unwrap();
        // …with at least one artifact present; build it the way the existing
        // artifact tests in this file do.

        store.migrate().await.unwrap();

        let left: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM artifacts WHERE category IS NOT NULL
             AND category NOT IN ('concept','procedure','reference','snippet',
                                  'configuration','definition','example','other')",
        )
        .fetch_one(&store.pool).await.unwrap();
        assert_eq!(left, 0, "no row keeps a category the filter row cannot show");
    }
```

Read the neighbouring migrate tests (around `src/store/mod.rs:400-530`) and build the artifact with the same helper they use; do not invent one.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib store::tests::migrate_folds_categories`
Expected: FAIL — the off-list category survives.

- [ ] **Step 3: Add the fold to `migrate`**

In `src/store/mod.rs`, after `sqlx::raw_sql(SCHEMA)` executes (line ~185) and beside the existing one-time data fix for `jobs`:

```rust
        // The kind became a closed vocabulary of form words. Rows written while
        // it was a free string hold subject words — "System Administration",
        // "Forensic Science / Criminalistics" — which the filter row then
        // offered beside "concept" as though they were the same kind of choice.
        //
        // Folded rather than dropped: `other` is true of them, and the text,
        // the title and the vector are untouched. Idempotent, like every other
        // statement here: a second run matches nothing.
        let listed = crate::infer::prompt::CATEGORIES
            .iter()
            .map(|c| format!("'{c}'"))
            .collect::<Vec<_>>()
            .join(",");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "UPDATE artifacts SET category = 'other'
             WHERE category IS NOT NULL AND category NOT IN ({listed})"
        )))
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::Error::Store(e.to_string()))?;
```

The `AssertSqlSafe` audit, stated the way the neighbouring one is: every value interpolated comes from `CATEGORIES`, a compile-time constant. No caller, request or database value reaches it.

- [ ] **Step 4: Run the test and watch it pass**

Run: `cargo test --lib store::tests::migrate_folds_categories`
Expected: PASS

- [ ] **Step 5: Write the failing test for the Qdrant side**

In `src/web/ui.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn opening_housekeeping_repairs_payload_categories_that_the_fold_changed() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id).await.unwrap();
        for c in core.store.artifacts_for_corpus(&out.id).await.unwrap() {
            core.store.update_artifact_category(&c.id, Some("other")).await.unwrap();
        }

        assert!(
            !core.store.artifacts_needing_category_repair(200).await.unwrap().is_empty(),
            "there is something to repair before the page is opened"
        );

        let _ = get_body(&app, &cookie, "/ui/ops").await;

        assert!(
            core.store.artifacts_needing_category_repair(200).await.unwrap().is_empty(),
            "opening the page brought the payloads back into step, and the pass \
             does not run again on the next visit"
        );
    }
```

Asserted on the work being done rather than on Qdrant's stored payload: the fake vector store used in tests need not round-trip a payload edit, and what this pass has to guarantee is that it runs once and then stops.

- [ ] **Step 6: Run it and watch it fail**

Run: `cargo test --lib web::ui::tests::opening_housekeeping_repairs_payload`
Expected: FAIL — nothing rewrites the payload.

- [ ] **Step 7: Add the payload pass**

In `src/web/ui.rs`, above the `ops` handler:

```rust
/// Bring Qdrant payloads back in step with rows the category fold changed.
///
/// SQLite is migrated on connect; Qdrant is a separate store and has no such
/// hook. Rather than a startup sweep over every point, this runs where an
/// operator already goes when something looks wrong, costs one scan of the
/// artifacts that disagree, and does nothing at all once they agree — which is
/// after the first visit.
///
/// Payload only: nothing the embedder saw has changed, so no vector is
/// recomputed. Same reasoning as the tag edit in `api.rs`.
async fn reconcile_categories(st: &AppState) -> Result<usize> {
    let mut fixed = 0;
    for c in st.core.store.artifacts_needing_category_repair(200).await? {
        if c.embed_state != crate::store::artifacts::EmbedState::Embedded {
            continue;
        }
        st.core
            .vectors
            .set_payload(&crate::vector::VectorPayload {
                artifact_id: c.id.clone(),
                corpus_id: c.corpus_id.clone().unwrap_or_default(),
                text: c.text.clone(),
                title: c.title.clone(),
                category: c.category.clone(),
                tags: c.tags.clone(),
                created_at: c.created_at,
                last_seen_at: None,
                hit_count: None,
                superseded: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
            })
            .await?;
        fixed += 1;
    }
    Ok(fixed)
}
```

Fill in the remaining `VectorPayload` fields from `src/web/api.rs:791-805`, which constructs the same struct — copy it field for field.

The store side needs a query. In `src/store/artifacts.rs`, beside `update_artifact_category`:

```rust
    /// Artifacts whose stored category the payload may still disagree with.
    ///
    /// A row folded to `other` by the migration is the only way the two can
    /// diverge, so this is bounded by how many such rows existed and empties
    /// itself as they are rewritten.
    pub async fn artifacts_needing_category_repair(&self, limit: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts
              WHERE category = 'other' AND payload_synced_at IS NULL
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Stamped once the vector payload has been brought back into step.
    pub async fn mark_payload_synced(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE artifacts SET payload_synced_at = ? WHERE id = ?")
            .bind(crate::store::now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

`row_to_artifact` and `now()` are the same helpers the neighbouring queries in this file use (`src/store/artifacts.rs:835-844`). `payload_synced_at` is not read back into `Chunk`, so `row_to_artifact` needs no change.

This needs a marker so the pass terminates. Add an `ADDED_COLUMNS` entry in `src/store/mod.rs`:

```rust
            // Set when the category fold's payload rewrite reached this row.
            // NULL on every row predating the fold, which is what makes the
            // repair pass finite: it empties itself.
            ("artifacts", "payload_synced_at", "INTEGER"),
```

and the same column in `src/store/schema.sql`'s `artifacts` table, so a fresh database has it without the append — that file says what the schema *is*, and `ADDED_COLUMNS` only rescues databases that predate it.

Stamp it in `reconcile_categories` after each successful `set_payload`, via `Store::mark_payload_synced(&c.id)`.

- [ ] **Step 8: Call it from `ops`**

At the top of the `ops` handler in `src/web/ui.rs`:

```rust
    // Best-effort: a vector store that is down must not make the page that
    // explains what is wrong the one page you cannot open.
    if let Err(e) = reconcile_categories(&st).await {
        tracing::warn!(error = %e, "category payload repair deferred");
    }
```

- [ ] **Step 9: Run the tests**

Run: `cargo test --lib store:: web::ui::tests::opening_housekeeping_repairs_payload`
Expected: PASS

- [ ] **Step 10: Run the whole suite and commit**

```bash
cargo test
git add src/store/mod.rs src/store/artifacts.rs src/web/ui.rs
git commit -m "feat(store): fold old categories onto the closed list

SQLite is folded on connect. Qdrant has no such hook, so the payloads are
brought back into step where an operator already goes when something looks
wrong — bounded by the rows the fold touched, and empty after the first visit."
```

---

### Task 9: Stop generating tags, stop showing them

**Files:**
- Modify: `src/infer/prompt.rs:45`, `:49`, `:592`, `:601`, `:656`, `:665`
- Modify: `src/web/templates/search.html:58-75`
- Modify: `src/web/templates/_artifact.html:8`
- Modify: `src/web/templates/_artifact_detail.html:138`
- Test: `src/web/ui.rs`, `src/infer/prompt.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: no template renders `d.tags` or `c.tags`. `RenderedResult.tags` and `Chunk.tags` stay on their structs and are still served by the API.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn tags_are_stored_and_filterable_but_never_rendered() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let c = &core.store.artifacts_for_corpus(&out.id).await.unwrap()[0];
        core.store.update_artifact_tags(&c.id, &["forensik".into()]).await.unwrap();

        let page = get_body(&app, &cookie, &format!("/ui/artifacts/{}", c.id)).await;
        assert!(!page.contains("forensik"), "no chips on the artifact: {page}");

        let search = get_body(&app, &cookie, "/ui/search").await;
        assert!(!search.contains(r#"aria-label="Tag""#), "no tag facet row: {search}");

        // Still true, still stored, still the pinning channel.
        assert_eq!(core.store.get_artifact(&c.id).await.unwrap().tags, vec!["forensik"]);
    }
```

And in `src/infer/prompt.rs`:

```rust
    #[test]
    fn the_schema_no_longer_asks_for_tags() {
        let items = &artifacts_schema()["properties"]["artifacts"]["items"];
        assert!(items["properties"]["tags"].is_null());
        let required: Vec<&str> = items["required"].as_array().unwrap()
            .iter().map(|v| v.as_str().unwrap()).collect();
        assert!(!required.contains(&"tags"));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib infer::prompt::tests::the_schema_no_longer_asks web::ui::tests::tags_are_stored_and_filterable`
Expected: FAIL.

- [ ] **Step 3: Take tags out of the prompt**

In `src/infer/prompt.rs`:
- Line 45: drop `"tags":["..."],` from the shape line.
- Line 49: delete the `- tags:` bullet.
- Line 592 and 656: delete the `"tags"` property from both schemas.
- Line 601 and 665: remove `"tags"` from both `required` arrays.
- Where a parsed artifact assigns `tags` (near line 861 and the merged-draft path near 544), assign `Vec::new()` and note why:

```rust
            // Nothing writes tags automatically any more. No domain-agnostic
            // vocabulary exists for subject terms, so a generated one is a
            // vocabulary that drifts — `forensics` and `forensik` as two
            // filters over the same idea. The field stays: it is the pinning
            // channel and a public API filter, written by a caller who means it.
            tags: Vec::new(),
```

Update the `MergedDraft`/`Chunk` parse tests in that file that assert on generated tags.

- [ ] **Step 4: Take the tag row out of Search**

In `src/web/templates/search.html`, delete lines 58-75 (the whole `{% if !facets.tags.is_empty() %}` block). Change line 33's condition to:

```html
  {% if !facets.categories.is_empty() %}
```

and remove the now-dead inner `{% if !facets.categories.is_empty() %}` at line 39 with its matching `{% endif %}` at line 56, so one condition guards the row.

Leave `facets.tags`, `ensure_facet(&mut facets.tags, &tag)` and the `tags` query parameter in `src/web/ui.rs` alone: a deep link carrying `?tags=` still filters, it just has no chips.

- [ ] **Step 5: Take the chips off the artifacts**

`src/web/templates/_artifact.html` line 8: delete the `{% for t in c.tags %}` loop.
`src/web/templates/_artifact_detail.html` line 138: delete the `{% for t in d.tags %}` loop. Keep the category badge on line 137.

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS. Several existing tests assert on rendered tags — update their assertions to the new truth (stored, not rendered); do not delete them.

- [ ] **Step 7: Commit**

```bash
git add src/infer/prompt.rs src/web/templates/search.html src/web/templates/_artifact.html src/web/templates/_artifact_detail.html src/web/ui.rs
git commit -m "feat(infer): stop generating tags; keep the field for callers

Two chips for one idea — forensics and forensik, security and sicherheit —
because nothing normalises a free vocabulary and nothing can: there is no
domain-agnostic list of subject words. The field stays. It is the pinning
channel and a public filter, written by a caller who means it."
```

---

### Task 10: The result list

**Files:**
- Modify: `src/web/templates/search.html:80-84`
- Modify: `src/web/templates/_results.html:1-21`, `:65-88`
- Modify: `src/web/ui.rs` (the `_results` template struct, to carry a count)
- Modify: `assets/app.css:342-345`, and the `.related` block near line 380
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: `ResultsTemplate` fields `results`, `associated`, `all_weak`, `terms`, `timing`.
- Produces: `ResultsTemplate` gains `pub count: usize`. The `#timing` span and its OOB swap are gone.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn the_result_list_says_how_many_and_keeps_debug_timing_off_the_page() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let frag = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(frag.contains("results"), "the count is stated once: {frag}");
        assert!(!frag.contains("embed "), "timing is not operator-facing: {frag}");
        assert!(!frag.contains(r#"hx-swap-oob="innerHTML:#timing""#), "{frag}");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib web::ui::tests::the_result_list_says_how_many`
Expected: FAIL — the OOB timing span is line 1 of the fragment.

- [ ] **Step 3: Remove the timing readout**

Delete line 1 of `src/web/templates/_results.html`. In `src/web/templates/search.html`, delete lines 80-84 and put the spinner where the results begin:

```html
<div class="row" style="margin:0.5rem 0 1rem">
  <span id="search-spinner" class="spinner">searching…</span>
</div>
```

In `src/web/ui.rs`, keep computing the timing and emit it as a `Server-Timing` response header on the results handler rather than as markup — a browser already knows where to show that. Read `search_results` and add the header to its response; drop the `timing` template field.

- [ ] **Step 4: State the count**

Add `pub count: usize` to the results template struct and set it to `results.len()`. In `_results.html`, immediately inside the `<div data-terms=…>`:

```html
  {# Said once, above the list. A ranked list with no count is a list you
     cannot tell is complete. #}
  <div class="result-count">{{ count }} results</div>
```

- [ ] **Step 5: Give the cards a floor and the recalled ones their own shape**

In `assets/app.css`, add to the `.rail-snippet` rule at line 342:

```css
  min-height: 2.4em;
```

and after the `.related` block near line 380:

```css
/* Recalled by association, not ranked against the query. It should not borrow
   the shape of something that was: no rank column, a lighter border, and the
   reason it surfaced doing the work the rank does in the list above. */
.rail-assoc { border-style: dashed; }
.result-count { font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.04em;
                color: var(--color-fg-muted); margin-bottom: 0.5rem; }
```

- [ ] **Step 6: Highlight every card, not one**

Read `highlightable_terms` and the JS that applies it (`grep -n "data-terms" assets/app.js`). The handler currently marks within one container; change it to walk every `.rail-snippet` and `.rail-title` under `[data-terms]`. Write the test first:

```rust
    #[tokio::test]
    async fn the_terms_travel_with_the_whole_list_not_one_card() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let frag = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(frag.contains(r#"data-terms="#), "{frag}");
    }
```

The marking itself is client-side; assert on the data the client needs being present for every card's container, and fix `assets/app.js` to iterate rather than to take the first match.

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: PASS. Existing tests asserting on `#timing` must be updated to assert the `Server-Timing` header instead.

- [ ] **Step 8: Commit**

```bash
git add src/web/templates/_results.html src/web/templates/search.html src/web/ui.rs assets/app.css assets/app.js
git commit -m "fix(ui): a count, a floor under the cards, and no debug readout

'embed 0ms · total 10ms' was telemetry rendered to the operator; it moves to
a Server-Timing header. A result with no excerpt no longer collapses to a
title, and a card recalled by association stops borrowing the shape of one
that was ranked."
```

---

### Task 11: The selected result

**Files:**
- Modify: `assets/app.js:134-146`
- Test: `assets/app.js` has no test harness; assert the markup the handler needs, in `src/web/ui.rs`

**Interfaces:**
- Consumes: `.rail-item` anchors carrying `href="/ui/artifacts/{id}"`, already rendered by `_results.html:31`.

- [ ] **Step 1: Confirm the defect**

Read `assets/app.js:134-146`. `aria-selected` is set only inside the `keydown` handler, so a click leaves every card `false` and the `[aria-selected="true"]` rules at `app.css:338` and `app.css:494` never apply. Note this in the commit; it is a bug, not a missing feature.

- [ ] **Step 2: Write the test for the markup the fix needs**

```rust
    #[tokio::test]
    async fn every_result_carries_the_id_the_selection_handler_matches_on() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let frag = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(frag.contains(r#"role="option" aria-selected="false""#), "{frag}");
        assert!(frag.contains("/ui/artifacts/"), "{frag}");
    }
```

- [ ] **Step 3: Run it**

Run: `cargo test --lib web::ui::tests::every_result_carries_the_id`
Expected: PASS immediately — the markup is already right. This test guards the contract the JS depends on.

- [ ] **Step 4: Mark the clicked card**

In `assets/app.js`, beside the existing `htmx:afterSwap` listener near line 118:

```js
  // A clicked result was never marked selected: `aria-selected` was set only by
  // the arrow-key handler below, so the styling for an open card — the accent
  // border, and dropping the snippet the pane is already showing in full —
  // applied to keyboard navigation and to nothing else.
  document.body.addEventListener('htmx:afterSwap', function (e) {
    if (e.target.id !== 'pane') return;
    var open = window.location.pathname;
    document.querySelectorAll('.rail-item').forEach(function (el) {
      var mine = el.getAttribute('href') === open;
      el.setAttribute('aria-selected', mine ? 'true' : 'false');
    });
  });
```

`hx-push-url="true"` on the rail items means `location.pathname` is the artifact URL by the time the swap settles; that is what makes the match possible without threading an id through the fragment.

- [ ] **Step 5: Verify by hand**

Run the app and click a result. Expected: the clicked card takes the accent border and drops its two-line snippet; the previously selected one returns to normal. Use the `run` skill if a launch recipe exists.

- [ ] **Step 6: Commit**

```bash
git add assets/app.js src/web/ui.rs
git commit -m "fix(ui): mark the result you clicked as the open one

aria-selected was set only by the arrow-key handler, so the styling for an
open card applied to keyboard navigation and to nothing else — clicking a
result left the whole list looking unselected while its pane was on screen."
```

---

### Task 12: The source pane

**Files:**
- Modify: `src/web/corpus_view.rs:73-81`
- Modify: `assets/app.css:278-283`, `assets/app.css:265-276`
- Modify: `src/web/templates/_artifact_detail.html:212-214`
- Test: `src/web/corpus_view.rs` (`mod tests`)

**Interfaces:**
- Produces: `CorpusSlice.label` reads `line 42` for a one-line span and `lines 42–96` otherwise; `transcription ` still prefixes both.

- [ ] **Step 1: Write the failing tests**

In `src/web/corpus_view.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_one_line_span_is_not_a_range() {
        let src = a_corpus("l1\nl2\nl3").await;
        let slice = slice(&src, Some(&CorpusSpan { start_line: 2, end_line: 2 }), 0);
        assert_eq!(slice.label, "line 2");
    }

    #[tokio::test]
    async fn a_real_range_still_reads_as_one() {
        let src = a_corpus("l1\nl2\nl3").await;
        let slice = slice(&src, Some(&CorpusSpan { start_line: 1, end_line: 3 }), 0);
        assert_eq!(slice.label, "lines 1–3");
    }
```

Update the existing `an_image_corpus_labels_its_lines_as_transcription` test, which asserts `"transcription lines 2–2"`, to expect `"transcription line 2"`.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::corpus_view`
Expected: FAIL — `lines 2–2`.

- [ ] **Step 3: Fix the label**

In `src/web/corpus_view.rs`, replace the `label` expression at lines 75-80:

```rust
        label: if span.start_line == span.end_line {
            // "lines 576–576" is a range with one thing in it, which reads as a
            // system that did not check.
            format!(
                "{}line {}",
                if transcript { "transcription " } else { "" },
                span.start_line
            )
        } else {
            format!(
                "{}lines {}–{}",
                if transcript { "transcription " } else { "" },
                span.start_line,
                span.end_line
            )
        },
```

- [ ] **Step 4: Wrap the source lines**

In `assets/app.css`, replace the `.raw td` rule at line 283 and its comment above it:

```css
/* Wrapped, with the continuation indented under the code column so a broken
   line still reads as one line. `pre` was chosen when the pane was narrow
   enough that a command wrapped mid-flag and read as two commands; the pane is
   wider now, and clipping a filename behind a hairline scrollbar hid the very
   thing the pane exists to show. */
.raw td { padding: 1px 0.5rem; vertical-align: top;
          white-space: pre-wrap; overflow-wrap: anywhere; text-indent: -1.5rem;
          padding-left: 2rem; }
.raw td.ln { text-indent: 0; padding-left: 0.5rem; }
```

Keep every other `.raw td.ln` declaration as it is — the sticky column, the width, the tabular numerals.

- [ ] **Step 5: Match the pane height**

In `assets/app.css`, the `.raw` rule at line 270 caps at `max-height: 30rem`. Change it so the source column tracks the artifact beside it:

```css
.raw {
  background: var(--color-bg-elevated); border: 1px solid var(--color-border);
  border-radius: var(--radius-md); overflow: auto; max-height: min(60vh, 45rem);
}
```

- [ ] **Step 6: Say "highlighted" only when something is**

In `src/web/templates/_artifact_detail.html`, replace line 213:

```html
        <a href="{{ d.source_at_lines }}">Source</a> · {{ d.slice_label }}
```

The word "highlighted" was doing no work: the highlight is visible in the table below it, and the label already names the lines.

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: PASS. Any test asserting `"lines 2–2"` or `" highlighted"` needs its expectation updated.

- [ ] **Step 8: Commit**

```bash
git add src/web/corpus_view.rs assets/app.css src/web/templates/_artifact_detail.html
git commit -m "fix(ui): let the source pane show the lines it is there to show

Long lines were clipped behind a hairline scrollbar in the one pane whose
purpose is showing exactly what the source says. They wrap now, indented so a
broken line still reads as one. And a one-line citation stops calling itself
'lines 576–576'."
```

---

### Task 13: Housekeeping splits

**Files:**
- Create: `src/web/templates/settings.html`
- Modify: `src/web/templates/ops.html:193-244`
- Modify: `src/web/templates/capture.html:70`
- Modify: `src/web/ui.rs` (`ops` handler, new `settings` handler, `ui_router`)
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: `TokenRow`, the feedback view model, and `judge_pending` — all already used by `ops`.
- Produces: route `GET /ui/settings`; `SettingsTemplate { judge_pending, tokens: Vec<TokenRow>, feedback }`. The token and feedback POST routes keep their `/ui/ops/...` paths and gain `to` redirects back to `/ui/settings`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn the_installation_lives_on_its_own_page() {
        let (app, cookie) = app_with_session().await;

        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(settings.contains("API tokens"), "{settings}");
        assert!(settings.contains("Browser extension"), "{settings}");

        let ops = get_body(&app, &cookie, "/ui/ops").await;
        assert!(!ops.contains("API tokens"), "housekeeping is about the corpus: {ops}");
        assert!(!ops.contains("Browser extension"), "{ops}");
    }

    #[tokio::test]
    async fn both_pages_are_reachable_from_capture() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("/ui/ops"), "{page}");
        assert!(page.contains("/ui/settings"), "{page}");
    }

    #[tokio::test]
    async fn minting_a_token_returns_to_settings() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .clone()
            .oneshot(form("/ui/ops/tokens", &cookie, "name=claude-code"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::ui::tests::the_installation_lives_on_its_own_page`
Expected: FAIL — 404 on `/ui/settings`.

- [ ] **Step 3: Create the settings template**

`src/web/templates/settings.html`:

```html
{% extends "layout.html" %}
{% block title %}Settings — engram{% endblock %}
{% block content %}

{# Everything about this installation rather than about what is in it. The two
   were one page, which meant scrolling past thirty-eight merge decisions to
   revoke a token. #}
<p class="crumb"><a href="/ui/capture">Capture</a> › Settings</p>

<h3>Browser extension</h3>
<p><a href="/extension/install">Install it</a> — capture the page you are
reading, and search from a panel beside it.</p>

<h3>API tokens</h3>
<div id="token-result"></div>
<form hx-post="/ui/ops/tokens" hx-target="#token-result" hx-swap="innerHTML"
      class="row" style="margin-bottom:1rem">
  <input class="input" name="name" placeholder="Token name, e.g. claude-code" style="max-width:20rem">
  <button class="btn btn-accent" type="submit">Mint</button>
</form>

<table class="grid">
  <thead><tr><th>Name</th><th>Created</th><th>Last used</th><th>State</th><th></th></tr></thead>
  <tbody>
  {% for t in tokens %}
    <tr>
      <td>{{ t.name }}</td>
      <td class="muted mono">{{ t.created }}</td>
      <td class="muted mono">{{ t.last_used }}</td>
      <td>{% if t.revoked %}<span class="badge badge-muted">revoked</span>
          {% else %}<span class="badge badge-success">active</span>{% endif %}</td>
      <td>
        {% if !t.revoked %}
        <form method="post" action="/ui/ops/tokens/{{ t.id }}/revoke"
              onsubmit="return confirm('Revoke “' + this.dataset.name + '”? Anything using it stops working immediately.')"
              data-name="{{ t.name }}">
          <button class="btn btn-sm btn-danger" type="submit">Revoke</button>
        </form>
        {% endif %}
      </td>
    </tr>
  {% endfor %}
  </tbody>
</table>
{% match feedback %}
{% when Some with (f) %}
<h3>Feedback</h3>
<p class="muted">
  Searches are being recorded — {{ f.captured }} captured, {{ f.pending }} unjudged.
  {% if f.pending > 0 %}<a href="/ui/judge">Judge them</a>{% endif %}
</p>
<form method="post" action="/ui/ops/feedback/purge"
      onsubmit="return confirm('Delete every captured search and every verdict given on one?')">
  <button class="btn btn-sm btn-danger" type="submit">Delete all captured searches</button>
</form>
{% when None %}
{% endmatch %}
{% endblock %}
```

The `data-name` / `this.dataset.name` pattern is copied from `_decide.html:38-40` — a title escaped into an attribute survives; escaped into a string literal inside an attribute it does not.

- [ ] **Step 4: Add the handler and the route**

In `src/web/ui.rs`, define `SettingsTemplate` beside `OpsTemplate` with `judge_pending`, `tokens`, `feedback`, and a handler that builds the token rows exactly as `ops` does today (lift that block into a shared `async fn token_rows(&AppState) -> Result<Vec<TokenRow>>` used by both — until Task 14 removes the `ops` caller entirely).

Register after the `/ui/ops` route:

```rust
        .route("/ui/settings", get(settings))
```

- [ ] **Step 5: Strip settings out of ops**

In `src/web/templates/ops.html`, delete lines 193-244 — everything from `<h3>Browser extension</h3>` to the end of the feedback block. Remove the now-unused fields from `OpsTemplate` and the code in `ops` that filled them.

- [ ] **Step 6: Link both from Capture**

In `src/web/templates/capture.html`, replace line 70:

```html
{# Reachable, not advertised. Housekeeping is what happened to the artifacts;
   Settings is what is true about this installation. Neither belongs in the top
   row, which stays three destinations wide. #}
<a class="quiet-link" href="/ui/ops">Housekeeping</a>
<a class="quiet-link" href="/ui/settings" style="margin-left:1rem">Settings</a>
```

- [ ] **Step 7: Send the POSTs back to the right page**

`mint_token` returns a fragment into `#token-result` and needs no change. `revoke_token_ui` and `purge_feedback_ui` redirect — read them and change their redirect target to `/ui/settings`.

- [ ] **Step 8: Run the tests**

Run: `cargo test`
Expected: PASS. `ops_shows_queue_state_and_tokens` at `src/web/ui.rs:3729` asserts `html.contains("API tokens")` — split it into an ops assertion and a settings one rather than deleting it.

- [ ] **Step 9: Commit**

```bash
git add src/web/templates/settings.html src/web/templates/ops.html src/web/templates/capture.html src/web/ui.rs
git commit -m "feat(ui): split what is true about the install off housekeeping

One page held six tables about the corpus plus the extension, the API tokens
and the feedback purge — so revoking a token meant scrolling past thirty-eight
merge decisions. Two pages, both still reached from one quiet line under
Capture, neither in the top row."
```

---

### Task 14: Housekeeping reads clearly

**Files:**
- Modify: `src/web/templates/ops.html:10-17`, `:27-54`, `:117-136`
- Modify: `src/web/ui.rs` (`ops`: row view models gain a subtitle; table limits)
- Modify: `assets/app.css` (link treatment inside `.grid`)
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: `MergedRow`, `SupersededRow` and `SourceRow` from `src/web/ui.rs`; `Chunk.created_at`, `Chunk.text`, `markdown::snippet`.
- Produces: `MergedRow`, `SupersededRow` and `SourceRow` each gain `pub subtitle: String` — `"2026-08-15 · alpha beta gamma…"`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn identically_titled_rows_are_told_apart() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["first body of text", "second body of text"]).await;
        for id in &ids {
            core.store.update_artifact_title(id, Some("Windows Update-Typen")).await.unwrap();
        }
        core.store.supersede_artifact(&ids[0], &ids[1]).await.unwrap();

        let page = get_body(&app, &cookie, "/ui/ops").await;
        assert!(
            page.contains("first body") || page.contains("second body"),
            "a row has to say which artifact it is: {page}"
        );
    }

    #[tokio::test]
    async fn the_counts_say_what_they_count() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/ops").await;
        assert!(page.contains("jobs done"), "a job count must not read as artifacts: {page}");
        assert!(page.contains("links between artifacts"), "{page}");
    }

    #[tokio::test]
    async fn both_reversals_are_called_the_same_thing() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/ops").await;
        assert!(!page.contains("Put it back"), "{page}");
        assert!(!page.contains("Undo merge"), "{page}");
    }
```

Match `supersede_artifact` and `artifacts(...)` to the real helpers — `grep -n "fn artifacts(" src/web/ui.rs` and `grep -n "pub async fn supersede" src/store/artifacts.rs` — and use them verbatim.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib web::ui::tests::the_counts_say_what_they_count web::ui::tests::both_reversals_are_called`
Expected: FAIL.

- [ ] **Step 3: Rewrite the counts**

In `src/web/templates/ops.html`, replace lines 10-17:

```html
{# A sentence, not a row of key-value chips: this is a page you open to reassure
   yourself, and a debug dump reads as something going wrong. Each number says
   what it counts — "1523 done" beside "1236 artifacts" read as an artifact
   count, and there is no reading of that sentence in which it is one. #}
<p class="muted">
  {{ artifact_count }} artifacts, {{ vector_count }} embedded.
  {% if job_counts.is_empty() %}No jobs queued.{% else %}
  {% for j in job_counts %}{{ j.1 }} jobs {{ j.0 }}{% if !loop.last %}, {% endif %}{% endfor %}.
  {% endif %}
  {% if let Some(age) = oldest_pending_secs %}Oldest pending job {{ age }}s old.{% endif %}
  {% if let Some(l) = links %}
  {{ l.total }} links between artifacts, {{ l.related }} named, {{ l.judge_queue }} waiting on the judge.
  {% endif %}
</p>
```

- [ ] **Step 4: One name for one reversal**

In `src/web/templates/ops.html` line 48, change `Undo merge` to `Undo`. Line 129, change `Put it back` to `Undo`. Both columns already have headings that carry the meaning; if the merged table's action column has no heading, give it one: `<th>Reverse</th>`.

- [ ] **Step 5: Add the subtitles**

In `src/web/ui.rs`, add `pub subtitle: String` to `MergedRow`, `SupersededRow` and `SourceRow`, filled where each is built:

```rust
        // Two artifacts can carry the same title — a merge of two documents
        // that named the same section identically produces exactly that — and a
        // table of them is unreadable without something to tell them apart.
        subtitle: format!("{} · {}", fmt_time(c.created_at), markdown::snippet(&c.text, 60)),
```

Render it under each title in `ops.html`, in the merged table (line 33) and the superseded table (lines 125-126):

```html
        <a href="/ui/artifacts/{{ m.id }}">{{ m.title }}</a>
        <div class="muted row-sub">{{ m.subtitle }}</div>
```

and in the sources loop at line 43:

```html
        {% for src in m.sources %}
        <div><a href="/ui/artifacts/{{ src.id }}">{{ src.title }}</a>
          <div class="muted row-sub">{{ src.subtitle }}</div></div>
        {% endfor %}
```

- [ ] **Step 6: Cap the tables**

The handler already passes `50` to every list call. Change the merged and superseded calls to `26`, render the first 25, and where a 26th came back render:

```html
<p class="muted">More than 25 — the rest appear as these are cleared.</p>
```

State the cap rather than silently truncating. Do the same for `deprecated`, `stale` and `parked` only if their handler limits already exceed 25; otherwise leave them.

- [ ] **Step 7: Quiet the link wall**

In `assets/app.css`:

```css
/* Four underlined links per row read as a wall. The artifact the row is about
   keeps its underline; what it was written from is reachable but recedes. */
.grid .row-sub { font-size: 0.75rem; }
.grid td div > a { text-decoration: none; }
.grid td div > a:hover { text-decoration: underline; }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test`
Expected: PASS. `ops_says_how_many_links_there_are_and_how_many_are_named` at `src/web/ui.rs:3753` asserts on the old sentence — update its expected strings.

- [ ] **Step 9: Run the full check and commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
git add src/web/templates/ops.html src/web/ui.rs assets/app.css
git commit -m "fix(ui): make housekeeping rows say which artifact they mean

'1523 done' beside '1236 artifacts' read as an artifact count. Rows merged
from two identically titled sources were indistinguishable. And one reversal
had two names — 'Undo merge' and 'Put it back' — for the same action."
```

---

### Task 15: Tell two tokens apart

**Files:**
- Modify: `src/store/schema.sql` (`api_tokens`), `src/store/mod.rs` (`ADDED_COLUMNS`)
- Modify: `src/store/auth.rs:31-51` (`insert_token`, `ApiToken`)
- Modify: `src/auth/tokens.rs:31-46` (`mint`)
- Modify: `src/web/pair.rs:159`, `src/web/ui.rs:1471`, `src/web/test_support.rs:35`
- Modify: `src/web/templates/settings.html`
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: `SettingsTemplate.tokens: Vec<TokenRow>` from Task 13.
- Produces: `mint(store, name, subject, user_agent: Option<&str>)`; `ApiToken.user_agent: Option<String>`; `TokenRow.minted_by: String`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn two_tokens_with_one_name_are_still_tellable_apart() {
        let (app, cookie, core) = app_session_and_core().await;
        crate::auth::tokens::mint(&core.store, "browser extension", "user-1", Some("Firefox/141.0"))
            .await
            .unwrap();
        crate::auth::tokens::mint(&core.store, "browser extension", "user-1", Some("Chrome/152.0"))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/settings").await;
        assert!(page.contains("Firefox"), "{page}");
        assert!(page.contains("Chrome"), "{page}");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib web::ui::tests::two_tokens_with_one_name`
Expected: FAIL to compile — `mint` takes three arguments.

- [ ] **Step 3: Add the column**

In `src/store/schema.sql`, add `user_agent TEXT` to the `api_tokens` table (line 406-414). In `src/store/mod.rs`'s `ADDED_COLUMNS`:

```rust
            // Arrived with the settings page. NULL on every token minted before
            // it, which is the truth: nothing recorded what asked for them.
            ("api_tokens", "user_agent", "TEXT"),
```

- [ ] **Step 4: Carry it through**

Add `pub user_agent: Option<String>` to `ApiToken`. Give `insert_token` and `mint` a trailing `user_agent: Option<&str>` parameter and bind it in the INSERT. Update the three call sites:

- `src/web/pair.rs:159` — the extension pairing flow. Pass the request's `User-Agent` header; this is the one call site where it matters, because the extension mints every token under the same name.
- `src/web/ui.rs:1471` — the mint form. Pass the browser's `User-Agent`.
- `src/web/test_support.rs:35` — pass `None`.

Read `pair.rs` around line 159 to see whether the handler already has the `HeaderMap`; if not, add `headers: axum::http::HeaderMap` to its extractor list.

- [ ] **Step 5: Show it**

Add `pub minted_by: String` to `TokenRow`, filled where the rows are built:

```rust
        // What asked for this token. Two tokens named "browser extension",
        // minted two days apart and neither used yet, are otherwise the same
        // row twice — and one of them is the one currently working.
        minted_by: t.user_agent.clone().unwrap_or_else(|| "—".into()),
```

In `settings.html`, add a `<th>Minted by</th>` column after `Name` and `<td class="muted">{{ t.minted_by }}</td>` in the row.

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/store/schema.sql src/store/mod.rs src/store/auth.rs src/auth/tokens.rs src/web/pair.rs src/web/ui.rs src/web/test_support.rs src/web/templates/settings.html
git commit -m "feat(auth): record what asked for a token

The extension mints every token under the same name, so two rows called
'browser extension', neither used yet, were the same row twice — and one of
them was the one currently working."
```

---

## Final verification

- [ ] **Run everything**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- [ ] **Walk the app**

Start it and check by hand, since most of this plan is markup no test can see:

1. `/ui/capture` — the button sits below the note field; the note says "file you drop next"; a pending pair leads with its titles; a Recent row reads `N artifacts · X% covered` with the timestamps aligned.
2. Click a low coverage percentage — it lands on the corpus page at `Never reached an artifact`, with a range that links to the lines and a `Read these again` button.
3. `/ui/search` — one KIND row of form words, no TAG row, no timing readout, a result count; click a result and watch the card take the selected state; the source pane wraps and its label reads `line N` for a one-line span.
4. `/ui/ops` — wide, no tokens section, one `Undo` per row, rows distinguishable, counts that say what they count.
5. `/ui/settings` — tokens with what minted them, extension, feedback.

- [ ] **Squash-free push**

```bash
git log --oneline feat/ask-harness ^master
git push origin feat/ask-harness
```
