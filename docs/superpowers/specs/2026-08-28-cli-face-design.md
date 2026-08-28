# The terminal door, drawn and joined

**Status:** design, awaiting review
**Date:** 2026-08-28

## The situation

`src/cli/` is 3 424 lines and already has a face. `face.rs` carries a `Pulse`
for a request in flight, a `Fill` for bytes leaving the process, three `Lamps`
for the background stages a capture passes through, a `Readout` measuring the
rate an answer's tokens arrive at, and a ranked list with score rungs and a
broken trace past the cliff. It was written to three rules, stated at the top of
the file: it never survives a pipe, it never delays a result, and it never says
by colour or by glyph alone what it must say in words.

Those rules are right and this design does not touch them. What it touches is
the two places the door falls short of them, and the one place where the door is
connected to nothing.

**The wait is the only dishonest thing in the file.** `Readout` says of itself
that "the amplitude is measured, never invented", and it earns that: it draws
the arrivals it saw. `Pulse` is the opposite — a cell travelling along a strand
on an 80 ms timer, drawn identically for a search that took 90 ms and one that
took nine seconds, saying only "a request is open". Every other display in the
file is a function of something that happened. This one is a function of the
clock.

**The list spends its width on the wrong things.** A hit's line carries the raw
fused score to two decimals and a 26-character id, and the cliff — the one
element the frontend audit named as worth keeping — is marked only by a dimmer
row and a `╵` instead of a `┃`. The score is not a probability; `search::prime`
says so about those very numbers, and a whole list of them is routinely
negative. The id in full is not needed either: `last::resolve` already accepts a
leading piece.

**Colour is decided by a rule that is too coarse in one direction and absent in
the other.** `Face::decide` folds `NO_COLOR` into the single `on` flag, so
`NO_COLOR=1 engram -c big.pdf --watch` loses the lamps, the upload track and the
layout — none of which are colour. In the other direction `--fancy always`
ignores `NO_COLOR` entirely, so the flag that means "I want the drawn form"
silently overrides a preference that was never about drawing.

**And the shell is a door the base does not learn from.** This is the finding
that turned a face change into a system change:

- `engram -s` reaches `GET /api/v1/search?door=cli`. The event is recorded, the
  door is `captured()`, the pursuit sweep reads it.
- `engram --show` reaches `GET /api/v1/artifacts/{id}`, which calls
  `record_interaction(cid, None, Some(subject))` — the open is logged.
- They never join. `src/web/api.rs:1044` hands a non-typing door
  `door.into()`, which is `scope: None`. The interaction carries
  `scope: Some(subject)`. `jobs/pursuit.rs:317` attaches an interaction to a
  search only `if e.scope == i.scope`.

A shell-only session therefore opens pursuits that accumulate exactly zero
engagement, fall under `min_engagement = 3.0` and `min_sources = 2`, and close
`unsatisfied`. The CLI can widen the hole in the base and can never fill it.

`engram -a` is worse. `Core::record_ask` admits `Door::Ui` alone
(`src/core/ask/mod.rs:911`) and `/api/v1/ask/stream` labels every caller
`Door::Extension` (`src/web/api.rs:1331`), so a question typed at a shell — the
strongest statement of a need engram can receive — is written down nowhere at
all.

## The principle

**Nothing is drawn that did not happen.**

The face already half-believes this. This design finishes it: the animation
during a wait becomes a report of the stage the server is actually in, the
progress display is drawn only for stages that are actually going to run, and
what the shell does is actually recorded, so the base can learn from a person
who never opens a browser.

The corollary, which decides several arguments below: **engram does not own the
terminal.** Not its palette, not its scrollback, not its alternate screen. This
is the conclusion `c853210` reached for the mark — an ink that composes on one
ground only is the wrong ink — applied to a surface whose ground is a stranger's
colour scheme.

## 1. The wait says which stage

### The stage channel

`Core::search_inner` already measures the pipeline: it stamps `embed_ms` at
line 1087 and `total_ms` at 1441, and reports both as `server-timing`. The
spans exist; nothing publishes their *starts*.

Add one optional parameter to `search_inner` — a `Option<mpsc::Sender<Stage>>` —
and send at the head of each span. The three public entry points (`search`,
`search_with`, `search_with_ranking`) pass `None` and are otherwise unchanged, so
the tuning sweep and every existing caller keep the signature they have.

`Core::search_events(query, cap, origin) -> impl Stream<Item = SearchEvent>`
wraps it, in the shape `ask_events` already established: one `async_stream`,
terminal by construction, ending at its first error.

```
stages  { "stages": ["embed", "retrieve", "rerank"] }
stage   { "stage": "retrieve" }
stage   { "stage": "rerank" }
results { "results": [ … ] }
```

The first frame is the load-bearing one. **The server names the stages this
search will pass through, and the client draws lamps for those and no others.**
A search with the reranker off never shows a `rerank` lamp waiting to light,
because there is nothing there to light it. A stage list is a promise about work
that is going to happen, which is the only kind of progress display this
codebase permits.

### The route

