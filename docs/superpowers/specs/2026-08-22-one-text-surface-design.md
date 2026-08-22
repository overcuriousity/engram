# One text surface for the whole web UI — Design

Date: 2026-08-22
Status: draft
Adds `src/web/workspace.rs`, `src/web/insights.rs`,
`templates/workspace.html`, `templates/insights.html`,
`assets/css/40-workspace.css`; retires `templates/search.html`,
`templates/ask.html`, `templates/capture.html`, `templates/_sitting.html`,
`assets/css/40-search.css`, `assets/css/41-capture.css`,
`assets/css/45-ask.css`; touches `src/web/ui.rs`, `src/web/mod.rs`,
`templates/layout.html`, `assets/app.js`, `assets/css/50-phone.css`,
`ROADMAP.md`.
No new endpoint that returns a fragment, no new store table, no model call that
was not already there.
See §11 for what it is not allowed to break.

## 1. Why

Capture, search and ask are three pages, and moving between them means retyping
or carrying a prefill: the same words are a query on one, a question on the
second and a document on the third, and the operator navigates to say which.
`ROADMAP.md:476` names this and names the price of not fixing it — the box
cannot be the application while the page you are on is still a thing to decide.

The extension's side panel already shows the answer. One box that never changes
shape, with the verb chosen by a button: typing searches, **Ask** spends the
model call, **Capture** stores what is in the box, and no state hidden between
them. It shipped there first because 350 pixels of one column is the cheapest
place to find out whether one surface really holds three verbs. It does.

This folds the three pages into one route, moves everything that is management
or measurement off it, and takes the visual pass that the three-page split had
been deferring.

`ROADMAP.md:490` leaves one question open — where the rail, the filter chips and
the judged-verdict bar live when the box is doing all three jobs. §5 answers it.

## 2. What is built

1. **One workspace** at `/ui`, holding the box, the results, the artifact you
   are reading, its source lines, the answer, and the context recommendation.
2. **One act in flight.** The rail and the focus column each show what the
   current act produced and nothing else; pressing **Ask** disables the surface
   until the answer lands.
3. **The doors that were pages become deep links.** `/ui/search`, `/ui/ask` and
   `/ui/capture` keep working and render the workspace in a given state. There
   is no "capture mode": `/ui/capture` focuses the box and carries its prefill,
   and the file control is in the verb row on every state of the page.
4. **An Insights page** absorbing Housekeeping, the three fragments that leave
   the workspace, and — as a separable second half — the measures over the base.
5. **The V1 visual pass**: today's palette, executed against one type scale,
   with the retrieval given a legible shape.

## 3. The route, and the doors that survive

`/ui` becomes the workspace and is canonical. Nothing reachable today stops
being reachable.

| URL | Today | After |
|---|---|---|
| `/` , `/ui` | redirect to `/ui/search` | the workspace |
| `/ui/search?q=…&category=…` | search page | workspace, box prefilled, results shown |
| `/ui/ask?q=…` | ask page | workspace, box prefilled, answer requested |
| `/ui/capture` | capture page | workspace, box focused and carrying any prefill |
| `/ui/artifacts/{id}` | full page | unchanged — it is the reading column, and `hx-push-url` already puts it there |
| `/ui/ops` | Housekeeping | redirects to `/ui/insights` |

`/ui/capture` keeps honouring `prefill_text`, `prefill_ask` and
`prefill_question`: the extension and the *keep this answer* flow post into it,
and neither knows anything about this change.

**Every fragment endpoint is untouched.** `/ui/search/results`,
`/ui/ask/{id}/stream`, `/ui/ask/{id}/verdict`, `/ui/ask/{id}/carried`,
`/ui/ask/{id}/keep`, `/ui/context`, `/ui/context/seen`, `/ui/queue`,
`/ui/artifacts/{id}` and the rest answer exactly as they do now. This is what
makes the change affordable: an htmx fragment does not care which page it lands
in, so the retrieval, streaming and capture paths are re-arranged rather than
rewritten.

The `.cmdk` overlay in `layout.html` and `commandBar()` in `app.js` are deleted.
They were a second, hidden text surface for the problem the first one now
solves; `/` focuses the real box instead.

