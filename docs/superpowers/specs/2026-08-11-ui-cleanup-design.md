# UI cleanup: fewer destinations, a real phone layout

Date: 2026-08-11
Status: approved

## Why

Two problems, found by reviewing the four screens and reading `assets/app.css`
and `src/web/templates/`.

The desktop UI repeats itself. The chunk pane restates the result card beside
it; "lines 8–8" is said three times on one screen; Ops answers five headings
with "None."; a whole Browse column says `ready` in every row.

The phone UI was never designed. `app.css` has exactly one breakpoint —
`60rem` — so a phone gets the desktop layout at a smaller size: five-column
tables, a nav row that cannot wrap, 14px inputs that make iOS zoom and never
zoom back in a standalone window, 32px touch targets, scrollers nested inside
the page scroller, and no way back from an opened result.

Separately, a real layout bug: `.pane` sets `min-width: 0` but its grid
siblings do not. A grid child defaults to `min-width: auto` — min-content — so
one unwrappable line of shell source widens the column instead of letting
`.raw` scroll inside itself, and the document gains a horizontal scrollbar at
every width.

## Decisions

**Browse is deleted.** Recent captures live under Capture; anything older is
found by searching for what it says. The corpus page keeps its route — artifacts
link to the lines they came from — it is simply no longer indexed.

*Accepted consequence:* a capture that yielded no artifacts becomes unreachable
once it falls off the recent list, because it has nothing for search to match.
Rare, and preferred over keeping an index page.

**The "Optional label" field is deleted.** Titles come from the model. Until
synthesis finishes, a capture reads *Untitled capture*.

**Ops folds into Capture.** Pairs awaiting a decision appear as a *Needs you*
section where the work arrives. What remains — hidden artifacts, deprecated
ones, retries, counts — moves to a Housekeeping page reached by one quiet link
at the bottom of Capture. Same route, no nav slot.

**The queue polls.** `hx-trigger="every 3s"`, and the returned fragment omits
its own trigger when nothing is active, so an idle page makes no requests.
Chosen over SSE for the installed-app case: when iOS suspends a backgrounded
PWA an SSE connection dies and must reconnect, whereas the next poll after
resume simply fetches current truth. The underlying events are one model call
apart, so sub-second push buys nothing.

**Phones get a bottom tab bar** — Capture, Search, Ask — with the top row
reduced to identity and sign-out.

## Screens

### Capture (the home screen)

One line of guidance, the textarea, the button. Then, when non-empty, *Needs
you*; then *Recent*; then the Housekeeping link.

A queue row is a row, not a table cell: title on the left, state on the right.
Only work in flight announces itself — a pulsing dot and a `segmenting 3/7`
badge. A finished capture is its title and `23 artifacts · 84%`. Coverage below
the low-coverage threshold keeps its warning badge; otherwise it is plain text.

### Search

The rail keeps rank, title and a two-line snippet, and loses its tag chips —
the pane lists tags anyway, and the chips were what made the rail heavy enough
to need its own scrollbar. The selected card also drops its snippet, because
the pane beside it is showing that text in full.

"lines 8–8" is stated once, in the label above the pane that shows them, as the
link out to the source. The breadcrumb and the "open source at these lines"
link are removed.

Delete is separated from verify and hide with `margin-left: auto`, so the
destructive control is not flush against two reversible ones.

### Housekeeping

