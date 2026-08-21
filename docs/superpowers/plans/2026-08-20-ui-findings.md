# GUI Findings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the twenty-odd usability defects found by operating the live deployment, without disturbing the design decisions this codebase argues for in comments.

**Architecture:** Almost every change is at the web door — one helper module for the rules that several call sites currently each invent (stand-in titles, elapsed time), then template, CSS and handler changes on top of it. One change reaches deeper: the consolidation queue groups pending pairs by the cluster `jobs/consolidate.rs` already computes. No schema migrations.

**Tech Stack:** Rust, axum, askama templates (`src/web/templates/`), htmx + a single hand-written `assets/app.js`, plain layered CSS (`assets/css/00-…` through `50-phone.css`), SQLite via sqlx. Tests are inline `#[cfg(test)] mod tests` in the file under test, run with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-08-20-ui-findings-design.md`

## Global Constraints

- **Branch:** `fix/ui-findings`, based on `origin/feat/one-system`. Not `master` — prod runs the former.
- **Never render the word `Untitled`.** Existing test `a_result_with_no_title_of_its_own_is_given_no_heading` (`ui.rs:4700`) enforces this for the rail; the same must hold everywhere.
- **Never render `Chunk N` as a title.**
- **Two title rules, by context.** Where a snippet sits beside the name (search rail, judge card): no heading at all. Where a name is structurally required (button label, sitting list, artifact pane heading): a stand-in from the body, cut at a word boundary, leading punctuation stripped, marked as a stand-in.
- **Truncate by chars, never by bytes.** Slicing mid-codepoint panics; the corpus is largely German.
- **Do not change:** the phone hiding KIND chips (`50-phone.css:108`), the phone badge dropping its number (`layout.html:130`), Judge's rank numbers stopping at nine (`judge.rs:190`), or Judge's IR vocabulary. All are deliberate and defended in comments.
- **Comment style:** this codebase explains *why* in prose above the code, and expects it. Match it.
- **Test fixtures:** several tasks call helpers like `render_ask_page_fixture()` or `chunk_fixture()`. Where one does not exist yet, build it in that task's test module following the pattern already in `ui.rs:4707` — construct the real struct with every field named, and render through `askama::Template::render`. Do not stub the type under test.

---

### Task 1: A stand-in title that does not cut mid-word

**Files:**
- Modify: `src/web/markdown.rs` (add `stand_in_title`, fix `snippet` truncation)
- Test: `src/web/markdown.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `pub fn stand_in_title(text: &str, max_chars: usize) -> String` — markdown stripped, leading punctuation and whitespace removed, truncated at a word boundary with an ellipsis. Used by Tasks 2, 3, 4, 5.
- Produces: `snippet` keeps its signature `(markdown: &str, max_chars: usize) -> String` but also stops at a word boundary.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_stand_in_title_stops_at_a_word() {
        // The sitting rendered "…zusätzlich darin vo" — a name cut mid-word
        // reads as a truncated name, not as the opening of a passage.
        let t = stand_in_title("Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort", 60);
        assert!(!t.contains("vo…"), "cut mid-word: {t:?}");
        assert!(t.ends_with('…'), "{t:?}");
        assert!(t.chars().count() <= 61, "{t:?}");
    }

    #[test]
    fn a_stand_in_title_drops_leading_punctuation_and_markup() {
        // "Keep \"- schneller Schreibzugriff (…) -\"" was a body opening,
        // dashes and all, pressed into service as a name.
        assert_eq!(stand_in_title("- schneller Schreibzugriff auf den Stapel", 60),
                   "schneller Schreibzugriff auf den Stapel");
        assert_eq!(stand_in_title("## 3.4.2 FESTE MFT RECORDS", 60), "3.4.2 FESTE MFT RECORDS");
    }

    #[test]
    fn a_short_body_becomes_a_stand_in_unchanged() {
        assert_eq!(stand_in_title("CPU fair scheduler parameter", 60), "CPU fair scheduler parameter");
    }

    #[test]
    fn a_stand_in_of_nothing_is_empty() {
        assert_eq!(stand_in_title("   \n\n  ", 60), "");
        assert_eq!(stand_in_title("---", 60), "");
    }

    #[test]
    fn a_snippet_stops_at_a_word_too() {
        let s = snippet("Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort", 30);
        assert!(!s.contains("zusä…"), "cut mid-word: {s:?}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib web::markdown`
Expected: FAIL — `cannot find function stand_in_title in this scope`, plus `a_snippet_stops_at_a_word_too` failing on the existing char-count truncation.

- [ ] **Step 3: Implement**

Add to `src/web/markdown.rs`:

```rust
/// Truncate at a word, never inside one, and never inside a codepoint.
///
/// `chars().take(n)` was the whole of this and it produced "…darin vo" in the
/// sitting and "Fake title: al…" in the corpus rail: a name cut mid-word reads
/// as a broken name, where one cut at a space reads as an opening. Falls back
/// to the hard cut when the text has no space to fall back to — one
/// unbroken 200-character token is still better shortened than shown whole.
fn truncate_at_word(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let hard: String = text.chars().take(max_chars).collect();
    let cut = match hard.rfind(char::is_whitespace) {
        // Not any space: one near the very start would leave a single word
        // standing for a whole passage. Half the budget is the floor.
        Some(i) if hard[..i].chars().count() * 2 >= max_chars => &hard[..i],
        _ => hard.as_str(),
    };
    format!("{}…", cut.trim_end())
}

/// A name for something that has none, derived from its own opening.
///
/// A verbatim passage has no title by design. Where the layout can let a
/// snippet speak for the row there should be no heading at all — see
/// `render_hit`. Where a name is structurally required, a button label or a
/// list of what this sitting touched, this is that name: the body's opening,
/// with the markup and the leading punctuation that are structure rather than
/// subject taken off the front.
pub fn stand_in_title(text: &str, max_chars: usize) -> String {
    let flat = snippet(text, usize::MAX);
    let opening = flat.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '-' | '–' | '—' | '*' | '#' | '>' | '·' | '•' | '|')
    });
    truncate_at_word(opening.trim(), max_chars)
}
```

And replace `snippet`'s tail (the `let mut out: String = text.chars().take(max_chars).collect(); out.push('…'); out` block) with:

```rust
    truncate_at_word(&text, max_chars)
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::markdown`
Expected: PASS, including the pre-existing `snippet_returns_plain_text_and_truncates_on_a_char_boundary`.

- [ ] **Step 5: Commit**

```bash
git add src/web/markdown.rs
git commit -m "feat(ui): a name cut at a word, and never the punctuation in front of it"
```

---

### Task 2: `title_of` adopts the rule

**Files:**
- Modify: `src/web/ui.rs:1957` (`title_of`)
- Test: `src/web/ui.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `markdown::stand_in_title` from Task 1.
- Produces: `title_of` keeps its signature `(c: &Chunk) -> String`; callers are the sitting rail (`ui.rs:472`) and the dedupe pair rows (`ui.rs` pair building).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_untitled_artifact_is_named_by_its_opening_not_by_its_first_sixty_bytes() {
        // Both of these came off the deployment: the sitting cut a name
        // mid-word, and "Needs you" offered a button reading
        // `Keep "- schneller Schreibzugriff (…) -"`.
        let c = |text: &str| crate::store::artifacts::Chunk {
            title: None,
            text: text.into(),
            ..chunk_fixture()
        };
        let t = title_of(&c("Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort"));
        assert!(!t.ends_with("vo"), "cut mid-word: {t:?}");
        assert_eq!(
            title_of(&c("- schneller Schreibzugriff (Änderungen vom Key auf Stapel) -")),
            "schneller Schreibzugriff (Änderungen vom Key auf Stapel) -"
        );
        assert_eq!(title_of(&crate::store::artifacts::Chunk {
            title: Some("LevelDB".into()), ..c("body")
        }), "LevelDB");
    }
```

If no `chunk_fixture()` helper exists in `ui.rs`'s test module, add one that builds a `Chunk` with every field defaulted — copy the field list from `store/artifacts.rs:124` — and reuse it in later tasks.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::an_untitled_artifact_is_named_by_its_opening`
Expected: FAIL — the assertion on `- schneller…`, because `title_of` returns the leading dash.

- [ ] **Step 3: Implement**

Replace `title_of` (`ui.rs:1957`):

```rust
/// What to call an artifact in a place that must call it something.
///
/// Sixty characters of raw body was this, and it is where the sitting's
/// "…darin vo" and the dedupe queue's `Keep "- schneller Schreibzugriff …"`
/// both came from. The rule now lives in one place — see
/// `markdown::stand_in_title` — so the sitting, the pair cards and the judge
/// cannot drift apart again.
pub(crate) fn title_of(c: &crate::store::artifacts::Chunk) -> String {
    c.title
        .clone()
        .unwrap_or_else(|| crate::web::markdown::stand_in_title(&c.text, 60))
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs
git commit -m "fix(ui): the sitting and the pair cards stop naming things by their punctuation"
```

---

### Task 3: The judge card stops saying "Untitled"

**Files:**
- Modify: `src/web/judge.rs:132` (`snippet_of`), `:179`, `:515`
- Test: `src/web/judge.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `markdown::snippet` from Task 1.
- Produces: `Choice.title` may now be empty; `_judge_card.html` must render no heading element when it is (Task 4 of this group — done here, in the same task, since the template and the field change together).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_judge_card_names_nothing_untitled_and_leaks_no_markdown() {
        // Two thirds of the deployment's judge list read "Untitled", and the
        // snippets under them carried `custom\_passphrase` and `# Configure`
        // because this door flattened whitespace instead of stripping markup.
        assert_eq!(snippet_of("## Configure **auditd**"), "Configure auditd");
        assert!(!snippet_of(r"2 - A custom passphrase (custom\_passphrase)").contains('\\'));
    }
```

Plus a rendering assertion in whichever test already builds a `Card` — assert `!html.contains("Untitled")`.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::judge`
Expected: FAIL — `snippet_of` returns `## Configure **auditd**` verbatim.

- [ ] **Step 3: Implement**

Replace `snippet_of` (`judge.rs:132`):

```rust
/// The card's preview: plain text, markup gone.
///
/// Flattening whitespace was the whole of this, so a card showed
/// `# Configure Linux…` and `custom\_passphrase` — the escapes an artifact
/// carries so that markdown renders it correctly, shown to a person as if
/// they were the text.
fn snippet_of(text: &str) -> String {
    crate::web::markdown::snippet(text, 140)
}
```

At `judge.rs:179` and `judge.rs:515`, replace `a.title.unwrap_or_else(|| "Untitled".into())` (and `h.title…`) with `a.title.unwrap_or_default()`.

In `src/web/templates/_judge_card.html`, wrap the title so an empty one renders no element at all:

```html
{% if !c.title.is_empty() %}<span class="choice-title">{{ c.title }}</span>{% endif %}
```

Add a comment above it in the codebase's voice, pointing at `render_hit`'s reasoning.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::judge`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/judge.rs src/web/templates/_judge_card.html
git commit -m "fix(judge): no heading where there is no name, and no markup in the preview"
```

---

### Task 4: The artifact pane stops saying "Chunk 56"

**Files:**
- Modify: `src/web/ui.rs:367`, `src/web/ui.rs:3102`
- Test: `src/web/ui.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `markdown::stand_in_title` from Task 1.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_artifact_pane_does_not_call_a_passage_chunk_fifty_six() {
        // "Chunk 56" is an ordinal in the ingest, not a name for anything a
        // reader asked for.
        let d = build_detail_fixture_with(None, "Die digitale Forensik unterscheidet sich");
        assert!(!d.title.starts_with("Chunk"), "{:?}", d.title);
        assert!(d.title.starts_with("Die digitale Forensik"), "{:?}", d.title);
    }
```

Build the fixture with whatever the two sites construct; if a helper does not exist, assert directly on `title_of`-style output at both call sites instead.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::the_artifact_pane_does_not_call_a_passage_chunk`
Expected: FAIL — title is `Chunk 56`.

- [ ] **Step 3: Implement**

At both `ui.rs:367` and `ui.rs:3102`, replace `.unwrap_or_else(|| format!("Chunk {}", c.ordinal))` with `.unwrap_or_else(|| crate::web::markdown::stand_in_title(&c.text, 60))`, with a comment saying why the ordinal is not a name.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs
git commit -m "fix(ui): an ordinal in the ingest is not a name for a passage"
```

---

### Task 5: The MCP door stops saying "Untitled"

**Files:**
- Modify: `src/mcp/mod.rs:19`
- Test: `src/mcp/mod.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_mcp_door_names_a_passage_by_its_opening() {
        // Claude Code reads this list. "Untitled" three times over is a list
        // of a word that says nothing.
        let out = render_result_line(&result_fixture(None, "Die digitale Forensik unterscheidet sich"));
        assert!(!out.contains("Untitled"), "{out}");
    }
```

Adapt the fixture name to whatever `mcp/mod.rs` already constructs at line 19.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib mcp`
Expected: FAIL — output contains `Untitled`.

- [ ] **Step 3: Implement**

Replace `r.title.clone().unwrap_or_else(|| "Untitled".into())` with `r.title.clone().unwrap_or_else(|| crate::web::markdown::stand_in_title(&r.text, 60))`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib mcp`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/mod.rs
git commit -m "fix(mcp): the door reads a passage's opening rather than the word Untitled"
```

---

### Task 6: The disambiguator that runs but cannot be seen

**Files:**
- Modify: `assets/css/41-capture.css:14-18` (`.qtitle`)
- Modify: `src/web/ui.rs:1368` (`disambiguate_labels`, generalised)
- Modify: `src/web/ui.rs` (pair rows, to reuse it)
- Test: `src/web/ui.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `fn disambiguate_by<T>(rows: &mut [T], label: impl Fn(&T) -> &str, opening: impl Fn(&T) -> &str, set: impl Fn(&mut T, String))` — or, if the generic form fights the borrow checker, a `disambiguate_labels`-shaped function per row type sharing one helper for the collision set. Task 8 consumes the disambiguated pair rows.

**Context you need before touching this.** Capture's Recent list already has a repair for shared titles: `disambiguate_labels` at `ui.rs:1368`, whose comment names this exact failure — "six rows read `HOCHSCHULE MITTWEIDA` and named nothing". It works. The reason the deployment still showed six identical rows is that `.qtitle` is one line with `text-overflow: ellipsis`, so the ` · opening` suffix it appends is cut off before it is ever visible. Do not rewrite the function; give its output somewhere to be seen. The dedupe pair rows are the opposite case — they have no disambiguation at all, and should borrow this one rather than grow a second copy.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_disambiguated_row_shows_the_part_that_distinguishes_it() {
        // `disambiguate_labels` appended the opening words and `.qtitle`
        // truncated them away, so six rows still read "HOCHSCHULE MITTWEIDA ·
        // HOCHSCH…" and the one column that exists to tell captures apart
        // still could not.
        let html = render_queue_fixture(vec![
            ("HOCHSCHULE MITTWEIDA", "Fachbereich Angewandte Computer- und Biowissenschaften"),
            ("HOCHSCHULE MITTWEIDA", "Ein Verfahren zur Sicherung flüchtiger Daten"),
        ]);
        assert!(html.contains("qtitle-opening"), "the opening has no element of its own: {html}");
    }

    #[test]
    fn pair_rows_sharing_a_title_are_disambiguated_too() {
        // Three artifacts on the deployment were titled "LevelDB:
        // Funktionsweise und forensische Analyse", so one cluster of
        // questions read as the same question three times.
        let mut rows = vec![
            pair_row_fixture("LevelDB: Funktionsweise", "Der Aufbau der Datenlagerung"),
            pair_row_fixture("LevelDB: Funktionsweise", "Die Extraktion der Keys"),
            pair_row_fixture("SQLite und WAL", "Pragma-Abfragen"),
        ];
        disambiguate_pair_titles(&mut rows);
        assert_ne!(rows[0].a_title, rows[1].a_title, "still identical");
        assert_eq!(rows[2].a_title, "SQLite und WAL", "a unique title is left alone");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib web::ui::tests::a_disambiguated_row_shows web::ui::tests::pair_rows_sharing_a_title`
Expected: FAIL — no `qtitle-opening` element, and `cannot find function disambiguate_pair_titles`.

- [ ] **Step 3: Implement**

Two changes, and neither touches `disambiguate_labels`' logic.

First, give the suffix a place to live. In the queue template, render the label and the opening as two elements rather than one concatenated string, and in `41-capture.css` let the opening sit on its own line, dim and smaller, exempt from the parent's `nowrap`:

```css
/* The label truncates; what tells this row from the one above it does not.
   `disambiguate_labels` appends the capture's opening words to a label that
   collides, and a single `nowrap` line cut the appended half off — so the
   repair ran, and six rows still read the same six words. */
.qtitle-opening {
  display: block; white-space: normal; overflow: visible;
  font-size: 0.8125rem; color: var(--color-fg-muted);
}
```

Second, apply the same collision rule to the dedupe pair titles. Factor the collision-set half of `disambiguate_labels` into a small shared helper and call it from a `disambiguate_pair_titles(&mut [PairRow])` that appends `markdown::stand_in_title(&body, 40)` to any `a_title`/`b_title` shared by another row. Keep the three exemptions the original documents: a label already unique, a row with no opening to offer, and an opening equal to the label.

- [ ] **Step 4: Run the tests, then look at the page**

Run: `cargo test --lib web::ui`
Expected: PASS. Then load `/ui/capture` against a base with repeated headings and confirm Recent's rows are now distinguishable without hovering.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs assets/css/41-capture.css src/web/templates
git commit -m "fix(capture): the half of the label that tells two rows apart survives the truncation"
```

---

### Task 7: Find out why every escalated pair has no detail

**Files:**
- Modify: `src/jobs/dedupe.rs` (the `settle` path), or `src/infer/prompt.rs` (the parse) — whichever the investigation indicts
- Test: `src/jobs/dedupe.rs` (inline `mod tests`)

**This task is a debugging task, not a design task.** REQUIRED SUB-SKILL: use `superpowers:systematic-debugging`. Do not write a fix before the test reproduces the empty field.

**What is known:** the field is stored (`store/pairs.rs:108`), rewritten on settle (`store/pairs.rs:318`), read and specially cased for the `link` marker (`ui.rs:1879`), and rendered (`_decide.html`, `{% if let Some(d) = p.detail %}`). The prompt demands it: "detail: one short sentence saying why. Always." (`prompt.rs`). All five `Contradiction` rows on the deployment have it null. Suspects, in order: the JSON parse at `prompt.rs:851` dropping `detail` for the `conflict` arm; `settle` passing `None` where the verdict had `Some`; a `Contradiction` written by a path that never had a verdict (`reopen_pairs_merged_into`, or the "merged member has lost its sources" escalation, which does pass a detail).

- [ ] **Step 1: Reproduce it in a test**

Write a test that parses a realistic `conflict` verdict and asserts the detail survives to the stored row:

```rust
    #[test]
    fn a_conflict_verdict_carries_its_reason_to_the_stored_pair() {
        // Every escalated pair on the deployment had a null detail, so each
        // card said two artifacts disagreed and never what about — which is
        // the whole of what a person needs to decide it.
        let v = crate::infer::prompt::parse_dedupe(
            r#"{"verdict":{"relation":"conflict","detail":"A says ext4 is supported, B says it is not."}}"#,
        )
        .unwrap();
        assert_eq!(v.relation, crate::infer::prompt::Relation::Conflict);
        assert_eq!(v.detail.as_deref(), Some("A says ext4 is supported, B says it is not."));
    }
```

Adapt `parse_dedupe` to the real parser's name at `prompt.rs:851`.

- [ ] **Step 2: Run it**

Run: `cargo test --lib infer::prompt jobs::dedupe`
Expected: either FAIL, which names the parse as the cause — proceed to Step 3 — or PASS, which exonerates the parse and moves the investigation to `settle` and then to the writers of `Contradiction` that never saw a verdict. Follow the evidence; do not guess.

- [ ] **Step 3: Fix the indicted layer**

Minimal change at the layer the test indicted. If the cause is a path that legitimately has no verdict, the fix is that it writes a detail saying so, in the codebase's voice — never a card that names no dispute.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "fix(dedupe): an escalated pair says what it is escalating"
```

---

### Task 8: One cluster is one question

**Files:**
- Modify: `src/web/ui.rs` (pair collection, near `PAIR_LIMIT` at `ui.rs:1974`)
- Modify: `src/web/templates/_decide.html`
- Test: `src/web/ui.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `disambiguate_pair_titles` (Task 6), the `Clusters` disjoint-set pattern in `jobs/consolidate.rs:60`.
- Produces: the template iterates clusters, each carrying `Vec<PairRow>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn pairs_that_share_an_artifact_are_one_card() {
        // The deployment showed one artifact against three others as three
        // separate questions, 90%, 90% and 88% alike — the same decision
        // asked three times, and answering one did not retire the others.
        let grouped = group_pairs(vec![
            pair_fixture(1, "a", "b"),
            pair_fixture(2, "a", "c"),
            pair_fixture(3, "a", "d"),
            pair_fixture(4, "x", "y"),
        ]);
        assert_eq!(grouped.len(), 2, "{grouped:?}");
        assert_eq!(grouped[0].pairs.len(), 3);
        assert_eq!(grouped[1].pairs.len(), 1);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::pairs_that_share_an_artifact`
Expected: FAIL — `cannot find function group_pairs`.

- [ ] **Step 3: Implement**

Add a `PairCluster { pairs: Vec<PairRow> }` and a `group_pairs` using the same union-find shape as `jobs/consolidate.rs:60` — union on `a_id`/`b_id`, then bucket by root, preserving the incoming order so `PAIR_STATES`' priority survives. Comment why: resolving a cluster pairwise leaves the operator answering the same question N times, and `consolidate.rs` already documents the dead-end that pairwise resolution creates.

In `_decide.html`, iterate clusters; a cluster of one renders exactly as it does today, so nothing regresses for the common case. For a cluster of several, state the shape once ("four artifacts, three open questions") above the pair rows.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs src/web/templates/_decide.html
git commit -m "feat(capture): one cluster asks once, not once per pair"
```

---

### Task 9: Deciding a pair without leaving the page

**Files:**
- Modify: `src/web/templates/_decide.html`
- Modify: `assets/css/41-capture.css`
- Test: `src/web/ui.rs` (inline `mod tests`, rendering assertion)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_pair_card_carries_both_texts_to_read_in_place() {
        // The titles were links, so reading either side meant leaving the
        // queue and coming back to it.
        let html = render_capture_fixture_with_one_pair();
        assert!(html.contains("<details"), "{html}");
        assert!(html.contains("Auto Vacuum"), "the A side's text is not on the card: {html}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_pair_card_carries_both_texts`
Expected: FAIL — no `<details>` in the card.

- [ ] **Step 3: Implement**

Add `a_excerpt` / `b_excerpt` to `PairRow` (via `markdown::snippet(&text, 400)`), and render each behind a `<details><summary>` inside the card, above the button row. Keep the existing title links — they remain the way to the whole artifact. Style the two excerpts as two columns at desk width, stacked on phone.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/_decide.html assets/css/41-capture.css src/web/ui.rs
git commit -m "feat(capture): read both sides where the decision is made"
```

---

### Task 10: Reasoning becomes something you open

**Files:**
- Modify: `src/web/templates/ask.html:42`
- Modify: `assets/app.js:285-310`
- Modify: `assets/css/45-ask.css`
- Test: `src/web/ui.rs` (inline `mod tests`, rendering assertion)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_ask_page_does_not_open_with_the_models_reasoning_showing() {
        // The deployment streamed the chain of thought into the page for
        // fifty seconds, restating the prompt's own constraints verbatim:
        // "Answer *only* using the provided knowledge-base excerpts".
        let html = render_ask_page_fixture();
        assert!(html.contains("<details"), "{html}");
        assert!(!html.contains("<details open"), "reasoning must start closed: {html}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::an_ask_page_does_not_open_with_the_models_reasoning`
Expected: FAIL — the page renders a bare `<div id="ask-reasoning">`.

- [ ] **Step 3: Implement**

In `ask.html`, replace the bare div with a closed disclosure, keeping the id on the inner element so `app.js` needs no rewiring:

```html
  {# What the model said on the way to the answer. Behind a disclosure, closed:
     it is not the answer, nothing in it is cited, and it restates the prompt's
     own constraints back at whoever is reading. Kept rather than dropped
     because it is what a tuning session actually wants to see. #}
  <details id="ask-reasoning-box" class="reasoning-box" hidden>
    <summary>Reasoning</summary>
    <div id="ask-reasoning" class="reasoning"></div>
  </details>
```

In `app.js`, toggle `ask-reasoning-box`'s `hidden` where it currently toggles `reasoning.hidden` (lines 228, 287, 309, 350), and scroll `reasoning` to its end on append **only when the disclosure is open** — a closed box must not fight the page for scroll position.

In `45-ask.css`, give `.reasoning` a `max-height` with `overflow-y: auto` so an opened box is bounded.

- [ ] **Step 4: Run the tests, then check by hand**

Run: `cargo test --lib web::ui`
Expected: PASS. Then run the app and ask a question: the box appears closed, opens on click, scrolls to the newest token while open, and disappears when the answer lands.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/ask.html assets/app.js assets/css/45-ask.css src/web/ui.rs
git commit -m "feat(ask): the reasoning is there when you want it and nowhere when you do not"
```

---

### Task 11: A fifty-second wait says what it is doing

**Files:**
- Modify: `src/web/templates/ask.html:21-31`
- Modify: `assets/app.js` (the ask driver, from line ~196)
- Modify: `assets/css/45-ask.css`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_ask_in_flight_offers_a_way_to_stop_it() {
        let html = render_ask_page_fixture();
        assert!(html.contains(r#"id="ask-stop""#), "{html}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::an_ask_in_flight_offers_a_way_to_stop_it`
Expected: FAIL — no such control.

- [ ] **Step 3: Implement**

Add `<button id="ask-stop" class="btn btn-ghost" type="button" hidden>Stop</button>` beside the spinner. In `app.js`: show it when the `EventSource` opens, hide it on `done`/`error`; on click call `source.close()`, hide the spinner, keep whatever `#ask-live` holds, and write "stopped" into `#ask-status`. Start a one-second interval on open that writes elapsed seconds into `#ask-spinner` (`thinking… 12s`), and clear it on close.

Leave `#ask-progress` where it is — it already carries the retrieval line, and that line is the honest progress signal; the elapsed counter supplements it rather than replacing it.

- [ ] **Step 4: Run the tests, then check by hand**

Run: `cargo test --lib web::ui`
Expected: PASS. Then ask a question and press Stop mid-stream: partial text stays, no console error, a second ask still works.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/ask.html assets/app.js assets/css/45-ask.css src/web/ui.rs
git commit -m "feat(ask): a wait that says how long it has been, and a way out of it"
```

---

### Task 12: The ask page's remaining copy, and the stylesheet that was never written

**Files:**
- Modify: `src/web/templates/_answer.html:16`
- Modify: `src/web/templates/_ask_verdict.html:14`
- Modify: `src/web/templates/_sitting.html`
- Modify: `assets/css/45-ask.css`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_answer_says_what_was_dropped_in_words_a_person_uses() {
        let html = render_answer_fixture(18);
        assert!(!html.contains("excerpt(s)"), "{html}");
        assert!(!html.contains("context budget"), "{html}");
        assert!(html.contains("18 more excerpts"), "{html}");
        let one = render_answer_fixture(1);
        assert!(one.contains("1 more excerpt did not fit"), "{one}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::the_answer_says_what_was_dropped`
Expected: FAIL — badge reads `18 excerpt(s) omitted for context budget`.

- [ ] **Step 3: Implement**

In `_answer.html`, replace the badge text with a properly pluralised sentence: `{{ dropped }} more excerpt{% if dropped != 1 %}s{% endif %} did not fit`, keeping the internal reason in the `title` attribute where an operator can still find it.

In `_ask_verdict.html`, make `Right` / `Wrong` / `Nothing here` and "keep this answer" carry `btn btn-sm` rather than reading as bare text.

In `_sitting.html`, change the heading from `This sitting` to something that says what it is — "Read just now" — and in `45-ask.css` write the `.sitting` rule that has never existed: it is a small aside, dim, above the answer, and its list must not read as a second set of results.

- [ ] **Step 4: Run the tests, then look at the page**

Run: `cargo test --lib web::ui`
Expected: PASS. Then load `/ui/ask` after reading an artifact and confirm the sitting block is styled rather than falling back to bare `<ul>` defaults.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates assets/css/45-ask.css src/web/ui.rs
git commit -m "fix(ask): plain words for what did not fit, and a stylesheet for the sitting"
```

---

### Task 13: The rail uses the width the pane is not using

**Files:**
- Modify: `assets/css/20-layout.css:69`, `assets/css/40-search.css`
- Modify: `src/web/templates/search.html` (a class on `.regions` when nothing is selected)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_search_with_nothing_open_says_so_on_the_grid() {
        // 22rem of rail beside a thousand pixels holding one line of
        // placeholder is the whole complaint.
        let html = render_search_fixture_without_selection();
        assert!(html.contains("no-selection"), "{html}");
        let open = render_search_fixture_with_selection();
        assert!(!open.contains("no-selection"), "{open}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_search_with_nothing_open`
Expected: FAIL — no such class.

- [ ] **Step 3: Implement**

Add `no-selection` to the `.regions` element when no artifact is open (mirroring the existing `has-selection` used by `50-phone.css:58`). In `20-layout.css`, under `.regions-rail-focus-source.no-selection`, widen the rail column to `40rem`. In `40-search.css`, let `.no-selection` rows show their whole snippet rather than clamping to two lines. Comment why the pane keeps its placeholder: opening the top hit would spend a read on every search, including the ones the rail already answers.

- [ ] **Step 4: Run the tests, then look at it**

Run: `cargo test --lib web::ui`
Expected: PASS. Then search at 1400px: results are readable before any click; clicking one returns the rail to its reading width.

- [ ] **Step 5: Commit**

```bash
git add assets/css src/web/templates/search.html src/web/ui.rs
git commit -m "feat(search): the rail takes the width nothing else is using"
```

---

### Task 14: Judge's answers stop being twenty-three cards away

**Files:**
- Modify: `src/web/templates/_judge_card.html:70-92`
- Modify: `assets/css/42-judge.css`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_judges_own_answers_are_reachable_without_scrolling_past_the_choices() {
        let html = render_judge_card_fixture(23);
        let actions = html.find("None of these").expect("no action row");
        let last = html.rfind("choice-").expect("no choices");
        assert!(actions < last, "the actions still sit below every choice: {actions} vs {last}");
    }
```

If moving the row in the DOM is wrong for the keyboard order, assert on the sticky class instead and keep the DOM order.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::judge::tests::the_judges_own_answers_are_reachable`
Expected: FAIL.

- [ ] **Step 3: Implement**

Wrap the three actions in a `.judge-actions` bar and make it `position: sticky; bottom: 0` with the page background behind it, so it is visible at every scroll position. The `N` / `S` / `X` shortcuts already work from anywhere; the bar makes that discoverable rather than replacing it.

Then give the page the width it has. `judge.html` declares no `regions` block, so it falls to `regions-focus` — one 68rem column, which rendered as content in the left half of a 1300px window with the right half empty. Switch it to `regions-table` (as `ops.html:3` already does) or give the candidate list two columns; whichever reads better with twenty-three cards. Do the same check on `settings.html`, which declares no block either.

- [ ] **Step 4: Run the tests, then look at it**

Run: `cargo test --lib web::judge`
Expected: PASS. Then open `/ui/judge` with a long candidate list and confirm the bar stays put.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/_judge_card.html assets/css/42-judge.css
git commit -m "feat(judge): the three answers stay on screen with the question"
```

---

### Task 15: The artifact pane stops fighting its own text

**Files:**
- Modify: `src/web/templates/_artifact_detail.html`
- Modify: `assets/css/40-search.css`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn delete_is_not_shoulder_to_shoulder_with_verified() {
        // Verified, Hide and Delete rendered as one flat row of equals.
        let html = render_artifact_detail_fixture();
        let bar = html.find("artifact-actions").expect("no action bar");
        let danger = html.find("artifact-danger").expect("delete is not set apart");
        assert!(bar < danger, "{html}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::delete_is_not_shoulder_to_shoulder`
Expected: FAIL — no `artifact-danger` grouping.

- [ ] **Step 3: Implement**

Three changes in the pane, each small:

1. Put `Delete` in its own `.artifact-danger` group, separated from `Verified`/`Hide`.
2. Make the title row `position: sticky; top: 0` inside the scrolling pane, so what you are reading keeps its name.
3. Move the `copy` control out of the text flow it currently overlaps — into the sticky title row, where it has somewhere to live that is not on top of the first paragraph.

- [ ] **Step 4: Run the tests, then look at it**

Run: `cargo test --lib web::ui`
Expected: PASS. Then open an artifact at both 1400px and 420px: `copy` never covers a word, the title stays while scrolling.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/_artifact_detail.html assets/css/40-search.css
git commit -m "fix(ui): copy off the text, the name kept, and delete set apart"
```

---

### Task 16: A passage cut mid-sentence says where the rest is

**Files:**
- Modify: `src/web/ui.rs` (`build_artifact_detail`, near `ui.rs:2963`)
- Modify: `src/web/templates/_artifact_detail.html`
- Test: `src/web/ui.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_passage_that_stops_mid_sentence_points_at_its_continuation() {
        // The pane ended "…der bereits vorgestellte Einsatz von" while the
        // source beside it showed the rest of the sentence.
        let d = build_detail_fixture_ending("Die erste Vorkehrung ist der bereits vorgestellte Einsatz von");
        assert!(d.continues_at.is_some(), "no way onward from a cut sentence");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_passage_that_stops_mid_sentence`
Expected: FAIL — no such field.

- [ ] **Step 3: Implement**

Add `continues_at: Option<String>` to the detail struct, set to the id of the next artifact of the same corpus by ordinal when this one's text does not end on sentence-final punctuation. Render it as a quiet link at the foot of the artifact: "continues in the next passage". Comment why the test is on punctuation rather than on the chunker: the pane cannot know whether a boundary was semantic, but it can tell that a sentence did not finish.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs src/web/templates/_artifact_detail.html
git commit -m "feat(ui): a sentence that does not finish says where it goes on"
```

---

### Task 17: Elapsed time stops being borrowed from a future-tense helper

**Files:**
- Modify: `src/web/ui.rs:331` (add `fmt_elapsed` beside `fmt_duration`), `ui.rs:2293`
- Test: `src/web/ui.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `pub fn fmt_elapsed(secs: i64) -> String`. `fmt_duration` is untouched — other callers depend on its future tense.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_sweep_that_took_no_time_does_not_say_it_happens_now() {
        // Every row of Housekeeping's TOOK column read "now", because the
        // column spends `fmt_duration`, which answers "when does this run
        // next" — "now", "in 5m" — not "how long did this take".
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(3), "3s");
        assert_eq!(fmt_elapsed(75), "1m 15s");
        assert_eq!(fmt_elapsed(3600), "1h 0m");
        // And the future-tense helper keeps its own meaning.
        assert_eq!(fmt_duration(0), "now");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_sweep_that_took_no_time`
Expected: FAIL — `cannot find function fmt_elapsed`.

- [ ] **Step 3: Implement**

```rust
/// How long something took, past tense.
///
/// `fmt_duration` above answers a different question — when does this run
/// next — and says "now" for zero and "in 5m" for three hundred. Housekeeping
/// spent it on the TOOK column, so every sweep in the history claimed to have
/// taken "now".
pub fn fmt_elapsed(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}
```

At `ui.rs:2293`, call `fmt_elapsed` instead of `fmt_duration`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs
git commit -m "fix(ops): how long a sweep took, in the tense that question is asked in"
```

---

### Task 18: Housekeeping reads like a status, not a paragraph

**Files:**
- Modify: `src/web/templates/ops.html`
- Modify: `assets/css/43-ops.css`
- Modify: `src/web/ui.rs` (a display name per sweep stage)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_ops_row_shows_no_markup_in_a_title() {
        // "**Was nicht abgedeckt ist:** *" was a row heading on the
        // deployment.
        let html = render_ops_fixture_with_merged_title("**Was nicht abgedeckt ist:** * Es werden keine");
        assert!(!html.contains("**"), "{html}");
    }

    #[test]
    fn a_sweep_stage_reads_as_words_and_keeps_its_identifier() {
        assert_eq!(sweep_label("arm_dedupe"), "Arming dedupe");
        assert_eq!(sweep_label("consolidate"), "Consolidating");
        assert_eq!(sweep_label("retention"), "Retention");
        // An identifier nobody has worded yet is shown, not swallowed.
        assert_eq!(sweep_label("some_new_sweep"), "some_new_sweep");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_sweep_stage_reads_as_words`
Expected: FAIL — `cannot find function sweep_label`.

- [ ] **Step 3: Implement**

Add `sweep_label`, mapping the known stages and returning the raw identifier unchanged for anything else — a new sweep must never silently render as nothing. Put the raw name in the row's `title` attribute so it stays greppable. Then break the opening counts paragraph into a stat row (`43-ops.css`), one figure per cell with its label under it.

The Merged and Generated tables on this page list artifacts by title, and those titles carry their own markup — the deployment showed `**Was nicht abgedeckt ist:** *` as a row heading. Pass them through `markdown::stand_in_title` (Task 1) the same way every other list now does, so the page stops being the last place raw markdown reaches a reader.

- [ ] **Step 4: Run the tests, then look at the page**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/ops.html assets/css/43-ops.css src/web/ui.rs
git commit -m "feat(ops): the counts as figures, and the sweeps in words"
```

---

### Task 19: One name for the page, and a 404 that belongs to the app

**Files:**
- Modify: `src/web/ui.rs` (router, near `ui.rs:3044`)
- Create: `src/web/templates/not_found.html`
- Test: `src/web/ui.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn housekeeping_is_one_name_and_one_url() {
        // The nav says Housekeeping, the URL says /ui/ops, the title says Ops,
        // and /ui/housekeeping was the browser's own error page.
        let r = get("/ui/housekeeping").await;
        assert_eq!(r.status(), 308);
        assert_eq!(r.headers()["location"], "/ui/ops");
    }

    #[tokio::test]
    async fn an_unknown_ui_path_gets_the_apps_own_page() {
        let r = get("/ui/nothing-here").await;
        assert_eq!(r.status(), 404);
        let body = body_string(r).await;
        assert!(body.contains("engram"), "the browser's error page, not ours: {body}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib web::ui::tests::housekeeping_is_one_name web::ui::tests::an_unknown_ui_path`
Expected: FAIL — 404 from the router with no body, and no redirect route.

- [ ] **Step 3: Implement**

Add a permanent redirect from `/ui/housekeeping` to `/ui/ops`, and a router `.fallback` rendering a `not_found.html` that extends `layout.html` — so a mistyped URL still has the nav, and a way back. Settle the naming in one direction: keep the URL, and make the page title match the nav word rather than the route.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs src/web/templates/not_found.html
git commit -m "feat(ui): one name for housekeeping, and our own page for a wrong turn"
```

---

### Task 20: An empty table says it is empty

**Files:**
- Modify: `src/web/templates/settings.html:27-56`
- Test: `src/web/ui.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_token_table_with_no_tokens_says_so_instead_of_showing_its_headings() {
        // Five column headings over nothing is a table pretending to have
        // rows.
        let html = render_settings_fixture(vec![]);
        assert!(!html.contains("Minted by"), "{html}");
        assert!(html.contains("No tokens yet"), "{html}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_token_table_with_no_tokens`
Expected: FAIL — headings render unconditionally.

- [ ] **Step 3: Implement**

Wrap the `<table>` in `{% if !tokens.is_empty() %}`, with an `{% else %}` saying there are none yet and what minting one is for. This is the same reasoning `_decide.html` already states at its top — "the old Ops page answered five headings with 'None.' and made an empty base look like a backlog".

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/settings.html src/web/ui.rs
git commit -m "fix(settings): no column headings over an empty table"
```

---

### Task 21: A gap's state stops looking like a button

**Files:**
- Modify: `src/web/templates/_gaps.html`
- Modify: `assets/css/41-capture.css`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_gaps_row_tells_its_state_apart_from_its_actions() {
        // "asked", "ask again" and "covered" rendered as three identical
        // ghost controls, one of which was not a control at all.
        let html = render_gaps_fixture();
        assert!(html.contains("gap-state"), "the state is styled as an action: {html}");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib web::ui::tests::a_gaps_row_tells_its_state_apart`
Expected: FAIL.

- [ ] **Step 3: Implement**

Give the state word (`asked`, `nothing near`, `pursued`, `judged`) a `.gap-state` badge class distinct from `.btn`, leaving `ask again` and `covered` as the only two things that look pressable. Also give `not yet grouped` a sentence saying what it means — the sweep has not run yet, and these are questions the base did not answer.

- [ ] **Step 4: Run the tests, then look at the page**

Run: `cargo test --lib web::ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/_gaps.html assets/css/41-capture.css src/web/ui.rs
git commit -m "fix(capture): what a gap is, beside what you can do about it"
```

---

### Task 22: The whole suite, and a look at the running app

**Files:** none

- [ ] **Step 1: Run everything**

Run: `cargo test`
Expected: PASS, with no test disabled or deleted along the way.

- [ ] **Step 2: Run clippy and the formatter**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 3: Drive the app**

Start it against a copy of a real base and walk the same path the review took: a search, an artifact, a full ask, capture, judge, housekeeping, settings — at 1400px and at 420px, in both themes. Confirm each finding this plan claims to fix is fixed, and that nothing in the "left as they are" list moved.

- [ ] **Step 4: Commit anything the walk turned up**

```bash
git add -A
git commit -m "fix(ui): what the walk through the running app turned up"
```
