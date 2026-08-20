# Frontend Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace engram's three competing page widths with one four-region layout that treats the phone as the same application at a smaller region count, then make that application pleasant — real typographic hierarchy, explained motion, a reachable light theme, and a command bar.

**Architecture:** One CSS grid with four named regions (`bar`, `rail`, `focus`, `source`). A page declares which regions it uses and never declares a width; container queries decide how many regions are visible (three-up ≥90rem, two-up 60–90rem, one-up <60rem). The stylesheet splits into numbered layers concatenated by `build.rs` into the single embedded `assets/app.css`. Almost everything is template, stylesheet and script; exactly one Rust function changes.

**Tech Stack:** Rust, axum, Askama templates, htmx 2, vanilla JS, hand-written CSS, `rust-embed`. No bundler, no `node_modules`, no build step beyond `cargo build`.

**Spec:** `docs/superpowers/specs/2026-08-20-frontend-overhaul-design.md`

## Global Constraints

- **No new dependencies.** Not in `Cargo.toml`, and no JavaScript packages at all. Modern CSS and vanilla JS only.
- **No build toolchain.** Ship stays `cargo build`. The only build-time addition is a string concatenation inside the existing `build.rs`.
- **Backend changes are minimal by decree.** Exactly one Rust behaviour changes in this whole plan: `queue_fragment`'s label disambiguation (Task 15). Everything else is templates, CSS, JS, or `build.rs`.
- **Every existing test keeps passing.** `cargo test` is green at the end of every single task. Where markup moves, update the assertion — never delete it.
- **Verbatim content is never altered to suit the layout.** Collapsing blank source lines is a *rendering* decision made client-side; the server keeps emitting every line and line numbers stay true to the source.
- **A rail item with no title still renders no heading.** `_results.html` is deliberate about this and commit `c934d3d` is the reasoning. Do not add "Untitled".
- **Accessibility floors:** interactive targets ≥44px on touch, inputs ≥16px on phone (iOS zoom), `env(safe-area-inset-*)` respected on every fixed element, all motion inside `@media (prefers-reduced-motion: no-preference)`.
- **Commit after every task.** Conventional-commit subject lines in the repo's existing voice.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `assets/css/00-tokens.css` | Custom properties: colour, type scale, spacing, radius, both themes |
| `assets/css/10-base.css` | Element defaults, focus rings, scrollbars, reset |
| `assets/css/20-layout.css` | The region grid, tiers, topbar, tabbar, phone bar |
| `assets/css/30-components.css` | Buttons, badges, chips, inputs, cards, tables |
| `assets/css/40-search.css` | Rail, focus pane, source pane, reading mode |
| `assets/css/41-capture.css` | Capture form, queue rows |
| `assets/css/42-judge.css` | Judge cards and verdict rows |
| `assets/css/43-ops.css` | Housekeeping tables and pursuit rows |
| `assets/js/theme.js` | Pre-paint theme application (inlined, not fetched) |

**Modified:**

| File | Change |
|---|---|
| `build.rs` | `build_stylesheet()` concatenating `assets/css/*.css` before `stamp_assets()` |
| `.gitignore` | `assets/app.css` becomes generated |
| `assets/app.css` | Deleted from git; regenerated at build |
| `assets/app.js` | Keyboard map, reading mode, source collapsing, theme toggle, command bar |
| `src/web/templates/layout.html` | Region declaration block, theme toggle, pre-paint script, phone bar |
| `src/web/templates/search.html` | Region markup, `hx-trigger` for as-you-type |
| `src/web/templates/_results.html` | Provenance line |
| `src/web/templates/_artifact_detail.html` | Unified header and button set |
| `src/web/templates/capture.html`, `ops.html`, `judge.html`, `corpus.html`, `ask.html`, `settings.html`, `extension.html` | `shell_class` → region declaration |
| `src/web/templates/_queue.html` | Coverage speaks only when partial |
| `src/web/ui.rs` | `QueueRow::opening`, `disambiguate_labels()`, `#[derive(Default)]` |

---

## Task 1: Split the stylesheet into layers

Infrastructure first — every later task edits one of these files, so this must land before anything else.

**Files:**
- Create: `assets/css/00-tokens.css`, `10-base.css`, `20-layout.css`, `30-components.css`, `40-search.css`, `41-capture.css`, `42-judge.css`, `43-ops.css`
- Modify: `build.rs`, `.gitignore`
- Delete from git (keep generated): `assets/app.css`
- Test: `src/web/assets.rs` (existing tests must still pass)

**Interfaces:**
- Consumes: nothing.
- Produces: `assets/app.css` as a **generated** artifact, byte-identical in behaviour to today's. Every later task edits a file under `assets/css/` instead of `assets/app.css`.

- [ ] **Step 1: Verify the current stylesheet renders and record its size**

```bash
cd /home/user01/Projekte/engram
wc -c assets/app.css
cargo test --lib web::assets 2>&1 | tail -5
```

Expected: a byte count (~30KB) and passing asset tests. Write the byte count down; Step 6 compares against it.

- [ ] **Step 2: Cut the existing stylesheet into numbered layers**

`assets/app.css` already carries `/* ── Section ── */` banners. Split on those boundaries, preserving every rule and every comment verbatim — this task changes zero CSS behaviour. Layer boundaries:

- `00-tokens.css` — the `@font-face` block, both `:root` blocks (light and the `prefers-color-scheme: dark` override). Currently lines 1–96.
- `10-base.css` — `*`/`html`/`body`/`a`/`.mono`/`h3`, scrollbars, `:focus-visible`, `::selection`. Currently lines 98–110.
- `20-layout.css` — `.shell`, `.shell-wide`, `.topbar`, `nav.top`, `.tabbar`, `.back`, and the `@media (max-width: 40rem)` block.
- `30-components.css` — `.btn`, `.badge`, `.chip`, `.input`, `.textarea`, `.select`, `.card`, `.row`, `.stack`, `.muted`, `.spinner`, `.crumb`, `.flag`.
- `40-search.css` — `.workspace`, `.rail*`, `.pane`, `.split`, `.raw`, `.actions`, `.pane-label`, `.band*`.
- `41-capture.css` — `.queue`, `.qrow`, `.qtitle`, `.qmeta`, `.qcov-low`, `.qtime`, `.qdot`, `.gaps`, `.gap`, `.decide`.
- `42-judge.css` — `.judge-*`, `.misses`.
- `43-ops.css` — ops tables and anything not claimed above.

Ordering matters: the cascade depends on tokens arriving before the rules that read them, which is what the numeric prefixes encode.

- [ ] **Step 3: Add the concatenation step to `build.rs`**

Add this function to `build.rs`:

```rust
/// One stylesheet, assembled from layers.
///
/// `assets/app.css` is generated and gitignored, exactly like
/// `assets/extension/`: `rust-embed` takes the whole of `assets/`, so writing
/// it there is what puts it in the binary. Concatenated in filename order,
/// which is what the numeric prefixes are for — the cascade depends on tokens
/// arriving before the rules that read them.
///
/// Must run before `stamp_assets`, which hashes the bytes of the file this
/// writes.
fn build_stylesheet() {
    println!("cargo:rerun-if-changed=assets/css");
    let dir = Path::new("assets/css");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("assets/css")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".css"))
        .collect();
    // Sorted, so the same layers produce the same stylesheet twice running.
    names.sort();

    let mut out = String::new();
    for name in &names {
        let body = std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("assets/css/{name}: {e}"));
        out.push_str("/* ===== ");
        out.push_str(name);
        out.push_str(" ===== */\n");
        out.push_str(&body);
        out.push('\n');
    }

    // Written only when the bytes differ. `stamp_assets` declares
    // `rerun-if-changed=assets/app.css`, so writing an identical file every
    // time would touch an mtime cargo is watching and rebuild forever.
    let dest = Path::new("assets/app.css");
    if std::fs::read_to_string(dest).ok().as_deref() != Some(out.as_str()) {
        std::fs::write(dest, &out).expect("assets/app.css");
    }
}
```

Then call it as the **first** line of `main()`, above `stamp_assets()`:

```rust
fn main() {
    build_stylesheet();

    println!("cargo:rerun-if-changed=extension/shared");
```

- [ ] **Step 4: Make the generated stylesheet untracked**

