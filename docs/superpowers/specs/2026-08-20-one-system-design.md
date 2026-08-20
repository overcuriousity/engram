# One system — sleep, the sitting, and one queue for what is missing — Design

Date: 2026-08-20
Status: draft
Adds `src/jobs/sleep.rs`, `src/core/sitting.rs`; touches
`src/core/background.rs`, `src/core/search.rs`, `src/store/gaps.rs`,
`src/store/jobs.rs`, `src/store/schema.sql`, `src/web/ui.rs`, `ROADMAP.md`.
Adds no model call anywhere. See §3 for what it is not allowed to break.

## 1. Why

The mechanisms are built. What is missing between them is not a mechanism.

**The background work has no order.** Six tickers run on six intervals, each
started separately in `main`, each gated separately, none aware of the others:
`spawn_consolidation_ticker` (`consolidate.interval_hours`),
`spawn_repair_ticker` (its own period, behind no setting at all),
`spawn_retention_ticker` (`feedback.sweep_hours`, which also carries the gap
sweep), `spawn_dedupe_ticker` (`consolidate.dedupe_interval_mins`),
`spawn_pursuit_ticker` (`pursuit.idle_secs / 2`) and `spawn_associate_ticker`
(`associate.interval_mins`). Where their work is directional, nothing expresses
it: `Relate` arms the pairs that `Dedupe` and the consolidation sweep both
read, the three sit on two tickers whose periods are configured in different
units, and which of them runs first on a given night is whichever interval
elapsed first. Inside one ticker the order is right — `associate::run` replays
before it prunes, the retention ticker expires before it regroups the gaps —
which is exactly the point: the ordering that was easy to see was written
down, and the ordering that crosses a ticker boundary was not.

And nothing reports. Each ticker logs its own line at `info`. There is no
answer to *what did the memory do while I was away*, which is the question a
system that describes itself as sleeping has to be able to answer.

**The sitting exists, but only afterwards.** `jobs/pursuit.rs` reconstructs one
from idle gaps in the search log: searches closer together than
`pursuit.idle_secs` are one sitting, and a sitting that engaged several
artifacts without the base answering earns a pursuit. So the base can name what
you were working on last Tuesday. It cannot name what you are working on now.
`Core::record_interaction` writes `interaction_events` on every open, and
nothing reads that table until the sweep runs, `idle_secs` after you stopped.

The cost is paid at every door: search, ask, capture and judge are four pages
with nothing carried between them. A query typed on the rail is retyped into
ask. An answer worth keeping prefills the capture box and loses the question it
answered. Nothing anywhere says *these are the six artifacts this afternoon has
been about*.

**Four ways to say the base did not answer, ending in four places.** A search
judged `gap` becomes a `GapKind::Search`. An ask verdict of *nothing here*
becomes a `GapKind::Ask` by a second path. A quiet run of engaged searches
becomes a **pursuit**, which lands on Ops and is never a gap. And a search
abandoned with nothing opened — recorded deliberately, described in the README
as *the most telling of all* — becomes nothing whatsoever. One statement about
one hole, filed under four vocabularies, three of which the operator has to
visit separately.

## 2. Goal

Three seams, no new mechanism, no model call that is not already being made:

1. **Sleep.** One cycle, one ticker, phases in a fixed order, each phase
   keeping its own rhythm. One report per cycle, on Ops.
2. **The sitting.** A live, session-scoped working memory: what this sitting
   has touched, carried between the doors, expiring with it.
3. **One queue.** The four exits above become four `GapKind`s in the list that
   already exists, closable three ways, and a capture says what it closed.

### Non-goals

- **A scheduler.** No cron expression, no calendar, no "sleep at 03:00". A
  cycle is a loop with a period, as today; what changes is that there is one of
  it and its phases are ordered.
- **New background work.** Every phase is an existing function called in an
  order it was not called in before. Nothing new runs, and nothing that runs
  today stops.
