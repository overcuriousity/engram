# One system — scheduling, the sitting, and one queue for what is missing — Design

Date: 2026-08-20
Status: draft
Adds `src/core/sitting.rs`; touches `src/store/jobs.rs`, `src/store/schema.sql`,
`src/core/background.rs`, `src/core/search.rs`, `src/store/gaps.rs`,
`src/web/ui.rs`, `ROADMAP.md`.
Adds no model call anywhere, one column, one table, and no new subsystem.
See §3 for what it is not allowed to break.

## 1. Why

The mechanisms are built. What is missing between them is not a mechanism.

**The background work has no order and no account.** Six tickers run on six
intervals, each started separately, each gated separately, none aware of the
others: `spawn_consolidation_ticker` (`consolidate.interval_hours`),
`spawn_repair_ticker` (its own period, behind no setting at all),
`spawn_retention_ticker` (`feedback.sweep_hours`, which also carries the gap
sweep), `spawn_dedupe_ticker` (`consolidate.dedupe_interval_mins`),
`spawn_pursuit_ticker` (`pursuit.idle_secs / 2`) and `spawn_associate_ticker`
(`associate.interval_mins`).

Where their work is directional, nothing expresses it: `Relate` arms the pairs
that `Dedupe` and the consolidation sweep both read, the three sit on two
tickers whose periods are configured in different units, and which runs first
on a given night is whichever interval elapsed first. Inside one ticker the
order is right — `associate::run` replays before it prunes, the retention
ticker expires before it regroups the gaps — which is exactly the point: the
ordering that was easy to see got written down, and the ordering that crosses a
ticker boundary did not.

Nor does anything decide what matters. A worker claims `ORDER BY attempts, seq,
id`: least-tried, then oldest. So a capture the operator is watching queues
behind a consolidation sweep armed a minute earlier, and waits for it. There is
no way to say that one of those has somebody in front of it and the other does
not.

And nothing reports. Each ticker logs its own line at `info`. There is no
answer to *what did the memory do while I was away*, which is the question a
system that describes itself as sleeping has to be able to answer.

**The sitting exists, but only afterwards.** `jobs/pursuit.rs` reconstructs one
from idle gaps in the search log: searches closer together than
`pursuit.idle_secs` are one sitting. So the base can name what you were working
on last Tuesday and not what you are working on now.
`Core::record_interaction` writes `interaction_events` on every open, and
nothing reads that table until the sweep runs, `idle_secs` after you stopped.

The cost is paid at every door: search, ask, capture and judge are four pages
with nothing carried between them. A query typed on the rail is retyped into
ask. An answer worth keeping prefills the capture box and loses the question it
answered.

**Four ways to say the base did not answer, ending in four places.** A search
judged `gap` becomes a `GapKind::Search`. An ask verdict of *nothing here*
becomes a `GapKind::Ask` by a second path. A pursuit that closes `unsatisfied`
lands on Ops and is never a gap. And a search where **nothing came close** —
every hit under `vector.weak_below`, the rail saying *nothing matches closely*
in as many words — is recorded, read by nothing but the link replay and the
export, and reaches the operator never.

## 2. Goal

Three seams. No new subsystem, and no model call that is not already made.

1. **Scheduling.** The job queue becomes what it is already three-quarters of:
   a scheduler with states, priority, ageing and dependencies. One column, and
   the tickers become units that reschedule themselves.
2. **The sitting.** A live, session-scoped working memory: what this sitting
   has touched, carried between the doors, gone when it goes.
3. **One queue.** The four exits above become four `GapKind`s in the list that
   already exists, closable three ways.

### Non-goals

- **Time slices.** A unit runs to completion. Every sweep already caps its own
  work per run (`REPLAY_LIMIT`, `PRUNE_SCAN_LIMIT`, `max_dedupe_per_tick`), so
  "to completion" is bounded already; yielding mid-sweep would mean every sweep
  carrying its own resume point, and the queue already has one of those in
  `segments`.
- **A priority scale.** One distinction, not a number. §4.3.
- **A calendar.** No cron expression, no "sleep at 03:00". Periods, as today.
- **Persisting the sitting.** In memory, gone on restart. §5.1.
- **The sitting at the other doors.** Web session only. §5.4.
- **Engagement at every door**, **access reconsolidation**, **error-driven
  re-synthesis**, **the corpus map.** Unchanged and still on the roadmap. All
  four get easier once this lands, and none of them are here.