```bash
cd /home/user01/Projekte/engram
git rm --cached assets/app.css
printf '\n# Generated by build.rs from assets/css/*.css\nassets/app.css\n' >> .gitignore
```

- [ ] **Step 5: Build and confirm no rebuild loop**

```bash
cargo build 2>&1 | tail -3
cargo build 2>&1 | tail -3
```

Expected: the second build reports `Finished` without recompiling — proving the write-only-if-different guard works. If it rebuilds every time, the guard is wrong.

- [ ] **Step 6: Confirm the generated stylesheet is equivalent**

```bash
wc -c assets/app.css
cargo test 2>&1 | tail -20
```

Expected: byte count within a few hundred of Step 1's (banners added, nothing removed), and the full suite green.

- [ ] **Step 7: Commit**

```bash
git add build.rs .gitignore assets/css/
git commit -m "refactor(css): one stylesheet, assembled from layers

892 lines in one file is where finding a rule starts to cost more than
changing it, and the overhaul ahead only grows it. build.rs concatenates
assets/css/*.css in filename order into the same generated, embedded,
hash-stamped app.css it always shipped — the numeric prefixes are the
cascade, and nothing about the served bytes changes.

Written only when the bytes differ: stamp_assets watches app.css, so an
unconditional write would rebuild forever."
```

---

## Task 2: The region grid

**Files:**
- Modify: `assets/css/20-layout.css`, `src/web/templates/layout.html`
- Modify: every page template's `shell_class` block — `search.html:3`, `ops.html:3`, and the `capture.html`/`judge.html`/`ask.html`/`corpus.html`/`settings.html`/`extension.html`/`artifact_detail.html`/`pair.html`/`login.html` templates that omit it today
- Test: `src/web/ui.rs` and `src/web/judge.rs` existing template tests