`GET /api/v1/search` is unchanged — a bare array, as scripts, `/mcp` and the
extension read it today. Streaming is a second route, `GET
/api/v1/search/stream`, taking the same query parameters including `door=`, and
answering `text/event-stream`.

Not a content negotiation on the existing path: the response shape of `/search`
is depended on by four clients, and the one thing worse than adding a route is
having an existing route answer two different shapes depending on a header.

### What the CLI draws

`Pulse` is deleted. In its place, the same `Lamps` renderer `-c --watch` already
uses, driven by the frames:

```
  ● embed   ● retrieve   ◉ rerank
```

Two rules govern when it appears:

- **Nothing is drawn for the first 120 ms.** A fast search shows no motion at
  all. Most searches on a warm box are fast, and a display that flashes on and
  off is worse than no display.
- **The results frame is the last frame.** The list is printed the moment it
  arrives; nothing is buffered to let an animation finish. "Never delay a
  result" is preserved because there is no timer left to wait for.

Fallback is by status: a `404` or an unparseable stream falls back to the plain
`GET /api/v1/search`, so a new client against an old server loses the lamps and
nothing else.

### The ask already streams its milestones, and the CLI throws them away

`AskEvent` (`src/core/ask/stream.rs:11`) emits `Retrieved { round, retrieved,
shown, dropped, cliff_at }`, `Needs`, `Citations` and `Reasoning` before the
first `Token`. `src/cli/ask.rs:174` matches on `token`, `citations` and `error`,
and drops the rest — and then draws `pulse("thinking")` over the gap. Truthful
material discarded and invented motion put in its place, in one function.

The strand is replaced by what the frames say, one line each, on stderr, taken
back when the answer starts:

```
  ● retrieved 24, showing 6
  ◉ still missing: journal rotation policy
```

`Reasoning` is a stage line too, not prose in the answer — the answer body stays
exactly what the model wrote. Nothing here is drawn on a timer, and when a fast
ask arrives with nothing before its first token, nothing is drawn at all.

### The web door reads the same frames

The search box consumes `/search/stream` and renders the same three stages. This
is the reason the channel lives in `Core` rather than in the CLI: one definition
of what a search is doing, rendered twice, rather than two implementations that
will disagree within a month.

## 2. The list

Before:

```
┃  1 ▆ -0.18  systemd-journald ring buffer  01J8Z4K2QW7NR3T9X0YB5C6D8E
┃    [merged]
┃    The journal is a ring buffer on disk; SystemMaxUse caps its total
┃    size and MaxRetentionSec caps its age. Both apply per-namespace.
┃
```

After:

```
  journal size cap                              7 hits · 412 ms

   1  ▇▇▇▇▇▇▇  systemd-journald ring buffer         01J8Z4K2
                merged · 2 sources
      The journal is a ring buffer on disk; SystemMaxUse caps its
      total size and MaxRetentionSec caps its age.

   2  ▇▇▇▇▇░░  journalctl --vacuum-size             01K2M9PA
      Removes archived journal files until the total falls below
      the given size.

  ──────────────────  relevance falls off here  ──────────────────

   3  ▇▇░░░░░  btrfs subvolume quotas               01J7QQ4T
      Quota groups account for shared extents, so the reported…

  engram --show 2   reads hit 2 in full
```

- **The cliff is said in words.** A divider carrying the sentence, which is what
  the results rail on the web already does and what the audit named as the
  thing to keep. Once it is there the `┃` trace carries nothing the divider does
  not, so the trace goes. This is a deletion.
- **The score number leaves the human rendering.** The bar carries
  rank-within-list, which is all the number honestly means; `--json` and
  `--explain` still carry the float for anyone debugging ranking.
- **The rung becomes a seven-cell bar.** The same eight steps `Scale::rung`
  already computes, drawn wide enough to compare two rows at a glance. It costs
  six columns, and that is the one genuine trade in this section.
- **The id is truncated to eight characters**, which `last::resolve` already
  accepts as a prefix.
- **Badges move under the title** as words separated by `·`, replacing the
  bracketed line.
- **A header and a footer**, both measured: the query, the count, the elapsed
  time from the stream, and one line naming the next command.

`render_plain` is untouched. There remains exactly one rendering a script can
ever see, and the drawn form still falls straight through to it when the face is
off.

## 3. Colour

`Face` becomes `{ on, color, unicode, width }`, and `decide` separates the two
decisions it currently conflates:

| Input | `on` | `color` |
|---|---|---|
| pipe, `--json` | off | off |
| `--plain` | off | off |
| `NO_COLOR` | **on** | off |
| `--fancy never` | off | off |
| `--fancy always` | on | off if `NO_COLOR`, else on |

`NO_COLOR` keeping the layout is the fix; `--fancy always` no longer overriding
it is the other half.

**Only the ANSI 8 are used. No 256-colour indices, no RGB.** The user's theme
has already mapped those eight to inks that work on their ground; a hex value
lifted from `app.css` would compose on `#0e1015` and nowhere else, which is
precisely the failure the mark commit fixed.