## 3. What this must not break

**The claim query stays covering and unsorted.** `idx_jobs_claim2` is
`(state, attempts, seq, id, run_after)` and its comment records why that column
order and not the obvious one: an inequality ends an index's usable ordering,
so putting `run_after` earlier turns every poll into a temp B-tree sort.
Priority goes in front of `attempts`, and ageing is written rather than
computed (§4.4), precisely so the claim stays one index walk that stops at the
first ready row.

**`seq` keeps doing its job.** Claiming orders by `seq` so that every
document's first window runs before any document's second, which is what stops
a large ingest starving a small one. Priority sorts *before* `seq`, never
instead of it: within one class the existing fairness is untouched.

**Repair does not join the schedule.** `spawn_repair_ticker` carries a comment
recording exactly this mistake: its passes used to ride the consolidation
sweep, and `consolidate.enabled = false` returned before their loop, so a
corpus left `segmenting` by a crash was never repaired on an install with
consolidation off. `a_crashed_capture_is_repaired_with_the_sweep_switched_off`
pins it. Repair is what recovers an interrupted schedule; it cannot be
scheduled by the thing it recovers. It stays its own ticker, behind no setting.

**Lazy decay stays lazy.** Activation and link weight both fold decay in at
read and at bump — `decayed(value, stamp, at, half_life)` — and
`bumping_folds_the_decay_in_rather_than_adding_to_a_stale_number` pins it. A
schedule is a tempting place to hang a decay pass, and it would be a
regression: a scheduled pass makes every number stale between passes. No unit
writes a decayed value.

**A default that changes ranking waits for the harness.** The sitting's
carrying changes no order and ships on. Its priming changes order and ships
off (§5.3).

## 4. Scheduling

### 4.1 What is already there

The queue has four of the five things a scheduler needs, and the fifth is one
column:

| Scheduler concept | In the tree today |
|---|---|
| Runnable | `state = 'pending'` and `run_after <= now` |
| Sleeping on a timer | `state = 'pending'` and `run_after > now` |
| Running | `state = 'claimed'`, with `claimed_at` |
| Blocked on a predecessor | not enqueued yet — the predecessor arms it |
| Fair order within a class | `ORDER BY attempts, seq, id` |
| **Priority** | **missing** |

"Blocked" needs no representation because the tree already expresses
dependencies the right way round: a unit that finishes arms its successor.
`Synthesize` arms one `SegmentWindow` per window, `Associate` arms `LinkJudge`,
the pursuit sweep arms `Generate`. A unit that has not been armed cannot be
claimed, which is what blocked means.

### 4.2 The tickers become units that reschedule themselves

Each periodic sweep, on completion, enqueues itself with
`run_after = now + its own interval`. A process that sleeps on a timer and
wakes runnable — no ticker task, no external clock, and no cursor recording
when it last ran, because `run_after` *is* that cursor and it is already
indexed.

Five tickers are deleted: consolidation, retention, dedupe, pursuit, associate.
`spawn_repair_ticker` stays (§3) and gains one duty: arming a periodic unit
that is missing entirely, which is how a schedule recovers from a crash between
claim and re-arm.

`UNIQUE(stage, target_id)` still gives at-most-one of each sweep, for free and
for the same reason as today.

**Ordering falls out of it.** The dependency that crosses a ticker boundary is
`Relate → Dedupe → Consolidate`, and it becomes expressed the way every other
dependency in the tree is — the sweep that produces the pairs arms the unit
that reads them, rather than that unit waking on a period of its own and
finding whatever happens to be there. The one ordering worth adding beyond that
is **replay before pursue**: a sitting scored against links this run folded in,
not against last half-hour's, so the association sweep arms the pursuit sweep
on completion instead of the pursuit sweep keeping a period.

### 4.3 Priority is one distinction

Not a scale and not a knob: **is someone waiting on this?**

| Class | Stages |
|---|---|
| `0` — foreground | `Synthesize`, `SegmentWindow`, `Title`, `Embed`, `Describe`, `Extract` |
| `1` — background | `Consolidate`, `Dedupe`, `Relate`, `Associate`, `LinkJudge`, `Pursuit`, `Generate` |