Only non-empty sections render. What is empty collapses into one closing
sentence ("Nothing deprecated, nothing waiting on a decision, nothing
retrying."). Counts read as prose rather than as `done: 17` badges. A
breadcrumb leads back to Capture.

### Phone

Bottom tab bar, 52px targets, clearing the home indicator with
`env(safe-area-inset-bottom)`. Inputs at 16px and 44px tall. Chips and icon
buttons at 44px. `.rail` and `.raw` drop their max-heights so the page is the
only scroller. `.workspace` and `.split` collapse to one column; opening a
result hides the rail and the pane gains a `← Results` link, styled as chrome
rather than as a bare browser link. `table.grid` renders as cards.

## Work

### Backend

1. **`CaptureForm` loses `title`** (`src/web/ui.rs:398-407`); `ingest` is called
   with `None`.
2. **A corpus title is written at the end of synthesis** — in
   `src/jobs/synthesize.rs::run`, once every segment is resolved. A new
   `Synthesizer::title(&self, text, artifact_titles)` returns a short name from
   one cheap call; `src/infer/fake.rs` gets a deterministic implementation for
   tests. Failure logs and leaves `title_hint` NULL: a missing title must never
   fail a capture. No new job stage.
3. **No backfill.** Existing rows keep the `markdown::snippet` fallback, which
   stays as the display default for a NULL `title_hint`. A `Re-segment` writes a
   title as a side effect.
4. **`GET /ui/queue`** returns the recent-captures fragment: the 10 newest
   corpora with status, segment progress, artifact count and coverage. It
   includes `hx-trigger="every 3s"` on itself only while at least one corpus is
   not terminal.
5. **`/ui/browse` is removed**, and redirects to `/ui/capture` so an installed
   PWA's start URL or a bookmark does not 404. `POST` handlers that redirect to
   `/ui/browse` (`src/web/ui.rs:702`) redirect to `/ui/capture`.
6. **`/ui/ops` keeps its route**, loses its pairs section (moved to Capture) and
   gains the collapsed-empties rendering.

### Templates

`browse.html` is deleted. `capture.html` gains `_queue.html` and `_decide.html`
partials. `search.html` and `_results.html` lose the tag chips and the
breadcrumb. `_artifact_detail.html` restates its pane label and reorders its
actions. `ops.html` renders only non-empty sections. `layout.html` drops Browse
and Ops from the nav, adds the tab bar, and adds
`apple-mobile-web-app-status-bar-style`.

### CSS (`assets/app.css`)

- `.workspace > *, .split > *, .rail-head, .qrow { min-width: 0 }` and
  `.raw { max-width: 100% }` — the overflow fix. `body { overflow-x: hidden }`
  as a backstop.
- `.badge { white-space: nowrap }`.
- `.rail-item[aria-selected="true"] .rail-snippet { display: none }`.
- New: `.queue`, `.qrow`, `.qtitle`, `.qmeta`, `.qdot`, `.decide`,
  `.quiet-link`, `.actions`, `.back`, `.tabbar`.
- A new `@media (max-width: 40rem)` block carrying everything under "Phone"
  above.
- `.shell` gains `padding-left/right: max(1rem, env(safe-area-inset-*))`.

### Assets

`manifest.webmanifest`: `background_color` and `theme_color` become `#f8f6f1`
to match the light default, instead of the dark `#0e1015` that flashes a dark
splash into a cream page. `start_url` moves from `/ui/search` to `/ui/capture`,
which is now the home screen; `id` stays `/ui/search`, because changing it
makes a browser treat the reinstall as a different app. `app.js`: the search box's `autofocus` attribute is
replaced by a focus call guarded on `matchMedia('(hover: hover)')`, so the
keyboard no longer covers the results on touch.

## Testing

- Existing route tests updated: `browse_lists_captured_sources` becomes a test
  that `/ui/browse` redirects, plus a test that the queue fragment lists recent
  corpora and that its poll trigger is absent when everything is terminal.
- A capture posted without a title still ingests, and its corpus reads as
  untitled until synthesis names it.
- The fake synthesizer names a corpus, and a failing `title` call leaves the
  capture successful with a NULL `title_hint`.

Layout is verified in headless Chromium at 1440/1024/820/640/390: the document's
`scrollWidth` must equal its `clientWidth`, and every `.raw` must sit inside the
viewport while scrolling internally.

## Not doing

Dark-mode review, a density toggle, offline caching, pagination of the recent
list, or backfilling titles for existing corpora.