- **Persisting the sitting.** It lives in memory, dies with the process, and is
  never written to `activation`. A working memory that survives a restart is a
  long-term memory, and engram already has one.
- **Moving ranking by default.** Working-memory priming changes the order of a
  result list. It ships behind a flag that is **off**, until the harness has
  been run on it. See §3 and §9.
- **Engagement at every door.** Retrieval is recorded over `/mcp` and the API;
  engagement is not (`mark_artifact_seen` and `record_interaction` have one
  production caller between them, the dwell route). Making the other doors
  engage is the natural next thing and is deliberately not here — it changes
  what activation means, and this design changes when activation is read.
  Named in `ROADMAP.md` [Associative Memory].
- **Access reconsolidation, error-driven re-synthesis, the corpus map.**
  Unchanged, still on the roadmap, and all three slot into the cycle §4 builds.

## 3. What this must not break

Four things in the tree are load-bearing and easy to lose here.

**Lazy decay stays lazy.** Activation and link weight both fold their decay in
at read and at bump. A cycle is a tempting place to put a decay pass, and it
would be a regression: a scheduled pass makes every number stale between
passes, and `bumping_folds_the_decay_in_rather_than_adding_to_a_stale_number`
pins the behaviour that avoids it. No phase writes a decayed value.

**Repair must not join the cycle.** `spawn_repair_ticker` carries a comment
recording exactly this mistake: its four passes used to ride the consolidation
sweep, and `consolidate.enabled = false` returned before their loop, so a
corpus left `segmenting` by a crash was never repaired on an install that had
switched consolidation off. `a_crashed_capture_is_repaired_with_the_sweep_switched_off`
pins it. Repair is what fixes an *interrupted cycle*; a cycle that owns its own
repair cannot recover from its own failure. It stays a separate ticker, behind
no setting, and this design does not touch it.

**One queued sweep, however often the ticker fires.** `jobs` is
`UNIQUE(stage, target_id)`, and every collection-scoped sweep enqueues against
the target `"collection"` to get at-most-one for free. The cycle keeps that
shape: one `Stage::Sleep` unit, one target.

**A default that changes ranking moves only after the harness has run.** The
sitting's carry and its display change no order and ship on. Its priming
changes order and ships off (§9).

## 4. Sleep

### 4.1 The shape

One ticker, `spawn_sleep_ticker`, enqueueing `Stage::Sleep` against
`"collection"`. One job handler, `jobs::sleep::run`, which calls the existing
phase functions in a fixed order. The five tickers it replaces are deleted;
`spawn_repair_ticker` is not (§3).

The phases, in the only order their data dependencies allow:

| # | Phase | What runs today | Rhythm |
|---|---|---|---|
| 1 | Replay | `jobs::associate::run` — events and verdicts folded into links, then `prune_learning_links` and `reopen_stale_judged_links` | `associate.interval_mins` |
| 2 | Pursue | `jobs::pursuit` sweep — groups quiet sittings, decides, arms `Generate` | `pursuit.idle_secs / 2` |
| 3 | Relate & dedupe | `Stage::Relate` arming, `jobs::dedupe` | `consolidate.dedupe_interval_mins` |
| 4 | Consolidate | `jobs::consolidate` sweep | `consolidate.interval_hours` |
| 5 | Retention | `expire_feedback` | `feedback.sweep_hours` |
| 6 | Gaps | `jobs::gaps::sweep` | `feedback.sweep_hours` |

Two of these orderings are already correct and are being written down rather
than fixed: replay before prune, inside `associate::run`, and retention before
the gap sweep, inside the retention ticker. Two are not expressed anywhere
today and are the reason the table is worth having: **relate before dedupe
before consolidate**, a pipeline spread over two tickers, and **replay before
pursue**, so a sitting is scored against links this cycle folded in rather
than against last half-hour's.

Two things that look like phases and are not:

- **Activation decay.** There is no decay pass and this design does not add
  one. `decayed(value, stamp, at, half_life)` is folded in at every bump and
  every read (`store/links.rs`), which is the better design — a lazily decayed
  number is never stale — and it means nothing that reads activation has a
  scheduling dependency on decay at all.