Foreground is the capture pipeline: the operator pasted something and is
watching it move through `raw → embedding → ready`. Background is everything
whose result nobody is standing in front of.

One column, one constant per stage, with the reason beside it. No configuration
key: a priority the operator can set wrong presents as *the capture is
hanging*, with nothing anywhere saying why, and the cliff constants in
`search.rs` already record this project's answer to knobs of that kind.

```sql
ALTER TABLE jobs ADD COLUMN class INTEGER NOT NULL DEFAULT 0;
```

Claim becomes `ORDER BY class, attempts, seq, id`, and the index becomes
`(state, class, attempts, seq, id, run_after)` — still covering, still stopping
at the first ready row, still no sort.

Default `0` is deliberate: a row written before the migration is foreground,
which is the safe direction to be wrong in. The backfill sets the sweeps to `1`.

### 4.4 Ageing is a write, not a read

A background unit that has waited longer than `schedule.age_after_mins` becomes
foreground. Without it, one long ingest keeps night work off the workers
indefinitely — starvation, and the exact failure a priority scheduler is
expected to have an answer for.

It is an `UPDATE` on the repair ticker, not a term in the claim query:

```sql
UPDATE jobs SET class = 0
 WHERE state = 'pending' AND class = 1 AND created_at < ?;
```

Computing age at claim time would put `created_at` — an inequality — into the
ordering and cost the covering index (§3). Ageing a few rows every repair tick
costs one indexed update and leaves the hot path exactly as fast as it is now.
A unit that has aged stays aged; it has already waited.

### 4.5 The account

One row per completed run of a periodic unit:

```sql
CREATE TABLE IF NOT EXISTS sweep_runs (
  id         INTEGER PRIMARY KEY,
  stage      TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at   INTEGER NOT NULL,
  -- 'ok' | 'failed'
  outcome    TEXT NOT NULL,
  -- What it did, JSON: the counts each sweep already returns.
  detail     TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_sweep_runs_at ON sweep_runs(started_at DESC);
```

The counts are what the functions already return: links strengthened and
pruned, pairs judged, merges written, clusters named, rows expired.

**One consequence to state plainly: there is no longer a "night".** Units that
reschedule themselves on their own periods do not line up into one cycle, and
pretending otherwise would mean inventing a cycle identity to group them by.
What Ops shows instead is *the last day*: one block summarising the runs of the
past 24 hours — *412 links strengthened, 8 forgotten, 2 merges, 1 gap named* —
and under it the history, which is the thing a single overwritten summary could
never give: whether this started yesterday or has been going wrong for a week.

Trimmed by the retention sweep to the last 2000 runs. Housekeeping about
housekeeping does not get a policy.

### 4.6 Failure

Unchanged: `attempts` and `run_after` are the existing backoff, a failed unit
is retried, and nothing takes the process down. A periodic unit that fails
still re-arms itself — otherwise one failure ends that sweep forever, which is
the one way this design could quietly stop the memory from learning.

## 5. The sitting

### 5.1 What it is

A map on `Core`, keyed by web session id (`sessions.id`, which exists):

```rust
pub struct Sitting {
    /// Artifact ids touched, most recent first, capped at CARRY.
    touched: VecDeque<Touched>,
    /// Queries typed in this sitting, most recent first, capped at CARRY.
    queries: VecDeque<String>,
    last_at: i64,
}
```

`CARRY = 20`. An entry idle for `pursuit.idle_secs` is dropped — the same
number that already defines a sitting for the sweep, so the live definition and
the reconstructed one agree by construction.

**In memory only.** No table, no migration, no expiry sweep. It dies with the
process, and a deploy mid-afternoon costs the operator their carried context.
That is the accepted price: a working memory that survives a restart is a
long-term memory, and engram has one of those already.

Writing it costs a lock and a push, on the paths that already record
engagement — `mark_artifact_seen`, `record_interaction`, and an ask's citations.

### 5.2 Carrying (ships on)

Nothing here changes an order.

- **Search → ask.** The ask box arrives prefilled with the sitting's last query
  when it is empty and the sitting is warm.
- **Ask → capture.** The keep-this-answer link already prefills the capture
  box; it gains the question it answered, as the note. The operator still
  decides and the trace still records that a model wrote the text — that line
  does not move, it is only better documented.