## 4. The box

One `<textarea>`, one line tall, growing to a ten-line cap and then scrolling
inside itself. It is a textarea from the first keystroke to the last. It never
switches element type, and it never infers a verb from a length or a newline —
what decides which of the three happens is a button.

- **Typing** runs the search. The existing trigger carries over verbatim:
  `keyup changed delay:120ms`, `hx-sync="this:replace"`, `hx-target="#results"`.
  The reasons for both are recorded at the top of the old `search.html` and move
  with the markup.
- **Ask** spends the model call. The existing POST-then-`EventSource` driver in
  `app.js` is unchanged; the answer streams into the focus column.
- **Capture** stores what is in the box, posting to `/ui/capture` as today. The
  receipt lands in the focus column.
- **Ask** and **Capture** are disabled while the box is empty. **Ask** is absent
  entirely where `ask_enabled` is false — the door is simply not there, as in
  today's nav.
- **Files.** A single control in the verb row opens the picker, which on a phone
  is the camera. Dropping a file anywhere on the workspace does the same. The
  staged-file box and its note field render only once something is staged;
  today they are always-on furniture that costs `capture.html` most of its
  height. A staged file still waits for **Capture** to be pressed.
- **Kind chips** sit at the right of the verb row, reading `Kind: all` until
  used. On a phone they are hidden inside the fixed bar, exactly as
  `50-phone.css` already hides `.facets` there — the bar has to stay one line.

### One act in flight

From the press of **Ask** until the answer lands, the textarea and both verb
buttons are `disabled` and only **Stop** is live.

Disabling the textarea *is* disabling search-while-type: a disabled input fires
no `keyup`, so the `hx-trigger` goes quiet with no second mechanism and no flag
to keep in sync.

Three ways out, and all three must re-enable:

1. the stream completing,
2. **Stop** being pressed — which closes the stream and keeps the tokens that
   arrived, as it does today,
3. **the stream failing** — `EventSource.onerror`. This is the one that locks
   the page forever if it is missed, and it is the only new failure path this
   design introduces.

A disabled textarea keeps readable contrast rather than the browser's default
grey: the question you asked stays worth reading while you wait for it.

## 5. The three regions

The grid is unchanged. The workspace declares `regions-rail-focus-source` and
inherits everything `20-layout.css` already does — the single left anchor, the
container queries, `has-selection` hiding the rail on a narrow window, and the
back link that appears at exactly that width.

The rail and the focus column each show **what the current act produced**:

| Act | Rail | Focus |
|---|---|---|
| typing | results, with the cliff rule and the all-weak banner | the artifact you open |
| Ask | the cited excerpts, headed `Written from · n` | the answer, verdict bar under it |
| Capture | unchanged from before the press | the receipt card |
| idle | empty | the sentence saying what the page is for |

Search results do **not** survive an Ask. They were produced by a different act
and have nothing to do with the excerpts the answer was written from. Since the
query is still in the box unedited and so nothing re-triggers the search, the
`Written from` label carries one `← results` anchor that re-fires
`/ui/search/results` with the box's current value. One anchor, no stored state.

Arrow keys walk `.rail-item` in both states, as they already do on both pages.

**The idle state** is the box, the context recommendation under it, an empty
rail, and one sentence in the focus column. Recent captures, the *Needs you*
pairs and the knowledge gaps are not there; §6 says where they went.

**The source region** is unchanged, and `s` still toggles it. `r` still toggles
reading mode, which is defined for pages with a rail region and this is one.

This answers `ROADMAP.md:490`. The rail belongs to the act. The filter chips
belong to the box, because they qualify what typing does and nothing else. The
judged-verdict bar rides under the answer into the focus column, exactly as
`_ask_verdict.html` already places it — no new decision was needed.

## 6. Navigation, and what leaves the workspace

The top row keeps: brand, spacer, **Judge** with its badge, **Insights**, the
theme button, sign out. Capture, Search and Ask leave it — there is one page,
and it is the one you are on.

The phone tab bar keeps three entries: **Search**, **Judge** (with its dot),
**Insights**. It is retained deliberately rather than folded into a menu: the
two destinations that are not the workspace stay visible.

Leaving the workspace for Insights:

