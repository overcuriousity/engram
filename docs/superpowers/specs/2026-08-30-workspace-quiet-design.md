# The quiet workspace: a retired reminder, a live due band, and a page that says what it is for

Four complaints, one page. A reminder that is done still sits at the top of
*Last captured* forever. The due band only appears on a reload, so a reminder
armed a second ago is invisible until you go away and come back. The front page
renders the same five captures twice, in two shapes, ten centimetres apart,
under four muted prose lines. And nothing anywhere tells a person that
*remind me Friday to send the invoice* is a sentence this program understands.

This design fixes all four, and three of them land in the same place: the state
the workspace is in when the box is empty, which is the state it is in on most
opens.

## 1. The idle page

Everything above the box stays where it is. The box, the verb row and the
header do not move, so the page does not jump when it stops being idle. What
changes is what renders *below* the box while the box is empty.

**The Kind chips hide while the box is empty.** They qualify a search, and on
an idle page there is no search. They appear on the first keystroke, on the
same event that reveals the rest of the page.

**The two muted prose lines collapse into one guidance line.** `attach-types`
and `box-hint` both go. The accepted file types already live on the Attach
label's `title` and on the input's `aria-label`; the sentence about typing
searching as you go is taught better by an example than by a claim. What
replaces them is section 4.

**The context offer, then what is due**, in that order, directly under the
guidance line. This is the order `workspace.html` already renders and it is the
right one: the offer is speculative and the due band has a hard reason, so the
band is what the eye should land on last and lowest, where it is closest to the
rest of the page.

**One closing line, muted**, at the bottom of the column:

> `11,528 artifacts from 235 sources · last kept "der vestigo-MCP-server…" today`

The title links to its source, *today* links to the day page. This one line
replaces both the rail's *This memory* block and its whole *Last captured*
list. That list is half of the duplication the page currently shows; the other
half is the capture receipt column, which is not idle-state content at all and
only appeared to be because the rail was rendering the same rows beside it.

**The rail and the pane do not render while the box is empty.** The rail's
first content is the first result set; the pane's first content is the first
artifact or capture receipt. Both regions collapse to zero height while idle,
so the column below the box is the whole page.

`_rail_idle.html` becomes `_idle_foot.html`: the same data, rendered as the
closing line above and swapped into the idle column rather than into
`#rail-head`. It still has to exist, because emptying the box brings the idle
state back and something has to come with it.

