# Fixing what the GUI does badly

A review of the live deployment at `engram.mikoshi.de`, and the design for
what to do about it.

## Base

Prod runs `origin/feat/one-system`, nineteen commits ahead of `master` — the
sitting, the sweep history on Housekeeping, and the knowledge gaps on Capture
are all on that branch and on no other. The review was of that build, so the
work branches from it (`fix/ui-findings`) rather than from `master`. Every
finding below survives the rebase; only line numbers moved.

## What this covers

Findings from operating the deployment: search, the artifact pane, a full
ask, capture, judge, housekeeping, settings, the light theme, and a 420px
viewport. Deliberately out of scope, by the owner's triage:

- **PDF ingest quality.** Most `Untitled` artifacts and the hex-dump and
  table-rule segments come from PDF extraction. A general weakness, possibly
  not fixable at this layer.
- **Images inside PDFs.** Not extracted; rendered in the raw source only.
- **An absolute relevance floor.** A nonsense query returns ten results
  because hybrid scores are fused ranks with no cross-query meaning. Accepted
  as fine in a vector base with no explicit tuning.
- **Judge's IR vocabulary.** `recall@10`, `MRR`, `hits · finds · gaps ·
  discarded` are tuning instruments, not a surface meant to survive.

## The decisions

**A `Conflict` still escalates.** The judge's four verdicts already implement
the three forks a person would want: `Replaced` supersedes without asking,
`Duplicate` merges into a new artifact naming both parents, `Distinct` leaves
both alone. `Conflict` — same subject, different value, nothing in either text
saying which is current — is the only one that reaches a person, and it stays
that way. Nothing decides which of two facts is true without the operator.
Recency was considered as a tiebreak, on the rule `keeper()` already uses for
clusters, and rejected: a newer capture that is wrong would silently bury a
correct older one, and `Undo` on Housekeeping is a poor place to discover
that. If a recency tiebreak is ever wanted, its threshold belongs under
`[consolidate]` beside `review_min` — recorded here so the option is not
re-derived from scratch.

**Reasoning becomes opt-in.** The ask page currently streams the model's
chain of thought, which restates the prompt's constraints verbatim. It moves
behind a closed disclosure rather than being deleted: it is genuinely useful
while tuning, and it should never face a reader who did not ask for it.

**The search rail widens; the pane keeps its placeholder.** Auto-opening the
top hit was considered and rejected — it would spend a read on every search,
including the ones whose answer is visible in the rail.

## 1. Titles

The multiplier. Every list in the app is a column of titles, so a bad title
rule degrades search, judge, recent, and the dedupe queue at once.

The right rule already exists, with its argument, at `ui.rs:1157`: a verbatim
passage has no title by design, and `Untitled` is a word that says nothing
while looking like it does. Three places do not follow it:

- `web/judge.rs:179` and `:515` — `unwrap_or_else(|| "Untitled".into())`
- `web/ui.rs:367` and `:3095` — `format!("Chunk {}", c.ordinal)`
- `mcp/mod.rs:19` — `unwrap_or_else(|| "Untitled".into())`

All adopt the rail's treatment: the opening of the body stands in for the
heading, marked as a stand-in rather than presented as a name.

**Truncation respects word boundaries.** The sitting renders
`Die digitale Forensik unterscheidet sich zusätzlich darin vo` — a cut mid-word.
Wherever a stand-in title is shortened, it ends at a word.

**Colliding titles get a disambiguator.** Three distinct artifacts on the
deployment carry the title `LevelDB: Funktionsweise und forensische Analyse`.
When two rows of one list share a title, each says what distinguishes it.
Without this, section 2's grouping still looks like a repeat.

## 2. The consolidation queue

Capture's "Needs you" showed five cards. Three were **one cluster of four
artifacts asked as three separate pairwise questions** — `01a01f80…` against
three others. `jobs/consolidate.rs` already holds the disjoint-set (`Clusters`)
that knows they are one group; the capture page does not use it. One cluster
becomes one card naming its members.

**`detail` is null on every escalated pair, and should not be.** The field is
stored (`store/pairs.rs:108`), read and rewritten for the `link` marker
(`ui.rs:1879`), rendered (`_decide.html`), and the prompt requires it —
"detail: one short sentence saying why. Always." Yet all five stored
`Contradiction` rows have it empty, so no card says what the disagreement is
about. This is a defect to root-cause before any UI work: a card that cannot
name the dispute cannot be decided however it is laid out.

**Each side becomes readable in place.** The titles are already links, but
following one leaves the queue. An expandable excerpt of both artifacts inside
the card, with the differing values marked, makes the decision local.

## 3. Ask

- Reasoning moves into a disclosure, closed by default (`ask.html:42`,
  `app.js:285`).
- The retrieval line (`#ask-progress`) becomes the primary progress signal,
  with elapsed time. Today a fifty-second wait is signalled by a small grey
  `thinking…` beside the button.