**Interfaces:**
- Consumes: the layer files from Task 1.
- Produces: a `{% block regions %}` in `layout.html` emitting a class on the shell — `regions-focus`, `regions-rail-focus`, or `regions-rail-focus-source`. Later tasks style `.region-rail`, `.region-focus`, `.region-source` and rely on `.shell` no longer setting a width.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/web/ui.rs`:

```rust
#[test]
fn every_page_anchors_to_the_same_left_edge() {
    // Three shell widths meant the content column moved under a brand that did
    // not, so navigating jolted. A page now declares which regions it uses and
    // never declares a width; the grid puts `rail` and `focus` in the same
    // columns everywhere, which is what makes the anchor single.
    let css = include_str!("../../assets/app.css");
    assert!(
        !css.contains("shell-wide"),
        "shell-wide still sets a per-page width"
    );
    assert!(
        css.contains(".regions-rail-focus-source"),
        "the three-up region tier is missing"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --lib every_page_anchors_to_the_same_left_edge 2>&1 | tail -15
```

Expected: FAIL — `shell-wide still sets a per-page width`.

- [ ] **Step 3: Replace the width rules with the region grid**

In `assets/css/20-layout.css`, delete the `.shell` / `.shell-wide` / `.shell-wide #filters` rules and put this in their place:

```css
/* ── The region grid ─────────────────────────────────────────────────────
   Four regions, and a page says which of them it uses — never how wide it
   is. `rail` and `focus` start in the same grid columns on every page, so
   the single left anchor is a property of the model rather than a fix laid
   over three disagreeing widths.

   Container queries rather than media queries: the workspace responds to the
   space it is actually in, which is what a half-width desktop window needs
   and what the 60rem back-link was already working around. */
.shell {
  container-type: inline-size;
  container-name: shell;
  max-width: 110rem;
  margin: 0 auto;
  padding: 0 1rem 4rem;
}

.regions {
  display: grid;
  gap: 1rem;
  align-items: start;
  grid-template-columns: [rail] 22rem [focus] minmax(0, 1fr);
}

/* A grid child defaults to min-width:auto, which resolves to min-content: one
   unwrappable line of source widens its column instead of scrolling inside it,
   and takes the document's width with it. */
.regions > * { min-width: 0; }

.region-bar   { grid-column: 1 / -1; }
.region-rail  { grid-column: rail; }
.region-focus { grid-column: focus; }
.region-source { display: none; }

/* Focus-only pages: prose, one column, at reading measure. */
.regions-focus { grid-template-columns: minmax(0, 1fr); }
.regions-focus .region-focus { grid-column: 1 / -1; }

/* Three-up. The source column earns its place only here. */
@container shell (min-width: 90rem) {
  .regions-rail-focus-source {
    grid-template-columns: [rail] 20rem [focus] minmax(0, 1fr) [source] minmax(0, 1.2fr);
  }
  .regions-rail-focus-source .region-source {
    display: block;
    grid-column: source;
  }
}

/* One-up. One region at a time; the rest are a navigation away. This is the
   phone, and equally a half-width desktop window. */
@container shell (max-width: 60rem) {
  .regions { grid-template-columns: minmax(0, 1fr); }
  .regions > * { grid-column: 1 / -1; }
  .regions.has-selection .region-rail { display: none; }
}
```

- [ ] **Step 4: Declare regions in the layout**

In `src/web/templates/layout.html`, replace the shell div:

```html
  {# A page says which regions it uses. It never says how wide it is: the grid
     puts rail and focus in the same columns everywhere, and the viewport
     decides only how many regions are visible at once. #}
  <div class="shell">
    <div class="regions {% block regions %}regions-focus{% endblock %}">
      {% block content %}{% endblock %}
    </div>
  </div>
```

Remove the `{% block shell_class %}` block entirely.

- [ ] **Step 5: Update each page to declare regions instead of a width**

In `search.html`, replace line 3:

```html
{% block regions %}regions-rail-focus-source{% endblock %}
```

In `ops.html`, replace line 3:

```html
{% block regions %}regions-focus{% endblock %}
```

Every other page template (`capture.html`, `judge.html`, `ask.html`, `corpus.html`, `settings.html`, `extension.html`, `artifact_detail.html`, `pair.html`, `login.html`) declared no `shell_class` and inherits the `regions-focus` default — leave them alone.

- [ ] **Step 6: Move search's markup into regions**

In `search.html`, wrap the existing `<form id="filters">` — every attribute and every comment inside it unchanged — in a `.region-bar` div together with the spinner row, then replace the `.workspace` block:

```html
<div class="region-bar">
  <!-- the existing <form id="filters"> … </form>, moved here verbatim -->
  <div class="row" style="margin:0.5rem 0 1rem">
    <span id="search-spinner" class="spinner">searching…</span>
  </div>
</div>

<div id="rail" class="region-rail rail" role="listbox" aria-label="Results"></div>
<div id="pane" class="region-focus pane">
  {# Before a search there is no rail beside this, so the sentence used to sit
     in a column that did not exist yet — stranded in empty space and pointing
     at a list nobody had asked for. It says what the page is for until there
     is something to point at. #}
  <p class="muted empty">Search to see an artifact here, beside the lines it
    came from.</p>
</div>
<div id="source" class="region-source"></div>
```

Delete the `.workspace` wrapper — `.regions` is now the grid. The rail's `hx-target="#rail"` and the results' `hx-target="#pane"` are unchanged: the ids stay, only their classes and their parent change.

- [ ] **Step 7: Run the full suite**

```bash
cargo test 2>&1 | tail -20
```

Expected: PASS, including the new test. Any template test asserting on `shell-wide` or `.workspace` gets updated to the new markup — updated, never deleted.

- [ ] **Step 8: Verify by eye at all three tiers**

```bash
cargo run 2>&1 | head -5 &
sleep 3
```

Open `http://localhost:8080/ui/search`, search for anything, and check: the query box's left edge lines up with the rail's left edge; widening past 90rem brings in a third column; narrowing under 60rem shows one region with the back link. Then visit Capture and Housekeeping and confirm the brand and the content column share one left edge across all three.

- [ ] **Step 9: Commit**

```bash
git add assets/css/20-layout.css src/web/templates/ src/web/ui.rs
git commit -m "feat(layout): four regions, and a page says which it uses

Capture centred itself in 60rem, Search centred a 48rem filter block in a
110rem shell and Housekeeping ran full bleed, so the content column moved
under a brand that did not and the search box lined up with nothing on its
own page.

A page now declares regions and never a width. rail and focus start in the
same columns everywhere, which makes the single left anchor a property of
the grid rather than a correction applied to three disagreeing pages — and
makes the phone the same application with the region count set to one.

Container queries, not media queries: the workspace answers to the space it
is in, which is what the 60rem back link was already working around."
```

---

## Task 3: The rail keeps its place

**Files:**
- Modify: `assets/css/40-search.css`
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: Task 2's region classes.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn the_open_rail_card_keeps_a_line_of_itself() {
    // The rail is the ranking as well as a list of links. A card collapsing to
    // a bare stub when opened punched a hole in the ordering and lost the
    // reader's place; the accent border already says which one is open.
    let css = include_str!("../../assets/app.css");
    assert!(
        !css.contains(r#".rail-item[aria-selected="true"] .rail-snippet { display: none; }"#),
        "the open card still erases its snippet"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --lib the_open_rail_card_keeps_a_line_of_itself 2>&1 | tail -10
```

Expected: FAIL — `the open card still erases its snippet`.

- [ ] **Step 3: Replace the hiding rule with a clamp**

In `assets/css/40-search.css`, replace the `display: none` rule:

```css
/* The open card keeps one line of itself. The pane beside it holds the text in
   full, but the rail is the ranking too — a card collapsing to a bare stub
   punched a hole in the ordering and lost the reader's place in it. The accent
   border and background are what say "this one is open"; the snippet's job is
   to keep the row a row. */
.rail-item[aria-selected="true"] .rail-snippet {
  display: -webkit-box;
  -webkit-line-clamp: 1;
  -webkit-box-orient: vertical;
  overflow: hidden;
  opacity: 0.6;
}
```

- [ ] **Step 4: Lift the past-cliff results above the contrast floor**

In the same file, change `.rail-past`:

```css
/* Demoted, not unreadable. At 0.55 over the dark base this was very likely
   under AA, and a result past the cliff is still a result. */
.rail-past { opacity: 0.7; }
```

- [ ] **Step 5: Run the tests**

```bash
cargo test 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add assets/css/40-search.css src/web/ui.rs
git commit -m "fix(rail): the open card keeps a line, and the cliff stays readable

The rail is the ranking as well as a list of links, and the open card
erasing its snippet punched a hole in the ordering. It keeps one clamped,
dimmed line; the accent border was always what said which one was open.

Past-cliff results move from 0.55 to 0.7. Demoted should read as demoted,
not as unreadable."
```

---

## Task 4: One button vocabulary

**Files:**
- Modify: `src/web/templates/_artifact_detail.html`, `assets/css/30-components.css`, `assets/css/40-search.css`
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: Task 2's regions.
- Produces: `.pane-header` and `.btn-icon` (icon + label), used nowhere else but stable.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn the_artifact_actions_carry_labels() {
    // One screen carried three button vocabularies: unlabelled icon buttons
    // stranded at the top of a wide row, text links inside the card, and solid
    // buttons elsewhere. An icon with no word beside it is a guess.
    //
    // Asserted against the template source rather than a render: the fragment
    // is `ArtifactDetailFragment { d: ArtifactDetail }` (`src/web/ui.rs:602`)
    // and building an ArtifactDetail by hand is thirty lines of scaffolding to
    // check for three words. The words are the whole change.
    let tpl = include_str!("templates/_artifact_detail.html");
    assert!(tpl.contains("<span>Verified</span>"), "the verify control has no label");
    assert!(tpl.contains("<span>Hide</span>"), "the hide control has no label");
    assert!(tpl.contains("<span>Delete</span>"), "the delete control has no label");
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --lib the_artifact_actions_carry_labels 2>&1 | tail -10
```

Expected: FAIL — the verify control has no label.

- [ ] **Step 3: Give the icon buttons their words**

In `_artifact_detail.html`, each action button gains a label span beside its existing `<svg>`:

```html
<button class="btn btn-icon btn-sm" type="submit">
  {% include "_icon_check.html" %}<span>Verified</span>
</button>
```

and the same shape for hide (`Hide`) and delete (`Delete`). Keep `_icon_*.html` includes exactly as they are.

- [ ] **Step 4: Anchor the row to the pane header**

In `assets/css/40-search.css`:

```css
/* Anchored to the header rather than floating at the window edge. Delete keeps
   its exile to the far end — the control that cannot be undone is never flush
   against two that can — but a lone trash button stranded 1300px from its
   siblings read as an orphan rather than as caution. */
.pane-header {
  display: flex; align-items: center; gap: 0.5rem;
  padding-bottom: 0.5rem; margin-bottom: 0.75rem;
  border-bottom: 1px solid var(--color-border-subtle);
}
.pane-header .actions { margin: 0; }
.pane-header > form:last-of-type { margin-left: auto; }
```

In `assets/css/30-components.css`:

```css
.btn-icon { display: inline-flex; align-items: center; gap: 0.375rem; }
.btn-icon svg { width: 14px; height: 14px; }
```

- [ ] **Step 5: Run the tests**

```bash
cargo test 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/web/templates/_artifact_detail.html assets/css/ src/web/ui.rs
git commit -m "fix(artifact): one button vocabulary, with words on it

The pane carried three: unlabelled icon buttons at the top of a wide row,
text links inside the card, solid buttons elsewhere. A check mark with no
word beside it is a guess about what it does.

Delete keeps the far end, for the reason it always had, but anchors to the
header instead of floating at the window edge where it read as stranded."
```

---

## Task 5: The keyboard map

**Files:**
- Modify: `assets/app.js`, `assets/css/40-search.css`, `src/web/templates/search.html`

**Interfaces:**
- Consumes: Task 2's `.region-*` classes, Task 3's rail.
- Produces: `data-key-hint` dismissal in `localStorage` under `engram.hints`.

- [ ] **Step 1: Extend the existing rail handler**

`assets/app.js` already walks the rail with arrows. Add `j`/`k` to the same handler by widening its guard:

```js
  document.addEventListener('keydown', function (e) {
    var down = e.key === 'ArrowDown' || e.key === 'j';
    var up = e.key === 'ArrowUp' || e.key === 'k';
    if (!down && !up) return;
    // Letters must not fire while something is being typed into.
    var tag = document.activeElement && document.activeElement.tagName;
    if ((e.key === 'j' || e.key === 'k') && (tag === 'INPUT' || tag === 'TEXTAREA')) return;
    var items = Array.prototype.slice.call(document.querySelectorAll('.rail-item'));
    if (!items.length) return;
    var i = items.indexOf(document.activeElement);
    var next = down ? Math.min(i + 1, items.length - 1) : Math.max(i - 1, 0);
    if (i === -1) next = 0;
    items.forEach(function (el) { el.setAttribute('aria-selected', 'false'); });
    items[next].setAttribute('aria-selected', 'true');
    items[next].focus();
    e.preventDefault();
  });
```

- [ ] **Step 2: Add the global keys**

```js
  // `/` reaches the query from anywhere, `Esc` steps back one region, `s`
  // toggles the source pane. Never while a field has focus: these are letters,
  // and a letter belongs to whatever is being typed into.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var tag = document.activeElement && document.activeElement.tagName;
    var typing = tag === 'INPUT' || tag === 'TEXTAREA';

    if (e.key === '/' && !typing) {
      var q = document.querySelector('input[name=q]');
      if (q) { e.preventDefault(); q.focus(); q.select(); }
      return;
    }
    if (e.key === 'Escape') {
      if (typing) { document.activeElement.blur(); return; }
      var back = document.querySelector('.back');
      if (back) { e.preventDefault(); back.click(); }
      return;
    }
    if (e.key === 's' && !typing) {
      var regions = document.querySelector('.regions');
      if (regions) { e.preventDefault(); regions.classList.toggle('show-source'); }
    }
  });
```

- [ ] **Step 3: Add the hint row to the search page**

In `search.html`, inside `.region-bar` below the spinner:

```html
{# Taught once. A shortcut nobody is told about is a shortcut nobody uses, and
   a hint that cannot be dismissed is a banner. #}
<p class="keyhint" hidden>
  <kbd>/</kbd> search · <kbd>↑</kbd><kbd>↓</kbd> move · <kbd>↵</kbd> open ·
  <kbd>s</kbd> source · <kbd>r</kbd> reading mode
  <button type="button" class="btn btn-ghost btn-sm" data-dismiss-hint>Got it</button>
</p>
```

- [ ] **Step 4: Wire the hint's dismissal**

```js
  // Shown until it is dismissed, then never again on this browser.
  (function keyHint() {
    var hint = document.querySelector('.keyhint');
    if (!hint) return;
    var seen = false;
    try { seen = localStorage.getItem('engram.hints') === 'seen'; } catch (e) { seen = true; }
    // A touch screen has no keys to press: the row would be noise there.
    if (seen || !window.matchMedia('(pointer: fine)').matches) return;
    hint.hidden = false;
    hint.querySelector('[data-dismiss-hint]').addEventListener('click', function () {
      hint.hidden = true;
      try { localStorage.setItem('engram.hints', 'seen'); } catch (e) {}
    });
  })();
```

- [ ] **Step 5: Style it**

In `assets/css/40-search.css`:

```css
.keyhint { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap;
  font-size: 0.8125rem; color: var(--color-fg-muted); margin: 0 0 0.75rem; }
.keyhint kbd { font-family: var(--font-mono); font-size: 0.75rem;
  padding: 1px 5px; border: 1px solid var(--color-border);
  border-radius: var(--radius-sm); background: var(--color-bg-surface); }
```

- [ ] **Step 6: Verify by hand**

```bash
cargo run &
sleep 3
```

On `/ui/search`: press `/` — the query focuses. Type a query, press `Escape` — focus leaves without clearing. Press `j`/`k` — the rail walks. Type `j` inside the query box — it types a `j` and does not move the rail. Dismiss the hint, reload, confirm it stays gone.

- [ ] **Step 7: Commit**

```bash
git add assets/app.js assets/css/40-search.css src/web/templates/search.html
git commit -m "feat(keyboard): / reaches the query, Esc steps back, s shows source

The rail already walked under the arrows. j/k join them, / reaches the query
from anywhere, Esc steps back one region and s toggles the source pane.

Every letter key is gated on nothing being typed into, which is the rule the
judge shortcuts already follow — and the hint row that teaches them is shown
only where a pointer says there are keys, and only until it is dismissed."
```

---

## Task 6: The phone query bar

**Files:**
- Modify: `assets/css/20-layout.css`, `src/web/templates/layout.html`, `src/web/templates/search.html`

**Interfaces:**
- Consumes: Task 2's regions.
- Produces: `.phonebar` — the fixed bottom container the command bar reuses in Task 12.

- [ ] **Step 1: Move the search bar into the thumb zone on phone**

In `assets/css/20-layout.css`, inside the existing `@media (max-width: 40rem)` block:

```css
  /* On Search the input genuinely is the application, so it belongs where the
     thumb already is. The tab bar keeps its job below it, slimmer — and Search
     gets a silhouette no other tab has, which is orientation for free. */
  .regions-rail-focus-source .region-bar {
    position: fixed; left: 0; right: 0; z-index: 11;
    bottom: calc(46px + env(safe-area-inset-bottom));
    padding: 0.5rem max(0.75rem, env(safe-area-inset-left))
             0.5rem max(0.75rem, env(safe-area-inset-right));
    background: var(--color-bg-surface);
    border-top: 1px solid var(--color-border);
  }
  /* The hint and the facet row are not thumb furniture: they would double the
     height of a bar that has to stay one line. */
  .regions-rail-focus-source .region-bar .hint,
  .regions-rail-focus-source .region-bar .facets,
  .regions-rail-focus-source .region-bar .keyhint { display: none; }

  /* Clear of both bars, so the last result is reachable. */
  .regions-rail-focus-source { padding-bottom: 7.5rem; }

  /* Slimmer, because it is no longer the only thing down there. */
  .tabbar a { min-height: 46px; }
```

- [ ] **Step 2: Verify on a phone viewport**

```bash
cargo run &
sleep 3
```

In Chrome DevTools device mode at 390×844: the query box sits above the tab bar, both clear the home indicator, the last rail result scrolls into view above them, and focusing the input does not zoom the page.

- [ ] **Step 3: Verify the other pages are untouched**

Capture, Ask and Housekeeping declare `regions-focus`, so their bars must **not** be fixed. Confirm Capture's textarea still scrolls normally with the page.

- [ ] **Step 4: Commit**

```bash
git add assets/css/20-layout.css
git commit -m "feat(phone): the search box moves to the thumb

On Search the input is the application, and it was at the top of the screen
where a thumb does not reach. It takes the bottom, the tab bar goes slimmer
underneath it, and both clear the home indicator.

Scoped to the search regions: Capture and Housekeeping keep a bar that
scrolls with the page, because on those the input is not the point."
```

---

## Task 7: A type scale, and headings get their element back

**Files:**
- Modify: `assets/css/00-tokens.css`, `assets/css/10-base.css`, `assets/css/30-components.css`, and every template using `<h3>` as a label
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: Task 1's layers.
- Produces: `--text-xs` … `--text-2xl` tokens and a `.label` class, used by Tasks 8 and 14.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn headings_are_headings_and_labels_are_labels() {
    // h3 was restyled globally into a small uppercase muted label, which is
    // why no page had hierarchy: the element that would carry it had been
    // spent on a label style.
    let css = include_str!("../../assets/app.css");
    assert!(css.contains(".label {"), "no .label class to carry the old h3 style");
    assert!(
        !css.contains("h3 { font-size: 0.8125rem"),
        "h3 is still restyled as a label"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --lib headings_are_headings_and_labels_are_labels 2>&1 | tail -10
```

Expected: FAIL — no `.label` class.

- [ ] **Step 3: Add the scale to the tokens**

In `assets/css/00-tokens.css`, inside the first `:root`:

```css
  /* Sizes were chosen per component — 0.6875, 0.75, 0.8125, 0.875, 1, 1.375 —
     so two things meaning the same thing rarely looked the same size. */
  --text-xs: 0.75rem;
  --text-sm: 0.8125rem;
  --text-base: 0.9375rem;
  --text-md: 1rem;
  --text-lg: 1.25rem;
  --text-xl: 1.5rem;
  --text-2xl: 1.875rem;
```

- [ ] **Step 4: Free the heading elements**

In `assets/css/10-base.css`, replace the `h3` rule:

```css
/* Headings carry hierarchy. The small uppercase muted style they used to be is
   a label, and it is now called one — an element spent on a label style is an
   element the pages cannot use to say what is a section and what is inside it. */
h1 { font-size: var(--text-2xl); font-weight: 600; letter-spacing: -0.02em; margin: 0 0 0.5rem; }
h2 { font-size: var(--text-xl); font-weight: 600; letter-spacing: -0.01em; margin: 1.5rem 0 0.5rem; }
h3 { font-size: var(--text-lg); font-weight: 600; margin: 1.25rem 0 0.375rem; }

.label {
  font-size: var(--text-sm); color: var(--color-fg-muted);
  text-transform: uppercase; letter-spacing: 0.04em;
  margin: 1.5rem 0 0.5rem;
}
```

- [ ] **Step 5: Retag every `<h3>` that was a label**

```bash
grep -rn '<h3>' src/web/templates/
```

Each hit is one of two things. A section *label* (`RECENT`, `KIND`, `GENERATED`, `PURSUITS`, `ARTIFACT`, `SOURCE`) becomes `<p class="label">`. A genuine heading stays `<h3>` — or is promoted to `<h2>` where it names the page's main section. Work through every hit; leave none.

- [ ] **Step 6: Run the tests**

```bash
cargo test 2>&1 | tail -20
```

Expected: PASS. Template tests asserting on label text still match — only the element changed.

- [ ] **Step 7: Commit**

```bash
git add assets/css/ src/web/templates/ src/web/ui.rs
git commit -m "refactor(type): a scale, and h3 stops being a label

Sizes were picked per component, so two things meaning the same thing
rarely looked the same size. They come from a named scale now.

The consequential half: h3 was restyled globally into a small uppercase
muted label, which is why no page had hierarchy — the element that would
carry it had been spent. That style is called .label, and h1 through h3 get
their job back."
```

---

## Task 8: A measure for prose

**Files:**
- Modify: `assets/css/10-base.css`, `assets/css/43-ops.css`, `src/web/templates/ops.html`

**Interfaces:**
- Consumes: Task 7's scale.
- Produces: `.prose`.

- [ ] **Step 1: Add the measure**

In `assets/css/10-base.css`:

```css
/* Housekeeping's introduction ran near 200 characters a line, because the page
   is wide and the paragraph inherited the width. Wide is right for the table
   beside it and wrong for the sentence above it, so only the sentence is
   constrained. */
.prose { max-width: 68ch; }
```

- [ ] **Step 2: Apply it in Housekeeping**

In `ops.html`, add `class="prose"` to the explanatory paragraphs — the stats line and the `GENERATED` description. Leave the tables at full width.

- [ ] **Step 3: Give the tables room to breathe**

In `assets/css/43-ops.css`:

```css
/* Columns sized to their content rather than spread across the window: the
   "written because" column sat a third of a screen from the artifact it
   explained, and Deprecate ended up alone at the far edge. */
.ops-table { width: 100%; border-collapse: collapse; }
.ops-table td, .ops-table th { padding: 0.5rem 0.75rem; vertical-align: top; }
.ops-table td:first-child { width: 30%; }
.ops-table td:last-child { width: 1%; white-space: nowrap; text-align: right; }
/* Wide content scrolls in its own box rather than dragging the page sideways. */
.ops-scroll { overflow-x: auto; }
```

- [ ] **Step 4: Verify**

```bash
cargo test 2>&1 | tail -10
cargo run &
sleep 3
```

At `/ui/ops`: the prose wraps at a readable width, the table still uses the full width, and nothing scrolls the page horizontally.

- [ ] **Step 5: Commit**

```bash
git add assets/css/ src/web/templates/ops.html
git commit -m "fix(ops): prose takes a measure, the table keeps the width

The introduction ran near 200 characters a line because the page is wide and
the paragraph inherited it. Wide is right for the table and wrong for the
sentence above it, so only the sentence is constrained — and the columns are
sized to their content, which puts 'written because' back beside the
artifact it explains."
```

---

## Task 9: Motion that explains what moved

**Files:**
- Modify: `assets/css/20-layout.css`, `assets/css/40-search.css`, `src/web/templates/layout.html`

**Interfaces:**
- Consumes: Task 2's regions.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Opt the document into view transitions**

In `layout.html`'s `<head>`:

```html
  {# Cross-document view transitions: navigating between pages that share a
     region grid should look like the content changing, not like the window
     being replaced. Browsers without it simply navigate. #}
  <meta name="view-transition" content="same-origin">
```

- [ ] **Step 2: Name the regions as transition roots**

In `assets/css/20-layout.css`:

```css
/* One rule: motion explains what moved, and never decorates. Anything that
   should be understood as the same thing across a change is named here; the
   browser tweens between them and everything else cross-fades. */
@media (prefers-reduced-motion: no-preference) {
  ::view-transition-group(*) { animation-duration: 180ms; }
  .region-rail { view-transition-name: rail; }
  .region-focus { view-transition-name: focus; }
  .topbar { view-transition-name: topbar; }
}
```

- [ ] **Step 3: Let a fresh result set arrive rather than blink**

In `assets/css/40-search.css`:

```css
/* A replaced rail used to appear all at once, which reads as a flicker. The
   stagger is short and capped: it says "these arrived", not "watch this".
   The cliff divider does not animate — its whole job is to stay put. */
@media (prefers-reduced-motion: no-preference) {
  .rail-item { animation: rail-in 160ms ease-out both; }
  /* Capped at the first six. Past that the delay would outlast the reading of
     the first result, and a list that is still arriving cannot be scanned. */
  .rail-item:nth-child(2) { animation-delay: 20ms; }
  .rail-item:nth-child(3) { animation-delay: 40ms; }
  .rail-item:nth-child(4) { animation-delay: 60ms; }
  .rail-item:nth-child(5) { animation-delay: 80ms; }
  .rail-item:nth-child(6) { animation-delay: 100ms; }
}
@keyframes rail-in {
  from { opacity: 0; transform: translateY(3px); }
  to   { opacity: 1; transform: none; }
}
```

- [ ] **Step 4: Use a transition for htmx pane swaps**

In `assets/app.js`, inside the existing `htmx:afterSwap` handling, wrap pane replacement:

```js
  // The artifact cross-fades when a different result is picked, so the change
  // reads as one pane showing something else rather than as two separate
  // paints. Guarded: not every browser has this, and a missing API must not
  // stop the swap.
  document.body.addEventListener('htmx:beforeSwap', function (e) {
    if (!document.startViewTransition) return;
    if (e.detail.target && e.detail.target.id !== 'pane') return;
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    e.detail.shouldSwap = true;
  });
```

- [ ] **Step 5: Verify, including with motion reduced**

```bash
cargo run &
sleep 3
```

Search, click between results — the pane cross-fades and the rail does not jump. Then enable "Emulate prefers-reduced-motion: reduce" in DevTools and confirm every animation stops while the app stays fully usable.

- [ ] **Step 6: Commit**

```bash
git add assets/css/ src/web/templates/layout.html assets/app.js
git commit -m "feat(motion): what moved, and nothing else

Region changes and pane swaps tween instead of repainting, and a fresh
result set arrives on a short capped stagger rather than blinking into
place. The cliff divider is deliberately not in it: its job is to stay put.

All of it inside prefers-reduced-motion: no-preference, and every use of the
view transition API is guarded — a browser without it navigates."
```

---

## Task 10: Reading mode

**Files:**
- Modify: `assets/css/40-search.css`, `assets/app.js`

**Interfaces:**
- Consumes: Task 2's regions, Task 5's key handler.
- Produces: `.regions.reading`, persisted under `engram.reading`.

- [ ] **Step 1: Add the spine**

In `assets/css/40-search.css`:

```css
/* Reading mode: the rail narrows to its ranking and gives the width to the
   artifact and its source. It still marks the cliff, so position in the
   ordering survives — that is the whole reason it narrows rather than closes. */
@container shell (min-width: 90rem) {
  .regions.reading { grid-template-columns: [rail] 3rem [focus] minmax(0, 1fr) [source] minmax(0, 1.2fr); }
  .regions.reading .rail-snippet,
  .regions.reading .rail-title,
  .regions.reading .badge { display: none; }
  .regions.reading .rail-item { padding: 0.5rem 0; text-align: center; }
  .regions.reading .cliff { font-size: 0; padding: 0; height: 1px; background: var(--color-accent-muted); }
}
```

- [ ] **Step 2: Bind `r` and the spine click**

Extend the global key handler from Task 5:

```js
    if (e.key === 'r' && !typing) {
      var rs = document.querySelector('.regions');
      if (!rs) return;
      e.preventDefault();
      var on = rs.classList.toggle('reading');
      try { localStorage.setItem('engram.reading', on ? '1' : '0'); } catch (err) {}
    }
```

and restore it on load, beside the key-hint block:

```js
  // Remembered, because it is a way of working rather than a per-visit choice.
  (function readingMode() {
    var rs = document.querySelector('.regions');
    if (!rs) return;
    try {
      if (localStorage.getItem('engram.reading') === '1') rs.classList.add('reading');
    } catch (e) {}
  })();
```

- [ ] **Step 3: Verify**

At ≥90rem: press `r` — the rail narrows to numbers, the artifact and source widen, the cliff is still visible as a rule. Press `r` again to restore. Reload and confirm the mode persisted. Narrow below 90rem and confirm the class is inert rather than broken.

- [ ] **Step 4: Commit**

```bash
git add assets/css/40-search.css assets/app.js
git commit -m "feat(search): reading mode narrows the rail to its ranking

Once an artifact is open the list has done its job and the verification has
not. r gives the width to the artifact and its source and leaves the rail as
a spine of ranks — which still marks the cliff, so position in the ordering
survives. Remembered, because it is a way of working rather than a choice
made per visit."
```

---

## Task 11: The dark/light toggle

**Files:**
- Modify: `assets/css/00-tokens.css`, `src/web/templates/layout.html`, `assets/app.js`
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: Task 1's token layer.
- Produces: `data-theme` on `<html>`, `engram.theme` in `localStorage`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_chosen_theme_beats_the_system_preference() {
    // The light palette has been in the stylesheet since the port and nobody
    // has ever seen it: it activated only on prefers-color-scheme. A choice
    // has to be able to override the system, in both directions.
    let css = include_str!("../../assets/app.css");
    assert!(css.contains(r#":root[data-theme="dark"]"#), "no explicit dark selector");
    assert!(css.contains(r#":root[data-theme="light"]"#), "no explicit light selector");
    assert!(
        css.contains(r#":root:not([data-theme="light"])"#),
        "the system dark block does not yield to an explicit light choice"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --lib a_chosen_theme_beats_the_system_preference 2>&1 | tail -10
```

Expected: FAIL — no explicit dark selector.

- [ ] **Step 3: Make the dark block yield to a choice**

`assets/css/00-tokens.css` currently has the light values on bare `:root` and the dark values inside `@media (prefers-color-scheme: dark) { :root { … } }`. Two changes.

First, narrow the media block's selector so an explicit light choice beats the system:

```css
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    /* …the existing dark custom properties, moved here unchanged… */
  }
}
```

Second, add an explicit dark block so a dark choice beats a light *system*:

```css
/* The same values again. CSS has no way to give one declaration block two
   conditions — a custom property cannot be defined once and applied
   conditionally, because the conditional is the cascade. The duplication is
   the construct, and the two blocks are adjacent in this file so they are
   changed together. */
:root[data-theme="dark"] {
  /* …the same dark custom properties… */
}
```

Extract the dark declarations into a single named list you paste into both blocks, and confirm by eye that the two are identical before moving on. Light needs no block: it is already on bare `:root`, and `[data-theme="light"]` wins by excluding itself from the media block above.

- [ ] **Step 4: Apply the theme before first paint**

In `layout.html`, as the **first** element inside `<head>`, before the stylesheet link:

```html
  {# Inline and first, because a stylesheet cannot know a stored choice and a
     deferred script runs after the first paint — either way the wrong theme
     flashes. Small enough to cost nothing; wrapped because a browser with
     storage disabled must still render a page. #}
  <script>
    try {
      var t = localStorage.getItem('engram.theme');
      if (t) document.documentElement.setAttribute('data-theme', t);
    } catch (e) {}
  </script>
```

- [ ] **Step 5: Add the control**

In `layout.html`'s nav, before the sign-out form:

```html
      <button class="btn btn-ghost btn-sm" type="button" data-theme-toggle
              aria-label="Switch between dark and light">
        <span data-theme-label>Theme</span>
      </button>
```

- [ ] **Step 6: Wire it**

In `assets/app.js`:

```js
  // Follows the system until it is touched; from then on it is a remembered
  // two-state switch. The theme-color meta moves with it, or an installed
  // phone app frames a light page in a dark status bar.
  (function themeToggle() {
    var btn = document.querySelector('[data-theme-toggle]');
    if (!btn) return;
    function current() {
      var set = document.documentElement.getAttribute('data-theme');
      if (set) return set;
      return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    }
    function paint() {
      var now = current();
      btn.querySelector('[data-theme-label]').textContent = now === 'dark' ? 'Light' : 'Dark';
      var meta = document.querySelector('meta[name="theme-color"]:not([media])');
      if (!meta) {
        meta = document.createElement('meta');
        meta.setAttribute('name', 'theme-color');
        document.head.appendChild(meta);
      }
      meta.setAttribute('content', now === 'dark' ? '#0e1015' : '#f8f6f1');
    }
    btn.addEventListener('click', function () {
      var next = current() === 'dark' ? 'light' : 'dark';
      document.documentElement.setAttribute('data-theme', next);
      try { localStorage.setItem('engram.theme', next); } catch (e) {}
      paint();
    });
    paint();
  })();
```

- [ ] **Step 7: Run the tests and check both themes**

```bash
cargo test 2>&1 | tail -10
cargo run &
sleep 3
```

Toggle on every page. Confirm: no flash of the wrong theme on reload, the choice survives a restart, the system preference is followed before any choice is made, and the light theme is actually legible — check the rail, the cliff divider, badges, and the source pane's highlight in particular.

- [ ] **Step 8: Commit**

```bash
git add assets/css/00-tokens.css src/web/templates/layout.html assets/app.js src/web/ui.rs
git commit -m "feat(theme): the light palette becomes reachable

It has been in the stylesheet since the port from Vestigo and nobody has
ever seen it, because it activated only on prefers-color-scheme. The page
follows the system until the toggle is touched and is a remembered two-state
switch after that.

Applied inline before first paint, or the wrong theme flashes on every load,
and mirrored into theme-color so an installed phone app does not frame a
light page in a dark status bar. That last part is why this matters most on
the phone, where dark-only is a real problem outdoors rather than a taste."
```

---

## Task 12: The command bar

**Files:**
- Modify: `assets/app.js`, `assets/css/30-components.css`, `src/web/templates/layout.html`

**Interfaces:**
- Consumes: Task 5's key handler, Task 6's `.phonebar` geometry.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Add the overlay to the layout**

In `layout.html`, before `</body>`:

```html
{# One input that reaches everything. The tabs stay: the bar is the fast path,
   not the only path, and a phone needs doors it can see. #}
<div class="cmdk" hidden>
  <div class="cmdk-box">
    <input class="input" type="text" data-cmdk-input autocomplete="off"
           placeholder="Search, or > to ask…" aria-label="Command">
    <p class="cmdk-hint muted">
      <kbd>↵</kbd> go · <kbd>&gt;</kbd> ask · <kbd>esc</kbd> close
    </p>
  </div>
</div>
```

- [ ] **Step 2: Style it**

In `assets/css/30-components.css`:

```css
.cmdk { position: fixed; inset: 0; z-index: 50; display: grid;
  place-items: start center; padding-top: 12vh;
  background: rgba(0, 0, 0, 0.45); }
.cmdk-box { width: min(40rem, calc(100vw - 2rem));
  background: var(--color-bg-overlay); border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-lg); box-shadow: var(--shadow-lg); padding: 0.75rem; }
.cmdk-hint { margin: 0.5rem 0 0; font-size: var(--text-xs); }
```

- [ ] **Step 3: Wire it**

```js
  // Prefix decides the destination. Plain text searches, `>` asks, and a paste
  // long enough to be a document offers to keep it rather than to look for it.
  (function commandBar() {
    var overlay = document.querySelector('.cmdk');
    if (!overlay) return;
    var input = overlay.querySelector('[data-cmdk-input]');

    function open() { overlay.hidden = false; input.value = ''; input.focus(); }
    function close() { overlay.hidden = true; }

    document.addEventListener('keydown', function (e) {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') { e.preventDefault(); open(); return; }
      if (e.key === 'Escape' && !overlay.hidden) { e.preventDefault(); close(); }
    });
    overlay.addEventListener('click', function (e) { if (e.target === overlay) close(); });

    input.addEventListener('keydown', function (e) {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      var v = input.value.trim();
      if (!v) return;
      if (v.charAt(0) === '>') {
        location.href = '/ui/ask?q=' + encodeURIComponent(v.slice(1).trim());
      } else if (v.length > 400) {
        // Long enough to be a document rather than a question.
        try { sessionStorage.setItem('engram.paste', v); } catch (err) {}
        location.href = '/ui/capture';
      } else {
        location.href = '/ui/search?q=' + encodeURIComponent(v);
      }
    });
  })();
```

- [ ] **Step 4: Verify**

`⌘K`/`Ctrl-K` opens from every page. Enter on plain text lands on Search with the query run. `>` plus text lands on Ask. Escape closes without navigating. Clicking the backdrop closes.

- [ ] **Step 5: Commit**

```bash
git add assets/app.js assets/css/30-components.css src/web/templates/layout.html
git commit -m "feat(cmdk): one input that reaches everything

Ctrl-K from any page. Plain text searches, > asks, and a paste long enough
to be a document offers to keep it rather than to look for it.

The tabs stay. The bar is the fast path, not the only path — a shortcut is
not a substitute for a door you can see, and the phone needs the doors."
```

---

## Task 13: Results as you type

**Files:**
- Modify: `src/web/templates/search.html:16-22`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing.

- [ ] **Step 1: Measure before changing anything**

```bash
cargo run &
sleep 3
time curl -s -o /dev/null -b "$(cat .cookie 2>/dev/null)" \
  'http://localhost:8080/ui/search/results?q=malware+analysis'
```

Run it five times and note the median. The spec's gate is p50 comfortably under ~150ms against the real 1744-artifact corpus. **If it is not, stop and report — do not enable this by default.**

- [ ] **Step 2: Shorten the debounce**

The form already targets `#rail` over htmx. Change only the delay on line 18:

```
                  keyup changed delay:120ms from:input[name=q],
```

- [ ] **Step 3: Verify it feels immediate and does not thrash**

Type a sentence at normal speed and watch the network panel: requests coalesce rather than firing per keystroke, and htmx cancels in-flight requests so the rail never shows the results of an earlier prefix.

- [ ] **Step 4: Run the tests**

```bash
cargo test 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/templates/search.html
git commit -m "perf(search): results keep up with the typing

250ms was a pause you could feel between a phrase and its answer. 120ms is
under it. Affordable because of the constraint the roadmap sets: a search
costs one embedding and one vector query and never a generation, so the
query path is the same work either way — measured against the real corpus
before it moved."
```

---

## Task 14: Retrieval, made legible

**Files:**
- Modify: `src/web/templates/_results.html`, `assets/css/40-search.css`
- Test: `src/web/ui.rs`

**Interfaces:**
- Consumes: Task 7's `.label` and scale.
- Produces: `.rail-why`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_primed_hit_says_why_it_arrived() {
    // primed, loose and model-written already reached the rail as scattered
    // chips. The badge said what the result is; nothing said why it is here.
    let mut r = ranked(false);
    r.primed = true;
    let body = ResultsTemplate {
        results: vec![r],
        associated: vec![],
        all_weak: false,
        terms: String::new(),
    }
    .render()
    .unwrap();
    assert!(body.contains("rail-why"), "no provenance line: {body}");
    assert!(body.contains("you reach this one often"), "{body}");
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --lib a_primed_hit_says_why_it_arrived 2>&1 | tail -10
```

Expected: FAIL — no provenance line.

- [ ] **Step 3: Gather the badges into one sentence**

In `_results.html`, below the snippet, add:

```html
    {# The badges said what a result is. This says why it is here — one quiet
       line rather than a row of chips, because "moved up because you reach it
       often" is a sentence and `primed` is a word you have to already know. #}
    {% if r.primed || r.weak || r.model_written %}
    <p class="rail-why">
      {% if r.primed %}moved up — you reach this one often{% endif %}
      {% if r.weak %}{% if r.primed %} · {% endif %}a loose match{% endif %}
      {% if r.model_written %}{% if r.primed || r.weak %} · {% endif %}written by a model{% if r.origin_count > 0 %} from {{ r.origin_count }} source{% if r.origin_count > 1 %}s{% endif %}{% endif %}{% endif %}
    </p>
    {% endif %}
```

Keep the existing badges: they are the scannable form and this is the readable one. The `primed` badge's `title` text stays, which is what the test's second assertion checks.

- [ ] **Step 4: Style it**

```css
/* Quieter than the snippet it sits under: this explains the row, it is not
   the row. Hidden in reading mode with everything else but the rank. */
.rail-why { margin: 0.25rem 0 0; font-size: var(--text-xs);
  color: var(--color-fg-muted); }
.regions.reading .rail-why { display: none; }
```

- [ ] **Step 5: Run the tests**

```bash
cargo test 2>&1 | tail -15
```

Expected: PASS, including the pre-existing `a_primed_hit_gets_a_small_marker`.

- [ ] **Step 6: Commit**

```bash
git add src/web/templates/_results.html assets/css/40-search.css src/web/ui.rs
git commit -m "feat(rail): a result says why it arrived

primed, loose and model-written already reached the rail, as chips scattered
across the header. They gather into one quiet line underneath: 'moved up —
you reach this one often' is a sentence, and `primed` is a word you have to
already know.

No new data and no new query. The badges stay as the scannable form."
```

---

## Task 15: Recent tells its captures apart

The one Rust change in this plan.

**Files:**
- Modify: `src/web/ui.rs:65-95` (`QueueRow`), `src/web/ui.rs:1187-1240` (`queue_fragment`), `src/web/templates/_queue.html`
- Test: `src/web/ui.rs` tests module

**Interfaces:**
- Consumes: nothing.
- Produces: `QueueRow::opening: String` and `fn disambiguate_labels(rows: &mut [QueueRow])`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn colliding_capture_labels_get_told_apart() {
    // Synthesis lifts a title from a page header, and a header repeats across
    // every document that carries it: six rows read HOCHSCHULE MITTWEIDA and
    // named nothing. The opening words are the one thing that differs.
    let mut rows = vec![
        QueueRow { label: "HOCHSCHULE MITTWEIDA".into(), opening: "Kapitel 1 Einleitung".into(), ..Default::default() },
        QueueRow { label: "HOCHSCHULE MITTWEIDA".into(), opening: "Kapitel 5 Malware".into(), ..Default::default() },
        QueueRow { label: "Configure auditd".into(), opening: "auditctl -w /etc".into(), ..Default::default() },
    ];
    disambiguate_labels(&mut rows);
    assert_eq!(rows[0].label, "HOCHSCHULE MITTWEIDA · Kapitel 1 Einleitung");
    assert_eq!(rows[1].label, "HOCHSCHULE MITTWEIDA · Kapitel 5 Malware");
    // A label that was already unique is left alone: the suffix is a repair,
    // not a decoration.
    assert_eq!(rows[2].label, "Configure auditd");
}

#[test]
fn a_collision_with_no_opening_words_is_left_alone() {
    // A PDF whose extraction has not landed has no opening words, and
    // "document · document" tells no one anything.
    let mut rows = vec![
        QueueRow { label: "document".into(), opening: String::new(), ..Default::default() },
        QueueRow { label: "document".into(), opening: String::new(), ..Default::default() },
    ];
    disambiguate_labels(&mut rows);
    assert_eq!(rows[0].label, "document");
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test --lib colliding_capture_labels 2>&1 | tail -15
```

Expected: FAIL to compile — no `opening` field, no `Default`, no `disambiguate_labels`.

- [ ] **Step 3: Add the field and derive `Default`**

In `src/web/ui.rs`, on `QueueRow`:

```rust
#[derive(Default)]
pub struct QueueRow {
```

and add the field beside `label`:

```rust
    /// The capture's opening words, kept whether or not synthesis has named it.
    /// Not rendered on its own: it is what tells two rows apart when synthesis
    /// gave them the same name. Empty for a photo or an unread PDF.
    pub opening: String,
```

- [ ] **Step 4: Write the disambiguator**

Beside `queue_fragment`:

```rust
/// Recent lists ten captures, and synthesis names a capture by lifting a
/// heading out of it. A heading repeats across every document that carries it,
/// so six rows read `HOCHSCHULE MITTWEIDA` and named nothing — the column that
/// exists to tell captures apart could not.
///
/// Where a label is not unique in the list, the capture's opening words are
/// appended, which is the one thing that differs between them. A row whose
/// label was already unique is left alone: the suffix is a repair, not a
/// decoration. A row with no opening words to offer — a photo, a PDF whose
/// extraction has not landed — is also left alone, because `document ·
/// document` tells no one anything.
fn disambiguate_labels(rows: &mut [QueueRow]) {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in rows.iter() {
        *counts.entry(r.label.as_str()).or_insert(0) += 1;
    }
    let collides: std::collections::HashSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(l, _)| l.to_string())
        .collect();
    for r in rows.iter_mut() {
        if collides.contains(&r.label) && !r.opening.is_empty() && r.opening != r.label {
            r.label = format!("{} · {}", r.label, r.opening);
        }
    }
}
```

- [ ] **Step 5: Populate `opening` and call the disambiguator**

In `queue_fragment`, inside the `rows.push(QueueRow { … })` literal, add:

```rust
            opening: markdown::snippet(&s.raw_text, 60),
```

and after the loop, before `let active = …`:

```rust
    disambiguate_labels(&mut rows);
```

- [ ] **Step 6: Run the tests**

```bash
cargo test --lib colliding_capture_labels 2>&1 | tail -10
cargo test --lib a_collision_with_no_opening_words 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 7: Let coverage speak only when it is not whole**

In `_queue.html`, the settled branch renders `{{ r.artifact_count }} artifacts · {{ r.coverage }} covered` for every row. `100% covered` on all ten makes the column decoration. Replace that final `{% else %}` branch:

```html
      {# Coverage speaks when it is not whole and stays quiet when it is. Ten
         rows all reading "100% covered" is a column that says nothing, and it
         crowded out the one number on the row that differs. #}
      {% else %}
        <span>{{ r.artifact_count }} artifacts</span>
      {% endif %}
```

The two `low_coverage` branches above it are untouched — they are the case worth saying.

- [ ] **Step 8: Run the full suite**

```bash
cargo test 2>&1 | tail -20
```

Expected: PASS. If a queue template test asserted on `100% covered`, update it to assert the low-coverage branch still speaks.

- [ ] **Step 9: Commit**

```bash
git add src/web/ui.rs src/web/templates/_queue.html
git commit -m "fix(capture): Recent tells its captures apart

Synthesis names a capture by lifting a heading out of it, and a heading
repeats across every document that carries it — six rows read HOCHSCHULE
MITTWEIDA, and the column that exists to tell captures apart could not.
Where a label is not unique, the opening words are appended; where it is
already unique, or where there are no opening words to offer, nothing
changes.

Coverage stops announcing itself when it is whole. Ten rows reading
100% covered is a column that says nothing and crowds out the number on
the row that differs."
```

---

## Task 16: The source pane

Covers the spec's three source-pane commitments: the extraction range marked with a left accent bar, `source` scrolling in lockstep with `focus`, and runs of blank lines collapsing.

**Files:**
- Modify: `assets/app.js`, `assets/css/40-search.css`

**Interfaces:**
- Consumes: Task 2's regions.
- Produces: nothing.

**The real markup.** Source lines are a table, in both `_artifact_detail.html:249-255` and `_bands.html`:

```html
<div class="raw">
  <table>
    <tr class="{% if l.in_span %}in{% endif %}">
      <td class="ln">{{ l.number }}</td><td>{{ l.text }}</td>
    </tr>
  </table>
</div>
```

`tr.in` is the extraction range and already takes `--color-accent-dim` (`app.css:338`). `td.ln` is the sticky line-number gutter. Use these; do not invent classes and do not change the server's markup.

- [ ] **Step 1: Mark the extraction range with a bar, not only a tint**

In `assets/css/40-search.css`:

```css
/* The tint says these lines are the ones. A bar down their edge says where
   the run starts and stops, which a background shared with the row above it
   does not — and it survives at a glance from across the pane. */
.raw tr.in td.ln { box-shadow: inset 2px 0 0 var(--color-accent); }
```

- [ ] **Step 2: Scroll the source with the artifact**

In `assets/app.js`, add to `enhance(root)`:

```js
  // The artifact and the lines it came from are one thing read twice, so they
  // move together: scrolling either moves the other to the same relative
  // place. Proportional rather than line-mapped, because prose and source do
  // not share a line count and pretending they do lands on the wrong line
  // more often than it helps.
  //
  // The guard is not decoration: setting scrollTop fires scroll, which would
  // set the other back, which would fire again.
  function lockstep(root) {
    var a = root.querySelector('.pane-artifact');
    var b = root.querySelector('.raw');
    if (!a || !b || a.dataset.lockstep) return;
    a.dataset.lockstep = '1';
    var busy = false;
    function sync(from, to) {
      return function () {
        if (busy) return;
        busy = true;
        var span = from.scrollHeight - from.clientHeight;
        var ratio = span > 0 ? from.scrollTop / span : 0;
        to.scrollTop = ratio * (to.scrollHeight - to.clientHeight);
        // Released on the next frame, after the scroll event it caused.
        requestAnimationFrame(function () { busy = false; });
      };
    }
    a.addEventListener('scroll', sync(a, b));
    b.addEventListener('scroll', sync(b, a));
  }
```

If `.pane-artifact` is not the artifact column's scroll container, use whichever element in `_artifact_detail.html` is — check with `grep -n 'class="pane' src/web/templates/_artifact_detail.html` and give it a scroll container in CSS if it has none.

- [ ] **Step 3: Collapse runs of blank lines**

Rendering only: the server keeps emitting every line and every number, and the numbers either side of a fold stay the source's own.

```js
  // A blank source line costs a full numbered row; in a chapter of exercises
  // that was a third of the pane. Runs of three or more fold to one rule
  // carrying their count.
  //
  // A row in the extraction range is never folded, whatever it contains: the
  // pane exists to show that range, and hiding part of it to save space
  // defeats the only thing the pane is for.
  function collapseBlanks(root) {
    root.querySelectorAll('.raw table:not([data-folded])').forEach(function (table) {
      table.setAttribute('data-folded', '1');
      var rows = Array.prototype.slice.call(table.rows);
      var run = [];
      function flush() {
        if (run.length < 3) { run = []; return; }
        var hidden = run.slice();
        var mark = document.createElement('tr');
        mark.className = 'srcfold';
        var cell = document.createElement('td');
        cell.colSpan = 2;
        cell.textContent = hidden.length + ' blank lines';
        mark.appendChild(cell);
        hidden[0].parentNode.insertBefore(mark, hidden[0]);
        hidden.forEach(function (el) { el.hidden = true; });
        mark.addEventListener('click', function () {
          hidden.forEach(function (el) { el.hidden = false; });
          mark.remove();
        });
        run = [];
      }
      rows.forEach(function (tr) {
        var blank = tr.cells.length > 1 && tr.cells[1].textContent.trim() === '';
        if (blank && !tr.classList.contains('in')) { run.push(tr); } else { flush(); }
      });
      flush();
    });
  }
```

- [ ] **Step 4: Call both from `enhance()`**

Add `lockstep(root);` and `collapseBlanks(root);` beside the existing `highlight`/`clamp`/`copyButtons` calls inside `enhance(root)`, so they run on htmx swaps as well as on load.

- [ ] **Step 5: Style the fold**

```css
/* A rule with a count, not a gap. Reads as "something was left out here",
   which empty space does not. Indented past the gutter so the line-number
   column stays a column. */
.raw tr.srcfold td {
  cursor: pointer; padding: 0.125rem 0.5rem 0.125rem 3.5rem;
  border-top: 1px dashed var(--color-border-subtle);
  color: var(--color-fg-muted); font-size: var(--text-xs);
}
.raw tr.srcfold:hover td { color: var(--color-fg-secondary); background: var(--color-bg-hover); }
```

- [ ] **Step 6: Verify against a real document**

```bash
cargo run &
sleep 3
```

Open the chapter-of-exercises artifact from the audit. Confirm: runs of three or more blank lines fold; the numbers either side of a fold are unchanged and correct; clicking a fold expands it; **no folded row is inside the highlighted range**; the range carries a bar down its left edge; and scrolling the artifact moves the source to the same relative place without either one juddering.

- [ ] **Step 7: Run the tests**

```bash
cargo test 2>&1 | tail -10
```

Expected: PASS — this task touches no Rust.

- [ ] **Step 8: Commit**

```bash
git add assets/app.js assets/css/40-search.css
git commit -m "fix(source): the pane reads as one document, not two

The extraction range gets a bar down its edge, so where it starts and stops
survives a glance. The artifact and its lines scroll together, because they
are one thing read twice. And a run of blank lines folds to a rule carrying
its count — every blank cost a numbered row, which in a chapter of exercises
was a third of the pane.

Rendering only. The server still sends every line, the numbers either side
of a fold are the source's own, and a row inside the extraction range is
never folded whatever it holds: hiding part of that range to save space
defeats the only thing the pane is for."
```

---

## Final verification

- [ ] **Step 1: Full suite**

```bash
cd /home/user01/Projekte/engram
cargo test 2>&1 | tail -25
```

Expected: all green.

- [ ] **Step 2: Clippy and formatting**

```bash
cargo clippy --all-targets 2>&1 | tail -20
cargo fmt --check
```

- [ ] **Step 3: Walk every page at every tier, in both themes**

```bash
cargo run &
sleep 3
```

For each of `/ui/search`, `/ui/capture`, `/ui/ask`, `/ui/judge`, `/ui/ops`, `/ui/settings`, and an artifact detail page: check three-up (≥90rem), two-up (60–90rem) and one-up (<60rem), in dark and in light. Confirm on every one that the brand and the content column share a left edge, nothing scrolls horizontally, and no text sits below the contrast floor.

- [ ] **Step 4: Phone pass**

At 390×844 with touch emulation: the search bar is in the thumb zone above the tab bar, both clear the home indicator, inputs do not trigger zoom on focus, every tap target is at least 44px, and the region-to-region navigation animates and can be reversed.

- [ ] **Step 5: Reduced motion and no-JavaScript**

Enable `prefers-reduced-motion: reduce` and confirm all animation stops with the app fully usable. Then disable JavaScript and confirm Search, Capture and Housekeeping still render and navigate — htmx and the enhancements are enhancements, and the `.back` link is the fallback the region model leans on.
