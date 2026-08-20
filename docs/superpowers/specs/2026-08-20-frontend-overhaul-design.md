# Frontend overhaul

The frontend works. Every page does its job, the mobile care is real — safe
area insets, 44px targets, 16px inputs so iOS does not zoom a standalone window
into a corner it cannot get out of — and the split pane that shows an artifact
beside the corpus lines it came from is the best argument the project has for
its own thesis.

It is also three different applications stacked in one binary. Capture centres
itself in 60rem, Search centres a 48rem filter block inside a 110rem shell,
Housekeeping runs full bleed. The brand stays put while the content column
moves underneath it, so navigating jolts. Nothing on the search page lines up
with anything else on the search page.

This is the design for making it one application, and for making that one
application pleasant enough that the tool gets reached for.

## What this is not

**Not a rewrite, and not a new stack.** Askama, htmx, one stylesheet and one
script, embedded by `rust-embed`, shipped by `cargo build`. No bundler, no
framework, no `node_modules`. The palette in `assets/app.css` was deliberately
ported *out* of Tailwind into plain custom properties; reintroducing a build
toolchain would undo that decision to buy nothing this design needs. Modern CSS
— grid, `:has()`, container queries, view transitions — carries essentially all
of it.

**Not a backend project.** Almost every change here is a template, a
stylesheet or a script. Exactly one item touches Rust, and it is about twenty
lines. See *What touches Rust*.

**Not a dashboard.** No graph visualisation of the memory, no activity charts,
no streaks, no gamification. The roadmap's position — *"the search box stays
the application; nothing here is a screen to look at"* — is correct, and the
things that would photograph best are the things that would earn least. A
density toggle is likewise out: `assets/app.css` records that it was cut on
purpose.

**Not a redesign of Ask or Judge.** They inherit the new spine and nothing
more. Judge in particular already earns its keep: digits pick an option, `N`/`S`
/`X` are the ways out, `U` undoes, and the whole thing is built so a verdict
costs about five seconds.

## The spine: regions, not breakpoints

One grid across the whole application, with four named regions:

| region | holds |
|---|---|
| `bar` | the query / command input |
| `rail` | the ranked list |
| `focus` | the artifact, the capture form, the judge card, the table |
| `source` | corpus lines, provenance, neighbours |

A page declares **which regions it uses**. It never declares a width. Search
uses all four; Capture uses `bar` and `focus`; Housekeeping uses `focus` alone.
Because `rail` and `focus` always begin in the same grid columns, the single
left anchor is a consequence of the model rather than a fix applied on top of
it. The three `shell_class` variants go away.

The viewport decides only how many regions are visible at once:

- **three-up** (≥ 90rem) — `rail | focus | source`
- **two-up** (60–90rem) — `rail | focus`, `source` a toggle inside `focus`
- **one-up** (< 60rem) — one region, the others reached by navigation

One-up is not a phone special case. It is the rule already in the stylesheet:

```css
.workspace.has-selection .rail { display: none; }
```

with its back link deliberately appearing at 60rem rather than at the 40rem
phone breakpoint, because a half-width desktop window has the same problem a
phone does. That instinct is right and it generalises. Expressing the tiers as
**container queries against the shell** rather than media queries against the
device makes it exact: the workspace responds to the space it is actually in.

The phone is therefore the same application with the region count set to one,
not a degraded copy of the desktop.

## Search

The dominant loop is *query → scan the rail → open an artifact → verify it
against its source lines*. Everything below serves that.

### The rail is a map

It never loses information.

The open card keeps a single dimmed line. Today `assets/app.css:647` hides
`.rail-snippet` on the selected item, reasoning that the pane beside it shows
the text in full — true, but the rail is also the ranking, and a card
collapsing to a bare stub punches a hole in the list and loses your place. The
accent border and background already say "this one is open".

`.rail-past` moves from `opacity: 0.55` to `0.7`. Demoted past-cliff results
should read as demoted, not as unreadable; at 0.55 over `#0e1015` they are very
likely under WCAG AA.

The cliff divider is untouched. *"Relevance falls off here"* says the one thing
a similarity score cannot.

Rail items still render **no heading where the artifact has none**. A verbatim
passage has no title by design and `_results.html` is explicit about it. The
duplicate-title problem is real but it lives on Capture, not here.

### Focus and source

`focus` gets a real header: title, kind, and one button vocabulary. Where the
artifact has no title the header carries its source and line range instead, for
the same reason the rail carries none — a passage has no name, and inventing
one says less than the lines it came from. Today a
single screen carries three — unlabelled icon buttons (`✓`, eye-off) stranded
at the top of a wide row, text links (`edit`, `copy`) inside the card, and
solid buttons elsewhere. They collapse into one set with icon *and* label.
Delete keeps its exile to the far end — `assets/app.css:635` explains why, and
the reasoning holds — but anchors to the header instead of floating at the
window edge.

`source` scrolls in lockstep with `focus`, and the extraction range carries a
left accent bar. Runs of blank lines collapse to a thin rule showing how many
were swallowed; in the audited screenshot they cost roughly a third of the
pane. This is done client-side over the rendered markup — the server keeps
emitting every line, and the line numbers stay true.

### Reading mode

On three-up, opening an artifact may narrow the rail to a numbered spine of
about 3rem, giving the width to `focus` and `source`. Toggled with `r` and by
clicking the spine. The spine still marks the cliff, so position in the ranking
survives.

### Keyboard

`/` focuses the query from anywhere. `↑`/`↓` and `j`/`k` walk the rail — the
arrow half already exists in `app.js`. `Enter` opens, `Esc` steps back one
region, `s` toggles source. A hint row teaches these once and can be dismissed
for good.