`_pane_idle.html` survives for exactly one case: the empty base. On a base with
nothing in it there are no counts to print and no last capture to name, and the
program has to say what it is for — so on `held = false` the closing line is
replaced by the first-run sentence those two fragments carry today ("Nothing
here yet. Paste anything worth keeping…", and what happens to it when you do).
On a base with contents, neither renders while idle.

Net effect: four prose lines become one, one of two recent-capture lists is
gone, the marooned chip row is gone, and the empty third column is gone.

## 2. Done retires the note

`done` does not delete anything today, and will not after this. It sets
`done_at` on the moment; the artifact stays embedded and searchable, and keeps
its place at the top of *Last captured* forever. That is the complaint.

**The rule.** When a moment is completed and no open moment remains for that
artifact, and the capture carried `intent = "remind"`, the source is marked
retired.

The recurring case falls out for free: `complete_moment` arms the next
occurrence, so an open moment still exists and nothing is retired. Snooze does
not retire. *move* does not retire — it closes the old row and writes a new
open one, so the "no open moment remains" test fails, which is correct.

**Retired means two things and nothing else.**

It leaves the closing line of section 1, and it leaves any future recent-capture
list. The day page keeps it: a day page is a record of what actually happened.

In search it keeps its rank and is pushed below the relevance divider, badged
`done reminder`. Not deleted, not silently filtered. Search for the words and it
is still there — it has simply stopped competing with the things that were
written to be kept.

**Storage.** One nullable `retired_at` on the corpus, written by
`complete_moment`, read by `recent_captures` and by the search fold. No new
table, no deletion path, nothing to garbage-collect. `undone` clears it, so the
undo already on screen restores the row and the note together.

**The judgement call.** This fires only for notes engram classified as `remind`
at capture. A date set by hand on an ordinary note — an article you gave a
deadline — is not retired when done, because that note was never a reminder
cue. It is a document with a date on it, and it stays a document.

## 3. The due band: live, and styled

### Live

Two mechanisms, both cheap.

The band is part of the idle column, so it re-renders whenever the box empties,
which is what a capture does. That covers the common case: type *remind me in
15 minutes about the background test*, press Capture, the box clears, the band
comes back holding it.

Intent classification is a background job, so the moment may not exist at that
instant. Hence the second mechanism: the fragment carries its own polling
trigger and computes its own delay, exactly as `_queue.html` already does. One
pattern for "a fragment that watches something", not two.

| Situation | Re-fetch |
| --- | --- |
| capture queue still moving | 2s |
| next due time inside 5 minutes | at that second |
| next due time further out | 5-minute cap |
| nothing pending, nothing due, empty queue | no trigger emitted at all |

An idle page with nothing armed makes no requests.

### Styled

The current row is a title, a stamp and seven controls on one line: `done`,
three snooze buttons, a datetime input and `move`. The same clutter problem as
the front page, one scale down.

- The band becomes a card with a left accent, visually distinct from the
  context offer above it, so the two never read as one list.
- A row is **title · when**, with `done` as the only visible button. Snooze and
  *move* collapse behind a per-row `later` disclosure.
- Overdue rows carry the accent at full strength, upcoming rows carry it faint.
  An undated row keeps its *when?* and opens the date field directly, since
  that is the entire point of the row.
- A row that arrived since the previous render gets a brief highlight on entry.
  Without it "real time" is invisible: the band would grow silently while you
  are looking at something else.
- *Coming up* stays as it is, one muted line.

## 4. Guidance: examples on the page, echo while typing

### Examples

The single guidance line from section 1 is:

> Try: **"remind me Friday to send the invoice"** · **"today I finally fixed the
> rebuild"** — or paste a whole paragraph; a sentence finds more than keywords do.

Two clickable chips that fill the box without submitting, so pressing one shows
the echo below immediately and teaches the whole loop in one click. The
trailing clause keeps the one true thing the old `box-hint` said.

The chips are drawn from the `PROTOTYPES` table in `src/core/moments.rs`,
picked by the browser's `Accept-Language`, English as the fallback. That table
already carries German, French, Spanish, Portuguese, Italian, Dutch, Polish,
Turkish and Russian, so a German reader is shown *erinnere mich morgen an den
zahnarzttermin* — and there is no second copy of the examples to keep in step
with the classifier that reads them.

### Echo

No new endpoint and no new request. `/ui/search/results` already receives every
keystroke's `q` on a 120ms debounce; it renders an extra out-of-band fragment
into an `#intent-echo` slot under the box. `cue()` and the date rules are pure
string work with no model and no store, so this costs nothing measurable on a
request that was already being made.

| Input | Echo |
| --- | --- |
| cue matched, date read | `reminder · Fri 4 Sep 09:00` |
| cue matched, no date | `reminder · no date read — it will ask you for one` |
| journal cue matched | `journal entry · today` |
| no cue matched | nothing at all |

The echo only ever claims what the cue table proves. It never says "not a
reminder": the embedding classifier at capture can fire where the cue table did
not, and an echo that promised otherwise would be lying. That surprise lands in
the safe direction, and the arrival highlight from section 3 is what makes it
visible when it happens.

**One deliberate cut.** The echo is read-only. No inline "correct the date
before capturing": the band below already has *move*, and a pre-capture
correction would mean threading an explicit `at` through the capture path for
something that is one click away afterwards.

## Error handling

The echo is decoration on a search response. If the OOB fragment is missing or
malformed, the search still lands and the slot keeps whatever it held; nothing
about the box's behaviour depends on it.

The due band's poll is a fragment swap. A failed fetch leaves the previous band
on screen and the next scheduled fetch retries; there is no error state to
render, because "the list you already have" is the correct thing to show.

`retired_at` is advisory. If it is set on something it should not have been,
the note is still in the base, still searchable, still on its day page, and
`undone` clears the flag. Nothing about retirement is lossy, which is why it is
a flag and not a delete.

An unparseable `Accept-Language` falls back to English examples.

## Testing

- **Idle page** — with an empty box the response contains no `#rail-head`
  content, no `kind-chips`, and exactly one recent-capture element. With a
  filled box all three are present.
- **Retirement** — a completed one-shot `remind` capture leaves
  `recent_captures`; a completed occurrence of a *recurring* moment does not;
  a hand-dated ordinary note does not; `undone` restores it; the retired note
  is still returned by a search for its own words, below the divider, badged.
- **Poll cadence** — the fragment emits no `hx-trigger` when nothing is due and
  the queue is empty; emits a 2s trigger while the queue is moving; emits a
  trigger timed to the moment when one is due inside five minutes.
- **Echo** — `remind me Friday to send the invoice` echoes a reminder with a
  Friday date in the viewer's zone; a note with no cue echoes nothing; a
  German cue echoes in the same shape. These are assertions over `cue()` and
  the date rules, which are already pure and already tested — the new tests are
  over the fragment, not the parsing.
- **Examples** — `Accept-Language: de` renders a German example; an unknown
  language renders the English one.