- **A rail of what this sitting has touched**, on search and ask. Six at most,
  each a link, absent on a cold sitting.

### 5.3 Priming (ships off)

A hit in `touched` is primed by the same rank-based, bounded rule
`search::prime` already applies to activation, and **inside the same budget**:
a hit cannot be lifted `prime_lift` places for activation and again for the
sitting. Index 0 stays untouchable. A hit the sitting lifted says so.

This is the only part of this design that moves ranking, so `sitting.prime` is
`false` until `cargo test --test eval` has been run with it both ways.

### 5.4 What it does not do

It does not write activation, open a pursuit, or survive the process. It exists
at the web door only: for the API and `/mcp` an access token is not a
conversation, and two agent sessions sharing a token would share a sitting,
which is worse than having none. Giving those doors a real session identity is
a change to the doors, not to this.

The pursuit sweep is unchanged and still reconstructs its own sittings from the
log: the live sitting is a *read* of what is happening, and a sweep that
depended on process memory would lose a day's pursuits to a restart.

## 6. One queue for what is missing

### 6.1 Two more kinds, not a new table

A gap is a reference to the row it came from plus its stored query vector. Both
new sources fit that shape:

```rust
pub enum GapKind { Ask, Search, Unmatched, Pursuit }
```

- **`Unmatched`** — a recorded search whose **best candidate similarity fell
  under `vector.weak_below`**. Nothing came close, and the rail said so at the
  time: *nothing matches closely* is already what the page renders when every
  hit is loose.
- **`Pursuit`** — a pursuit that closed `unsatisfied`. Today a row on Ops and
  nothing else; its clustered queries are already stored and are exactly what a
  gap needs.

`gap_clusters.members` is already `(kind, id)` pairs and needs no change.
Clustering, naming, and the model call that names a group are untouched.

### 6.2 Why distance and not behaviour

The first draft called the fourth source *abandoned*: a search after which
nothing was opened. That was wrong twice over.

It cannot be read. A result list is a rail of titles; not clicking one can mean
the list was useless, or that the titles alone told the operator what they
needed. The two readings are opposite and the system cannot separate them.

And it is not even measurable on every install. An open is recorded only when
pursuits *and* feedback are on — `record_interaction` returns early otherwise —
so with pursuits off, every search in the log looks abandoned.

Distance has neither problem. `weak_below` is an existing, configured and
already-explained line; the similarities are stored on the candidate rows; the
signal needs no interaction data, so it works whatever else is switched on; and
it can be computed over the existing log retroactively. It is also the more
honest claim: not *you gave up*, but *the base held nothing near this*.

The one guard it needs exists already: a typing burst folds into one event by
`feedback.coalesce_secs`, so what is measured is the finished query and not its
first two letters.

### 6.3 Closing

- **A capture covers it.** When a corpus reaches `ready`, the open gaps' stored
  query vectors are searched against that corpus's new artifacts only — one
  filtered vector query per open gap, no model call, on the background handle.
  A gap whose best new hit reaches `weak_below` is closed, and the capture page
  says which. Closed **silently and reversibly**: the source row is untouched,
  so an operator who disagrees reopens it, and nothing is deleted on a score.
- **A pursuit earns it.** Unchanged.
- **The operator dismisses it.** Unchanged, now on one list instead of two.

### 6.4 What the operator sees

One list on the capture page, each row badged with what asked it: *judged*,
*asked*, *nothing near*, *pursued*. Ops loses its separate pursuits table and
links into this list.

## 7. Surface

- **Ops** gains the last day and the history (§4.5), and loses the pursuits
  table to §6.4.
- **Capture** gains kind badges and the coverage line.
- **Search and ask** gain the sitting rail, and with `sitting.prime` on, one
  more reason a hit can say it was lifted.
- Nothing gains a graph or a dashboard. The search box stays the application.

## 8. Data model summary

| Change | Where |
|---|---|
| `jobs.class` | one column, default 0, backfilled |
| `idx_jobs_claim2` → `(state, class, attempts, seq, id, run_after)` | replaced index |
| `sweep_runs` | new table, §4.5 |
| `GapKind::{Unmatched, Pursuit}` | `store/gaps.rs` |
| The sitting | in memory, no schema |