| Role | SGR | Where |
|---|---|---|
| body, titles | *none* | The default foreground is the highest-contrast ink available and is never overridden. |
| hierarchy | `1` bold | Titles above the cliff. Weight, not hue. |
| recessive | `2` dim | Ids, timings, header, footer, every row past the cliff. |
| the one accent | `36` cyan | Score bars above the cliff, the running lamp, the divider rule. The nearest ANSI reading of `--color-accent`. |
| failure | `31` red | Stopped lamps, refused captures, failed job rows. |
| done | `32` green | Finished lamps, confirmed. |
| caution | `33` yellow | `NeedsReview`, `Partial`. |

Four hues and two attributes, and the third rule holds unchanged: every claim
made in colour is also made in words or in a glyph. Strip every escape from the
mockup above and it is the same rendering.

## 4. `engram --status`

`/api/v1/status` (`src/web/api.rs:1278`) already answers the machine half:
corpora by status, job counts, failed jobs, oldest pending age, artifact and
vector counts. It says nothing about what the base has been learning, which is
the half that is invisible from a shell.

`StatusResponse` gains a `learning` block, present only when `learn.enabled` —
pursuits open, pursuits closed unsatisfied, artifacts written from pursuits,
open gaps. Additive: every existing reader of the endpoint is unaffected.

A new verb, `--status`, one shot like the other four. `--json` prints the
server's own body unchanged, by the rule `-s --json` already follows. Exit `0`,
or `2` when the endpoint cannot be reached.

```
  engram.mikoshi.de                    1 842 artifacts · 1 842 vectors

  sources    1 780 ready   3 embedding   1 needs review   2 failed
  jobs           4 pending   1 running   2 failed · oldest 38 s

  learning   7 pursuits open · 4 closed unsatisfied · 11 gaps
             3 artifacts written from pursuits

  2 failed jobs
     embed   01J8Z4K2   connection refused          4 h ago
     extract 01K2M9PA   no text in the PDF          2 d ago
```

No footer on other verbs, and no ambient status line. A status display that
costs a request per invocation would make the cheapest door in the application
pay for information nobody asked for at that moment.

## 5. The shell becomes a door the base learns from

### Searches carry a scope

`src/web/api.rs:1044` currently reads *typing* and *scoped* as one decision.
They are two. `Door::Cli` is not a typing door — it wants the reranker, it marks
what it retrieved — and it is a door with a known subject.

```rust
let origin = match door {
    Door::Extension | Door::Cli => door.by(tenant.user.subject),
    _ => door.into(),
};
```

`Door::Api` and `Door::Mcp` are deliberately left unscoped: a bearer token is
not a person, and two agents sharing one token would fold into each other's
queries — the reason `sitting.rs` refuses to keep a sitting for those doors.

The cost, stated: two shells belonging to one user, searching within
`coalesce_secs`, fold into one recorded event. That is the same fold the web UI
already accepts per subject, and it is the correct behaviour far more often than
not — a query retyped in a second terminal usually is the same query.

### Asks are recorded, and every door says its own name

`record_ask`'s gate widens from `origin.door == Door::Ui` to `Ui | Cli`, and
`/api/v1/ask/stream` stops hardcoding `Door::Extension`: the request names its
door the way `?door=` already does for search, defaulting to `Extension` so the
panel's behaviour is unchanged when it says nothing.

A recorded CLI ask arms a pursuit through `asks_between`, and its citations
count as engagement through `mark_artifacts_cited`, which already runs on that
path. It carries no `event_id` to the judge page — only `Door::Ui` asks are
judged, and that stays true. **Recorded and unjudged is a coherent state**: the
sweep learns what was needed, and the judge still only grades answers a person
saw in a place where they could grade them.

### What this earns

A person who lives in a shell can now search, read, ask, and have the run of it
close as a satisfied pursuit that writes one artifact — the thing the feature
exists to do, and the thing it could not do from this door at all.

## Testing

- `engram -a` renders a `retrieved` frame as a stage line and never draws
  anything while no frame has arrived.
- `search_events` emits the stage list it will run and no stage it will not,
  asserted with the reranker on and off.
- The 120 ms floor: a search answered immediately draws nothing.
- `/api/v1/search` still answers a bare array, byte-identical to today.
- `Face::decide` truth table, all five rows.
- The drawn list with every escape stripped equals the drawn list rendered with
  `color: false` — the "said in words" rule, as an assertion rather than a
  comment.
- A CLI search followed by a CLI `--show`, swept: the pursuit's engagement
  clears `min_engagement` and it does not close `unsatisfied`. This is the test
  the whole of §5 exists for.
- A CLI ask is recorded; an API ask still is not.
- `--status --json` is the server's body, unchanged.

## Explicitly not in this design

- **No interactive session.** No REPL, no arrow-key selection, no alternate
  screen. Every verb remains one shot that leaves its output in scrollback, and
  the speed of `engram -s` is the property this whole design is written around.
- **No status footer on ordinary verbs.**
- **No RGB, no 256-colour, no theme detection.** See §3.
- **No scope for `Door::Api` or `Door::Mcp`.** See §5.