- A **Stop** control closes the `EventSource` and keeps what has arrived.
- `{{ dropped }} excerpt(s) omitted for context budget` (`_answer.html:16`)
  becomes plain English, pluralised properly.
- The verdict row (`_ask_verdict.html:14`) and "keep this answer" become
  controls that look like controls.
- **`.sitting` has no stylesheet.** `_sitting.html` renders `.sitting` and
  `.pane-label`; no rule for `.sitting` exists in any of the eleven CSS files.
  It needs one, and a heading that says what a sitting is.

## 4. Layout

The region grid (`20-layout.css`) is sound and documented; the problems are in
which regions each page claims.

- **Search**: the rail widens while nothing is selected, and shows whole
  snippets rather than two clipped lines. The placeholder stays.
- **Judge**: `None of these` / `Can't remember` / `Not a real search`
  (`_judge_card.html:78-89`) sit below twenty-three candidate cards. They
  become a sticky footer. Rank numbers stop after nine; they continue.
- **The artifact pane**: the title becomes sticky, `copy` moves out of the text
  flow it currently overlaps, `Delete` separates from `Verified`/`Hide`, and a
  segment cut mid-sentence links to its continuation — the source pane beside
  it already shows the rest.
- **Judge, Ops and Settings** use the width they have.

## 5. Copy and empty states

- Housekeeping's counts are one run-on sentence; they become a stat row.
- `TOOK` reads `now` for every sweep (`ops.html:60`); it reads a duration.
- Sweep identifiers (`arm_dedupe`, `pursuit`, `consolidate`, `retention`)
  get human labels, keeping the raw name available.
- One name for the page: the link says Housekeeping, the URL is `/ui/ops`, the
  title is `Ops`. `/ui/housekeeping` currently produces a browser error.
- Unknown `/ui` paths get a real 404 instead of the browser's.
- The API-tokens table renders bare headers with no rows; it gets an empty
  state.
- Capture's gaps row styles `asked` (a state) exactly like `ask again` and
  `covered` (actions).
- Markdown leaks into plain-text snippets — `custom\_passphrase`, `raw\_key`,
  `# Configure Linux…`, `**Was nicht abgedeckt ist:**`. Snippets get stripped.

## 6. Two tensions, left as they are

Both of these were findings in the review, and both collide with a decision
this codebase argues for in a comment. The comments win; they are recorded so
the question is not reopened blind.

- **KIND chips are hidden on the phone** (`50-phone.css:108`, "Not thumb
  furniture"). The row is correctly banished, but the *capability* goes with
  it — there is no way to filter by kind on a phone at all. Not fixed here.
  If it is ever wanted, an active-filter chip that appears only when a filter
  is set would respect the original reasoning.
- **The phone badge drops its number** (`layout.html:130`, "the count is not
  the point — that anything is waiting is"). Deliberate. Left alone.

## Testing

Every change is behavioural and most are template-level. The existing suite
already asserts on rendered HTML (`ui.rs:4562` asserts `Untitled` never
appears in the rail), which is the pattern to follow: assert on what the page
says. Specifically —

- Title fallback: a chunk with no title renders neither `Untitled` nor
  `Chunk N`, in the judge card, the artifact pane, and the MCP door.
- Truncation never splits a word.
- Two same-titled rows in one list render distinguishably.
- A cluster of three pending pairs renders one card, not three.
- A settled `Conflict` carries a detail line through to the page.
- Reasoning is not in the initial DOM of an ask.
- A 404 route returns the app's own page.

The dedupe `detail` defect gets a failing test reproducing the empty field
before it gets a fix.