No column is dropped and nothing stored is rewritten. The migration is one
`ALTER TABLE`, one `CREATE TABLE`, one index swap and one backfill `UPDATE`.

## 9. Configuration

```toml
[schedule]
age_after_mins = 60     # a background unit waiting longer becomes foreground

[sitting]
prime = false           # moves ranking; off until the harness says otherwise
```

Two keys. The existing interval keys keep their names and their meanings and
become each unit's own `run_after` step, so an operator who tuned
`associate.interval_mins` finds it still doing what it did.

## 10. Testing

- **Priority.** A background unit armed first is claimed after a foreground
  unit armed second. Within one class, `attempts, seq, id` order is unchanged —
  asserted directly, because `seq`'s anti-starvation property is the easiest
  thing to lose here.
- **The index still covers.** `EXPLAIN QUERY PLAN` on the claim shows the index
  and no temp B-tree. Pinned as a test, since the shape of §4.3 and §4.4 exists
  to keep it.
- **Ageing.** A background unit older than `age_after_mins` is claimed ahead of
  a fresh foreground one, and having aged is durable.
- **Rescheduling.** A completed sweep leaves exactly one pending copy of itself
  with `run_after` one interval out. A *failed* sweep re-arms too — the test
  that stops one failure from ending a sweep forever.
- **Recovery.** A periodic unit deleted outright is re-armed by the repair
  ticker.
- **The pinned regression.** `a_crashed_capture_is_repaired_with_the_sweep_switched_off`
  still passes. This is the test that fails if repair is ever folded in.
- **Dependency.** Dedupe runs only after the sweep that arms it; pursue only
  after replay.
- **Sitting.** Expires at `pursuit.idle_secs`; never writes activation —
  asserted directly, as the guard most likely to be lost to a refactor. With
  `prime` on, a hit in both `touched` and activation moves at most `prime_lift`
  places in total, and rank 0 never moves.
- **Unmatched.** A search whose best similarity is under `weak_below` becomes a
  gap; one with a single hit above it does not. Over the existing log,
  retroactively.
- **Coverage.** A capture answering an open gap closes it and says so; one that
  does not, does not; a closed gap reopens.
- **Harness.** `cargo test --test eval` with `sitting.prime` both ways before
  that default moves.

## 11. Rollout

Three commits, in this order, each independently useful and revertible:

1. **The scheduler** (§4). One column, one index, one table, five tickers
   deleted. No behaviour the operator asked for changes; the behaviour they did
   not ask for — a capture waiting behind a consolidation sweep — stops.
2. **The queue** (§6). Two kinds, one distance rule, one coverage check.
   Visible on day one on any base with recorded feedback.
3. **The sitting** (§5), carrying only. Priming lands behind its flag in the
   same commit and is switched on, if ever, by a separate one carrying the
   harness numbers in its message.

## 12. Risks

- **Priority starves the background.** A base under constant capture could keep
  workers on foreground work indefinitely. `age_after_mins` is the answer and
  its default is a guess; the `sweep_runs` history is how the guess gets
  checked — a sweep whose runs thin out is visible on Ops rather than silent.
- **One class is too few.** Two classes cannot say that embedding matters more
  than titling. Deliberate: the distinction that pays is *someone is waiting*,
  and a second one can be added later without moving the column.
- **Rescheduling loses a sweep.** A unit that dies between claim and re-arm
  never runs again — the failure mode a ticker does not have. Two guards: a
  failed unit re-arms itself (§4.6), and the repair ticker arms any periodic
  unit that is missing entirely (§4.2). The second is the real one, and it is
  why repair stays outside.
- **`Unmatched` floods the list.** A young base is mostly holes and would say
  so loudly. Grouping already needs two members to name a group, and the
  ungrouped list is the operator's to dismiss. If it still floods, the fix is a
  floor on how many the page shows, not on what is recorded.
- **Coverage closes a gap that is not closed.** A hit at `weak_below` from a new
  corpus is a weak claim to have answered a question. It closes silently
  because a base with forty gaps would otherwise turn its own housekeeping into
  a review queue — and it is reversible, the source row is untouched, and
  nothing is deleted.
- **The sitting makes results feel unstable.** The same query in two sittings
  ranks differently, which is what priming is for and what is disorienting
  about it. Hence off by default, hence the badge, hence rank 0 never moving.