- **Promotion.** `jobs::promote::maybe_promote` fires at the bump, from
  `mark_artifact_seen` and from `associate`. It is event-driven, and the one
  scheduled call to it — from the pursuit sweep — is the cheap backstop
  `ROADMAP.md` [Core Platform] already proposes retiring. Nothing to schedule,
  so no phase.

### 4.2 Rhythms are kept, not flattened

Replaying a search log wants half-hourly. A consolidation sweep wants six-hourly
and would be wasteful hourly. Collapsing six intervals into one number would
either make the cheap phases rare or the expensive ones constant, and both are
worse than the accidental order this replaces.

So the ticker fires at the **shortest configured rhythm**, and each phase
carries a due-time: a phase runs when its own interval has elapsed since it last
completed. The last-completed stamps live in `meta`, which already holds exactly
this kind of cursor with no row to live on (`associate.events_after` and its
two siblings). Keys: `sleep.phase.<name>.at`.

A cycle where only phase 1 is due runs phase 1 and reports one line. The
existing per-subsystem interval keys are unchanged and keep their meanings —
this is the same six rhythms, ordered, in one loop.

### 4.3 The report

```sql
CREATE TABLE IF NOT EXISTS sleep_cycles (
  id         INTEGER PRIMARY KEY,
  started_at INTEGER NOT NULL,
  ended_at   INTEGER,
  -- Per-phase outcome, JSON: name, ran|skipped|failed, duration, counts.
  -- Read on Ops and rendered; never parsed for a decision.
  phases     TEXT NOT NULL DEFAULT '[]'
);
```

Bounded by the retention pass to the last 200 cycles — a cycle report is
housekeeping about housekeeping and is not worth a policy.

Each phase's counts are what its function already returns: links strengthened
and pruned (`associate::run` has them), windows promoted, pairs judged, merges
written, clusters named, rows expired. Ops gains one block at the top: *Last
slept 03:14, 40s — 412 links strengthened, 8 forgotten, 3 windows synthesised,
2 merges, 1 gap named.* A phase that failed says so and names its error; the
cycle continues to the next phase, because a failed consolidation is not a
reason to skip retention.

### 4.4 Failure and shutdown

Unchanged from what the tickers do now: a failed phase is retried on the next
cycle, its due-stamp not advanced, and nothing takes the process down. The
`Stage::Sleep` unit is claimed and released by the existing queue, so a cycle
interrupted by a restart is re-armed by the repair ticker like any other
interrupted unit.

## 5. The sitting

### 5.1 What it is

An in-memory map on `Core`, keyed by **sitting id**: the web session id for a
browser (`sessions.id`, which exists), the token id for the API and `/mcp`. One
entry:

```rust
pub struct Sitting {
    /// Artifact ids touched, most recent first, capped at CARRY.
    touched: VecDeque<Touched>,
    /// The queries typed in this sitting, most recent first, capped at CARRY.
    queries: VecDeque<String>,
    last_at: i64,
}
```

`CARRY = 20`. An entry idle for `pursuit.idle_secs` — the same number that
already defines a sitting for the sweep — is dropped, so the live definition
and the reconstructed one agree by construction. No table, no persistence, no
write to `activation`.

Touched is written where engagement is already recorded: `mark_artifact_seen`,
`record_interaction`, and an ask's citations. It costs a lock and a push, off
the request path like every other bookkeeping write in `search.rs`.

### 5.2 What it does — carrying (ships on)

Nothing here changes an order.

- **Search → ask.** The ask box arrives prefilled with the last query of the
  sitting when it is empty and the sitting is warm.
- **Ask → capture.** The keep-this-answer link already prefills the capture
  box; it gains the question it answered, as the note, so the trace records
  what the text was written for. This is the existing operator decision, better
  recorded — nothing is written without the person, and that line does not move.