### On the phone

One-up: the rail, then the artifact, then source as a toggle in the artifact
header rather than a second column squeezed into 390px.

**The query bar sits at the bottom, in the thumb zone**, with the tab bar as a
slimmer strip beneath it. On Search the input genuinely is the application, and
it should be where the thumb already is. It also gives Search a silhouette no
other tab has, which is orientation for free.

Region changes animate as slides through the View Transitions API, so back
feels native. The existing `.back` link stays as the fallback.

## Craft

### Typography

The sizes today are chosen per component — `0.6875`, `0.75`, `0.8125`,
`0.875`, `1`, `1.375rem`. They become a named scale in the token layer and
components pick from it.

The more consequential change: `assets/app.css:100` restyles `h3` globally into
a small uppercase muted label. That is why the pages have no headings — the
element that would carry hierarchy has been spent on a label style. That style
moves to a `.label` class and `h1`–`h3` get their job back. This is most of
what makes Housekeeping and Capture read as documents rather than as flat runs
of rows.

Prose takes a measure of about 68ch **even inside a wide region**. Housekeeping's
introduction currently runs near 200 characters per line. The table beside it
keeps the full width; only the prose is constrained.

### Motion

One rule: motion explains what moved, and never decorates.

Region changes and page navigation use view transitions driven off htmx swaps.
The artifact cross-fades when a different result is picked. A fresh result set
fades in on a stagger so it reads as arriving rather than blinking. The cliff
divider does not move. All of it inside
`@media (prefers-reduced-motion: no-preference)`.

### Light and dark

There is a complete, considered light palette in `assets/app.css` that nobody
has ever seen, because it activates only on `prefers-color-scheme`.

An explicit toggle: the page follows the system preference until the toggle is
touched, and from then on it is a remembered two-state dark/light switch
written to `data-theme` on the root. Applied by a small inline script before
first paint, so there is no flash of the wrong theme, and mirrored into the
`theme-color` meta so an installed phone app does not frame a light page in a
dark status bar.

This matters most on the phone, where dark-only is a real usability gap
outdoors rather than a matter of taste.

## Aliveness

### The command bar

One input, reachable with `/` or `⌘K` from any page, and on the phone it is the
bottom bar. Plain text searches; a `>` prefix asks; a paste over roughly 400 characters
offers to capture it instead of searching for it. Judge appears there when the queue is not empty.

The tabs stay. The bar is the fast path, not the only path — discoverability
matters and the phone needs the tabs regardless.

### Results as you type

The search form already targets `#rail` over htmx (`search.html:16`), so this
is an added `hx-trigger` on the query field and nothing else.

It is affordable because of the write-time constraint the roadmap sets: a
search costs one embedding and one vector query, never a generation. It still
gets measured against the real corpus before it becomes the default, and stays
behind a setting if p50 is not comfortably under about 150ms.

### Retrieval made legible

`primed`, `loose` and `model-written · n` already reach the rail as badges
(`_results.html:38-56`). The work is presentational: making them read as one
quiet explanation of why this result arrived, rather than as scattered chips.

### Capture is already alive

`_queue.html` polls itself every three seconds, stops when nothing is in
flight so an idle background tab makes no requests, listens for a `captured`
event so a paste onto an idle page still updates, and already shows
`segmenting n/m`. There is nothing to build here. The audit screenshot showed a
settled queue.

What Capture does need is **disambiguation**. Six rows reading
`HOCHSCHULE MITTWEIDA` cannot be told apart, and some labels begin mid-sentence.
And `100% covered` on every row makes the column decoration: coverage should
speak when it is not whole and stay quiet when it is.

## What touches Rust

Everything above is templates, stylesheet and script, except:

- **`queue_fragment`** (`src/web/ui.rs:1187`) — `r.label` comes from the
  capture's opening line. When labels collide it needs a distinguishing suffix.
  One function, one test.
- **`build.rs`** — a concatenation step, build-time only, described below.

That is the whole backend surface.

## Stylesheet layout

`assets/app.css` is 892 lines and this design grows it. It splits into layers:

```
assets/css/00-tokens.css
           10-base.css
           20-layout.css
           30-components.css
           40-search.css, 41-capture.css, 42-judge.css, 43-ops.css
```

`build.rs` concatenates them in filename order into `assets/app.css`, which
stays the single generated, gitignored, embedded, hash-stamped asset. The
machinery already works this way: `build.rs` writes the extension packages into
`assets/` for `rust-embed` to pick up, and `stamp_assets()` derives the cache
stamp from the bytes of `app.css` itself. The only requirements are that
concatenation runs **before** `stamp_assets()`, and that `assets/css` is
declared in `rerun-if-changed`.

One request, one stamp, no toolchain.

## Testing

The existing template assertions are the regression net for the markup changes
— `a_primed_hit_gets_a_small_marker` (`src/web/ui.rs:4626`) and its neighbours
must all keep passing, and where markup moves they get updated rather than
deleted. New Rust tests cover only the Capture disambiguator.

The stylesheet and script work is verified by `cargo test` staying green plus a
walk of the three region tiers and both themes, on desktop and on a phone.

## Sequencing

Each phase ships on its own.

1. **Flow** — the region grid and the single left anchor, the rail fixes, the
   unified button set, the keyboard map, the phone bottom bar. Prerequisite for
   everything else.
2. **Craft** — the type scale and `h3` → `.label`, the measure, motion and view
   transitions, reading mode, the dark/light toggle.
3. **Aliveness** — the command bar, results as you type, the provenance line,
   the Capture disambiguator.

If the work runs out of day, it runs out at a phase boundary with a working
application.