- **Recent captures** — `_queue.html`, and the `/ui/queue` endpoint that feeds
  it, both unchanged.
- **Needs you** — `_decide.html` and the near-duplicate pair actions.
- **Knowledge gaps** — `_gaps.html`.

Retired outright:

- **The "Read just now" list** — `_sitting.html`, `SittingItem` and
  `sitting_rail()` at `src/web/ui.rs:550`. Its own comment states its reason:
  *"the pages had nothing between them, so a hit opened on search and wanted
  again on ask meant searching for it twice."* This design removes the pages, so
  the list is patching a problem that no longer exists.
- **Ask's last-query prefill** at `src/web/ui.rs:3153`, which carried the box's
  contents across the same gap. One box; the query is already in it.

`core::sitting` itself stays, and so does every write into it. It is not UI
furniture:

- it feeds Ask's **carried citations** — an answer may cite an artifact you were
  just reading rather than one retrieval returned (`citations.carried`,
  `_ask_carried.html`, `/ui/ask/{id}/carried`),
- `[sitting] prime` at `src/core/search.rs:995` reads it, off by default and
  still reachable,
- `sittings.touched()` at `src/web/ui.rs:3799` keeps feeding both.

## 7. Insights

Two halves. They land in this order, and the second is separable — it can be cut
without touching anything in §3–§6.

**7a. Maintenance.** Relocation, not new work: the three fragments above, plus
what Housekeeping already holds — hidden, stale and retrying artifacts, the
merge undo log, tokens, sources. `/ui/ops` redirects here and every
`/ui/ops/...` action endpoint keeps its path and its behaviour. Settings stays
its own page at `/ui/settings`, linked from here.

**7b. The measures.** New code, and the one part of this design that is a
project in its own right. `ROADMAP.md` lists it as a branch of its own — *"what
this memory is like"* — and it is built here because it is what the third tab
promises:

- **Held** — artifacts, corpora, passages; how much and how densely.
- **Fading** — the distribution of accessibility across the base, so what is
  decaying is visible before it is unreachable.
- **Retrieval** — recall@10 and MRR read from the positions judged searches
  actually gave. Not a proxy score. This is the same computation the judge page
  already reports as today's number; here it is a series.
- **Gaps** — the count over time, beside the list in 7a.

Every one of these is an aggregate over tables that already exist. No new table,
no new sweep, no model call.

## 8. The visual pass — V1

Today's palette and both themes, unchanged: the warm paper light and the cool
ink dark in `00-tokens.css`, the same accents, the same radii, the same three
theme states and the two selectors that resolve them.

What changes is execution:

1. **One type scale, obeyed.** `00-tokens.css` already defines seven steps and
   its own comment says why: sizes were chosen per component, so two things
   meaning the same thing rarely looked the same size. Components pick a step.
   Three roles carry almost everything: title, body, meta.
2. **The score gets a shape.** A five-cell meter beside the monospace decimal,
   so rank is scannable without reading numbers. The cliff rule and the
   past-cliff greying are unchanged; this makes what they are separating
   legible at a glance.
3. **Region micro-labels.** Uppercase, letterspaced, with a hairline rule
   running out from each. This is most of why the workspace reads as organised
   rather than as three lists that happen to be adjacent.
4. **Air between bands**, filled selected rows rather than outlined ones, and
   the source strip as a left-bordered monospace block.
5. **Focus rings** from `--color-accent-dim`, consistently, on everything
   reachable by keyboard.

No new font file. No motion beyond what exists — the pane cross-fade on artifact
swap and the rail stagger stay as they are, and the reduced-motion rule keeps
emptying both.

## 9. Code shape

`src/web/ui.rs` is 10,880 lines. Three page handlers collapsing into one is the
moment to extract, and the extraction is scoped to exactly what this work
rewrites — not a general refactor of the file.

**Moves to `src/web/workspace.rs`:** the workspace handler and its template
struct, `search_results` and `UiSearchParams`, the ask handlers
(`ask_page`/`ask_submit`/`ask_stream`/`ask_verdict`/`ask_carried`/`ask_keep`)
with `sse_event`, `rail_fragment`, `answer_fragment`, `verdict_label`,
`ask_verdict_bar`, and the capture handlers (`capture_page`, `capture_submit`,
`CaptureForm`).