- **A rail of what this sitting has touched**, on search and ask, six at most,
  each a link. Dismissible, and absent on a cold sitting.

### 5.3 What it does — priming (ships off)

A hit that is in `touched` is primed, by the same rank-based, bounded rule
`search::prime` already implements for activation, and **inside the same
budget**: a hit cannot be lifted `prime_lift` places for activation and again
for the sitting. Index 0 remains untouchable. A hit lifted by the sitting says
so on the rail — *seen in this sitting* — for the same reason a primed hit says
so today.

This is the one part of this design that moves ranking. `sitting.prime` is
`false` by default until `cargo test --test eval` has been run with it on and
off against the judged-pair set. §9.

### 5.4 What it does not do

It does not write activation, open a pursuit, or survive the process. The
pursuit sweep is unchanged and still reconstructs its own sittings from the
log: the live sitting is a *read* of what is happening, and a sweep that
depended on process memory would lose a day's pursuits to a restart.

## 6. One queue for what is missing

### 6.1 Two more kinds, not a new table

`GapKind` is `{ Ask, Search }` and a gap is a reference to the row it came from
plus its stored query vector. Both new exits fit that shape:

```rust
pub enum GapKind { Ask, Search, Abandoned, Pursuit }
```

- **`Abandoned`** — a recorded search that settled (past `feedback.coalesce_secs`,
  the same predicate `associate` uses to decide an event is settled), was never
  judged, has `answered = 0`, and has no `interaction_events` row in its window.
  Nothing was opened; the list did not answer.
- **`Pursuit`** — a pursuit that closed `unsatisfied`. Today it is a row on Ops
  and nothing else. Its clustered queries are already stored on the row and are
  exactly what a gap needs.

`gap_clusters.members` is already `(kind, id)` pairs and needs no change.
Clustering, naming, and the model call that names a group are untouched.

### 6.2 An abandonment is a weak gap

Most searches end without a click, and a system that called each of them a hole
would file the operator's typing as failure. Two guards:

1. An `Abandoned` gap is **never shown ungrouped**. The capture page's
   ungrouped list stays judged gaps only. An abandonment reaches the operator
   only by clustering with something else — and `MIN_CLUSTER` is 2, so the
   floor is *two questions about one hole*, at least one of which someone
   either judged or asked in earnest.
2. Only the last search of a burst counts. A typing burst already folds into
   one event by `coalesce_secs`; what survives folding is the query as it was
   finished.

### 6.3 Closing

Three ways in, three ways out.

- **A capture covers it.** When a corpus reaches `ready`, the open gaps' stored
  query vectors are searched against **that corpus's new artifacts only** — one
  filtered vector query per open gap, no model call, on the background handle.
  A gap whose best new hit is at or above `vector.weak_below` is marked
  covered, and the capture page says so: *this closes 2 open gaps*, naming them.
  This is the same test `search_events.answered` already applies, against a
  narrower set.
- **A pursuit earns it.** Unchanged: a pursuit that generates an artifact
  closes `generated`, and its gap closes with it.
- **The operator dismisses it.** Unchanged: `dismissed_at`, the existing
  button, now on one list instead of two.

### 6.4 What the operator sees

One list, on the capture page where it already is, each row badged with what
asked it: *judged*, *asked*, *abandoned*, *pursued*. Ops loses its separate
pursuits table and links into this list instead.

## 7. Surface

- **Ops** gains the sleep block (§4.3) at the top, and loses the pursuits table
  to §6.4.
- **Capture** gains kind badges and the coverage line.
- **Search and ask** gain the sitting rail (§5.2) and, with `sitting.prime` on,
  one more reason a hit can say it was lifted.
- Nothing gains a graph, a dashboard, or a chart. The search box stays the
  application.

## 8. Data model summary

| Change | Where |
|---|---|
| `sleep_cycles` table | new, §4.3 |
| `sleep.phase.<name>.at` cursors | `meta`, existing table |
| `Stage::Sleep` | `store/jobs.rs` |
| `GapKind::{Abandoned, Pursuit}` | `store/gaps.rs` |
| Sitting map | in memory on `Core`, no schema |