**Moves to `src/web/insights.rs`:** the Housekeeping handler and its actions,
`queue_fragment`, the pair-decision handlers, `gap_dismiss`.

**Deleted, with measured sizes:**

| File | Lines | Becomes |
|---|---|---|
| `templates/search.html` | 149 | `templates/workspace.html` |
| `templates/capture.html` | 355 | ″ |
| `templates/ask.html` | 84 | ″ |
| `templates/_sitting.html` | 16 | nothing |
| `assets/css/40-search.css` | 561 | `assets/css/40-workspace.css` |
| `assets/css/41-capture.css` | 186 | ″ |
| `assets/css/45-ask.css` | 66 | ″ |
| `ui.rs` `search_page` | 236 | one handler |
| `ui.rs` `capture_page` | 69 | ″ |
| `ui.rs` `ask_page` | 37 | ″ |
| `ui.rs` `sitting_rail` | 56 | nothing |
| `app.js` `commandBar()` | ~55 | nothing |
| `layout.html` `.cmdk` markup | ~20 | nothing |

The implementation plan records the after-figures beside these so the claim is
checkable rather than asserted. The expectation is a clear net reduction in
templates and CSS — most of those 813 CSS lines are three pages disagreeing
about width, and most of `capture.html` is file furniture that now renders only
when a file is staged — and roughly flat Rust for the workspace, with §7b adding
genuinely new code.

`45-ask.css` and `41-capture.css` disappear as files; the rules that survive
them are the ones about the answer and the staged file, and they live in
`40-workspace.css` beside the box they belong to.

## 10. Testing

- **`tests/browser/ask_stream.js`** drives `/ui/ask` and asserts on
  `#ask-verdict`, `#ask-live`, `#ask-result` and the Stop button. It moves to
  the workspace URL; every id it targets is kept deliberately stable, and the
  disabled-during-ask rule gets assertions of its own — including the
  `onerror` path, which is the one that would otherwise lock the page.
- **Route tests** for each surviving door in §3, asserting the workspace renders
  with the right state and that `/ui/ops` redirects.
- **`tests/eval.rs:171`** names `/ui/ask` in a message string only; the string
  is updated, the test is not.
- The existing `ui.rs` unit tests move with the handlers they cover.

## 11. What this is not allowed to break

1. **No reachable route stops answering.** `/ui/search`, `/ui/ask`,
   `/ui/capture` and `/ui/ops` all keep responding — as deep links or as
   redirects, never as a 404.
2. **No fragment endpoint changes shape.** The browser extension, the REST API
   at `/api/v1` and the MCP server at `/mcp` are untouched by this work and must
   stay untouched.
3. **`prefill_text`, `prefill_ask` and `prefill_question` keep working.** The
   extension posts into `/ui/capture`, and so does *keep this answer*.
4. **`core::sitting` keeps every writer and every reader it has.** Only its two
   UI surfaces go.
5. **The phone rules in `50-phone.css` keep their reasons.** The fixed bar at
   the thumb, the 16px inputs that stop iOS zooming a standalone window, the
   44px targets, the safe-area insets, the `has-selection` rail hiding and the
   back link at the container width rather than the phone width.
6. **The retrieval is not touched.** No change to ranking, the cliff, priming,
   activation, gaps, pursuits or what any of them record. The `Door` an event is
   recorded against must keep meaning what it means.
7. **Both themes stay complete**, and no colour gets its only definition inside
   a media query or a `[data-theme]` block.
8. **The page works without JavaScript to the extent it does today**, and no
   further: `/api/v1/ask` and MCP remain the JS-free ways to ask.

## 12. Out of scope

- Any change to the browser extension. It already has this surface; this brings
  the web UI to it.
- Any change to what capture, search or ask *do*. Verbs are re-arranged, not
  redefined.
- The `/ui/judge` page, `/ui/pair`, `/ui/extension/install`, `/ui/settings`, the
  corpus pages and the lineage view. Judge keeps its tab and its own screen.
- The general refactor of `src/web/ui.rs`. Only the handlers this work rewrites
  move out of it.