No column is dropped, no existing table is altered, and the migration is one
`CREATE TABLE IF NOT EXISTS`.

## 9. Configuration

```toml
[sleep]
enabled = true          # false runs nothing; repair is unaffected (§3)

[sitting]
prime = false           # moves ranking; off until the harness says otherwise
```

Two keys. The six existing interval keys keep their names and their meanings
and become phase rhythms (§4.2), so an operator who tuned
`associate.interval_mins` finds it still doing what it did.

`sleep.enabled = false` is for the harness and for debugging a phase in
isolation; it is not a supported way to run.

## 10. Testing

- **Order.** A cycle with all phases due runs them in the table's order —
  asserted on the recorded `phases` JSON, which is what makes the ordering
  claim testable at all.
- **Rhythm.** A phase whose interval has not elapsed is skipped and its
  due-stamp is not advanced; a phase that fails is skipped next cycle only if
  its interval has not elapsed, and retried otherwise.
- **The pinned regression.** `a_crashed_capture_is_repaired_with_the_sweep_switched_off`
  must still pass with `sleep.enabled = false` (§3). This is the test that
  fails if repair is ever folded in.
- **One unit.** A ticker firing three times before the cycle is claimed leaves
  one queued `Stage::Sleep`, as `the_ticker_queues_exactly_one_sweep` asserts
  for consolidation today.
- **Sitting expiry.** An entry idle past `pursuit.idle_secs` is gone; a warm
  one carries. A sitting is never written to `activation` — asserted directly,
  because it is the guard most likely to be lost to a convenient refactor.
- **Priming budget.** A hit in `touched` and activated does not move more than
  `prime_lift` places in total, and rank 0 never moves.
- **Abandonment floor.** One abandoned search alone produces no visible gap;
  two clustering ones produce a named group.
- **Coverage.** A capture that answers an open gap closes it and reports it; a
  capture that does not, does not.
- **Harness.** `cargo test --test eval` with `sitting.prime` on and off, on the
  judged-pair set, before that default moves.

## 11. Rollout

In three commits, in this order, because each is independently useful and
independently revertible:

1. **The cycle** (§4). Pure reorganisation: the same functions, in an order,
   with a report. No behaviour the operator asked for changes, and the one
   behaviour they did not ask for — promotion reading an undecayed activation —
   stops.
2. **The queue** (§6). Two kinds, two guards, one coverage check. Visible on
   day one on any base with recorded feedback.
3. **The sitting** (§5), carrying only. Priming lands behind its flag in the
   same commit and is switched on, if at all, by a separate one carrying the
   harness numbers in its message.

## 12. Risks

- **The cycle serialises what ran concurrently.** Six tickers could overlap;
  one cycle cannot. A long consolidation now delays the next replay by its
  duration. Mitigated by the rhythms (§4.2) — a phase that is not due costs a
  timestamp comparison — and bounded by the fact that every phase already caps
  its own work per tick (`REPLAY_LIMIT`, `PRUNE_SCAN_LIMIT`,
  `max_dedupe_per_tick`). If it bites, the answer is to split the cycle in two
  by cost, not to go back to six.
- **Abandonment floods the gap list.** The two guards in §6.2 are a judgement,
  not a measurement, and the right floor is a number nobody has. It ships
  visible and easy to walk back: an abandoned gap is one `WHERE kind != …`
  from being invisible again.
- **The sitting makes results feel unstable.** The same query in two sittings
  ranks differently, which is precisely what priming is for and precisely what
  is disorienting about it. Hence off by default, hence the badge, hence rank 0
  never moving.
- **Coverage marks a gap closed that is not.** A vector hit above
  `weak_below` from a new corpus is a weak claim to have answered a question.
  It is the same claim `answered` already makes, and closing is reversible:
  the gap's source row is untouched, so a covered gap can be reopened by the
  operator without inventing a state for it.
