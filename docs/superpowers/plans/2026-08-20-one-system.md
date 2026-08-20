# One System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the three seams between mechanisms that already exist — give the job queue a priority and an account so the tickers become units that reschedule themselves; give a sitting a live existence instead of only a reconstructed one; and make the four ways the base can fail to answer end in one list.

**Architecture:** Nothing new is built. One column on `jobs` (`class`), one table (`sweep_runs`), two more `GapKind`s, and one in-memory map on `Core`. Five ticker tasks are deleted and their periods become each unit's own `run_after`. `spawn_repair_ticker` stays outside the schedule and gains two duties: ageing background rows into the foreground, and re-arming a periodic unit that has gone missing.

**Tech Stack:** Rust, sqlx/SQLite, tokio, axum, Askama, htmx. No new dependency, no new subsystem, and no model call that is not already made.

**Spec:** `docs/superpowers/specs/2026-08-20-one-system-design.md`

---

## Global Constraints

- **The claim query stays one covering index walk.** `EXPLAIN QUERY PLAN` on `claim_job` must name the index and show no temp B-tree. Priority sorts *before* `attempts`; ageing is an `UPDATE`, never a term in the claim. Spec §3, §4.4.
- **`seq` keeps doing its job.** Within one class the order stays `attempts, seq, id`. Assert it directly — `seq`'s anti-starvation property is the easiest thing to lose here.
- **Repair does not join the schedule.** `a_crashed_capture_is_repaired_with_the_sweep_switched_off` is the pinned regression and must pass at the end of every task.
- **Lazy decay stays lazy.** No unit added here writes a decayed value.
- **No new model call.** Not for `Unmatched`, not for coverage, not for the sitting. The only inference in this area is the existing gap-cluster naming call, untouched.
- **The state string is `'running'`, not `'claimed'`.** The spec says "claimed" in §4.1 prose; the tree says `state = 'running'` with a `claimed_at` timestamp. Follow the tree.
- **Every existing test keeps passing.** Where a signature moves, update the assertion; never delete it.
- **Commit after every task**, conventional-commit subject in the repo's voice. Three commits' worth of *behaviour* (§11), but tasks inside a phase may commit separately.

---

## Decisions the spec leaves open

Four places where the spec's data-model summary does not survive contact with the tree. Each has a recommendation; settle them before Task 1, because three of them change the schema.

### D1 — There is no `ALTER TABLE` path in this codebase

`Store::migrate` applies `schema.sql` whole, every object `IF NOT EXISTS`, and then *checks* that every column the file declares exists — failing at boot, by name, when it does not. The doctrine is written into the file's own header: "Changing a column means changing it here and recreating the database."

So spec §8's "one `ALTER TABLE`" is not how this base migrates. Two options:

- **(recommended) Follow the existing doctrine.** Declare `class` in `schema.sql`. An existing base — including the live `engram.db` — fails at boot with `this database is older than the schema: jobs.class missing. Recreate it, or add the columns by hand.` The operator runs one `ALTER TABLE jobs ADD COLUMN class INTEGER NOT NULL DEFAULT 0;` and restarts. Nothing new is invented, and the failure is loud and already-explained.
- Add a narrow, idempotent additive-migration step to `migrate()`, run before the column check. This is precedent-setting: the next column will use it too, and the "one statement of shape" doctrine ends there.

**The backfill is not optional either way**: rows written before the column exists default to `0` (foreground), which §4.3 chose as the safe direction. The one `UPDATE` setting the sweeps to `1` runs in `migrate()` regardless, guarded so it is a no-op on a base that has it.

### D2 — The index cannot be swapped by `IF NOT EXISTS`

`CREATE INDEX IF NOT EXISTS idx_jobs_claim2` on a base that already has `idx_jobs_claim2` with the old columns is a silent no-op — the claim would then sort in a temp B-tree on exactly the installs this is meant to speed up. **Give the new index a new name** (`idx_jobs_claim3`) and add `DROP INDEX IF EXISTS idx_jobs_claim2;` beside it. `DROP ... IF EXISTS` is idempotent, so `schema.sql` stays applicable-on-every-connect.

### D3 — `GapKind::Pursuit` has no stored query vector

`pursuits.queries` is a JSON array of query *strings*. A gap is `(kind, id, text, query_vec)` — `open_gaps` reads `query_vec` straight off the source row, and the clustering and the calibration both need it. A pursuit row has no vector and no reference to the events it was built from, so there is nothing to read.

Embedding the queries at read time is a call this spec forbids. **Recommendation: carry the vector forward at close.** The pursuit sweep already holds the `search_events` rows it clustered; write the leading event's `query_vec`, `vec_dim` and `embed_model` onto the pursuit row when it closes (three columns, all nullable, all `NULL` on existing rows). A pursuit closed before this lands has no vector and is therefore not a gap — correct, not a bug: an uncomparable vector is exactly what `open_gaps` already excludes with `vec_dim > 0`.

This is a fourth schema change beyond §8. Say so in the commit message rather than pretending §8 was complete.

### D4 — "Closed silently, source row untouched" needs somewhere to write

§6.3 says a covered gap is closed, the capture page says which, and the operator can reopen it — while "the source row is untouched". Those cannot both hold with no third place to record the closure.

**Recommendation: one small table.**

```sql
-- A gap the base has since answered: the capture that covered it, and how
-- well. Kept off the source row on purpose — nothing an automatic score
-- decides should overwrite what a person judged, and deleting the row here
-- reopens the gap with the judgement intact.
CREATE TABLE IF NOT EXISTS gap_coverage (
  kind        TEXT NOT NULL,
  gap_id      TEXT NOT NULL,
  corpus_id   TEXT NOT NULL REFERENCES corpora(id) ON DELETE CASCADE,
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  score       REAL NOT NULL,
  covered_at  INTEGER NOT NULL,
  PRIMARY KEY (kind, gap_id)
);
```

`ON DELETE CASCADE` on both references is the reversibility the spec asks for, for free: delete the capture that closed a gap and the gap comes back.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `src/core/sitting.rs` | The live sitting: the map, `CARRY`, expiry, the read the doors take |
| `src/store/sweeps.rs` | `sweep_runs`: record one run, read the last day, read the history, trim |
| `src/store/coverage.rs` | `gap_coverage`: close, reopen, read what closed a gap (may live in `store/gaps.rs` instead) |

**Modified:**

| File | Change |
|---|---|
| `src/store/schema.sql` | `jobs.class`; `idx_jobs_claim3` + drop of `claim2`; `sweep_runs`; `gap_coverage`; three nullable columns on `pursuits` (D3) |
| `src/store/mod.rs` | The class backfill in `migrate()` |
| `src/store/jobs.rs` | `Stage::{Retention, ArmDedupe}`, `Stage::class()`, `class` written on insert, claim order, `age_background()`, `arm_missing_periodic()` |
| `src/core/background.rs` | Five tickers deleted; `repair_once` gains ageing and re-arming; `periodic_units()` |
| `src/jobs/mod.rs` | Dispatch for the two new stages; the re-arm-on-completion step; `sweep_runs` written around every periodic unit |
| `src/core/ingest.rs` | Queue-label arms for the two new stages (`~1045`) |
| `src/main.rs` | Five `spawn_*_ticker` calls and their join handles removed (`266–273`) |
| `src/config.rs` | `ScheduleConfig`, `SittingConfig`, both on `Config` |
| `src/core/mod.rs` | The sitting map on `Core`; `schedule`/`sitting` settings carried through |
| `src/core/search.rs` | Sitting writes on `mark_artifact_seen` / `record_interaction`; `Unmatched` recording; sitting priming behind its flag |
| `src/store/gaps.rs` | `GapKind::{Unmatched, Pursuit}`, their two source queries, dismissal for both |
| `src/jobs/gaps.rs` | Coverage check on a corpus reaching `ready` |
| `src/jobs/pursuit.rs` | Carry the query vector onto the pursuit row at close (D3) |
| `src/web/ui.rs` | Ops: the last day and the history, minus the pursuits table; capture: kind badges, coverage line; sitting rail on search and ask |
| `src/web/templates/ops.html`, `_gaps.html`, `capture.html`, `search.html`, `ask.html` | The surface, §7 |
| `config.example.toml`, `ROADMAP.md` | Two keys; the seams marked closed |

---

# Phase 1 — The scheduler (spec §4)

One column, one index, one table, five tickers deleted. No behaviour the operator asked for changes.

## Task 1: The schema, the column and the backfill

**Files:** `src/store/schema.sql`, `src/store/mod.rs`, `src/store/jobs.rs`

**Interfaces:** Produces a `jobs.class` column every later task reads. Consumes nothing.

- [ ] **Step 1: Settle D1 and D2** — write the chosen answer into this plan before touching a file.

- [ ] **Step 2: Declare the column, the index and the table**

In `schema.sql`, inside `CREATE TABLE ... jobs`:

```sql
  -- Is someone waiting on this? 0 = foreground (the capture pipeline the
  -- operator is watching move raw → ready), 1 = background (work nobody is
  -- standing in front of). One distinction and not a scale: a priority the
  -- operator can set wrong presents as "the capture is hanging" with nothing
  -- anywhere saying why. Default 0 because a row written before this column
  -- existed is foreground, which is the safe direction to be wrong in.
  class       INTEGER NOT NULL DEFAULT 0,
```

Then, beside the existing index lines:

```sql
-- Superseded by idx_jobs_claim3 below: same walk, with class in front of
-- attempts. Dropped by name because CREATE INDEX IF NOT EXISTS would leave
-- the old columns in place on every existing base — silently, and on exactly
-- the installs the new order exists to serve.
DROP INDEX IF EXISTS idx_jobs_claim2;
CREATE INDEX IF NOT EXISTS idx_jobs_claim3 ON jobs(state, class, attempts, seq, id, run_after);
```

Keep the existing comment explaining why `run_after` is last; extend it to say why `class` is first.

Add `sweep_runs` exactly as §4.5 gives it.

- [ ] **Step 3: Backfill in `migrate()`**

After the schema applies and before the column check, one guarded statement setting the periodic and background stages to `1`, listing the stages by name. Idempotent by construction — it only ever moves rows that are still `0` *and* whose stage is background — so applying the schema twice still changes nothing, which `applying_the_schema_twice_changes_nothing` asserts.

- [ ] **Step 4: `Stage::class()`**

One `match` in `src/store/jobs.rs`, exhaustive, no wildcard arm — a stage added later must be made to choose:

```rust
/// Is someone waiting on this?
///
/// Foreground is the capture pipeline: the operator pasted something and is
/// watching it move. Background is every sweep whose result nobody is
/// standing in front of. Exhaustive on purpose: a stage added later has to
/// answer this question rather than inherit an answer.
pub fn class(self) -> i64 {
    match self {
        Stage::Synthesize | Stage::Enrich | Stage::SegmentWindow
        | Stage::Title | Stage::Embed | Stage::Describe | Stage::Extract => 0,
        Stage::Consolidate | Stage::Dedupe | Stage::Relate | Stage::Associate
        | Stage::LinkJudge | Stage::Pursuit | Stage::Generate => 1,
    }
}
```

`Enrich` is not in the spec's table; it shares `synthesize::plan` with `Synthesize` and is foreground for the same reason.

- [ ] **Step 5: Write it on insert, read it on claim**

`upsert_job` sets `class = ?` from `Stage::class()`. `claim_job` becomes `ORDER BY class, attempts, seq, id`. Nothing else in the query moves.

- [ ] **Step 6: Test**

```bash
cargo test --lib store::jobs 2>&1 | tail -20
```

New tests:
- a background unit armed first is claimed after a foreground unit armed second;
- within one class, `attempts, seq, id` order is unchanged (assert directly against a batch with mixed `seq`);
- `EXPLAIN QUERY PLAN` on the claim's inner `SELECT` names `idx_jobs_claim3` and contains no `TEMP B-TREE`.

## Task 2: Ageing

**Files:** `src/store/jobs.rs`, `src/core/background.rs`, `src/config.rs`, `config.example.toml`

- [ ] **Step 1: `[schedule] age_after_mins = 60`**

New `ScheduleConfig` next to `PursuitConfig` in `config.rs`, `#[serde(default)]` on `Config`, carried onto `Core` the way `pursuit` is. Document in `config.example.toml` that the default is a guess and that `sweep_runs` on Ops is how the guess gets checked.

- [ ] **Step 2: `Store::age_background(older_than: i64) -> Result<u64>`**

```sql
UPDATE jobs SET class = 0
 WHERE state = 'pending' AND class = 1 AND created_at < ?;
```

The doc comment carries §4.4's reason: computing age at claim time would put an inequality into the ordering and cost the covering index. A unit that has aged stays aged; it has already waited.

- [ ] **Step 3: Call it from `repair_once`**, warn-and-continue like every other step there.

- [ ] **Step 4: Test** — a background unit older than `age_after_mins` is claimed ahead of a fresh foreground one, and re-reading the row shows `class = 0` (durable, not computed).

## Task 3: Two stages for the two tickers that did their work inline

**Files:** `src/store/jobs.rs`, `src/jobs/mod.rs`, `src/core/ingest.rs`

The consolidation, association and pursuit tickers *enqueue* a unit; the retention and dedupe tickers **do their work in the ticker body**. There is no stage to reschedule, so two are added. This is a deviation from §4.3's table — record it in the commit message.

- [ ] **Step 1: `Stage::Retention`** — expire feedback past `retain_days`, then run `jobs::gaps::sweep`, in that order, which is the order the retention ticker already uses and the ordering §1 credits it for. Class `1`.

- [ ] **Step 2: `Stage::ArmDedupe`** — calls `jobs::consolidate::arm_dedupe`. Class `1`. Named for what it does: it arms `Dedupe` units, it is not one.

- [ ] **Step 3: Dispatch arms in `run_claimed`**, `Stage::parse`/`as_str`, and the queue-label match at `src/core/ingest.rs:1045`.

- [ ] **Step 4: Test** — each stage, claimed and run, does what its ticker did. Port the two ticker tests in `background.rs` rather than deleting them.

## Task 4: The tickers become units that reschedule themselves

**Files:** `src/core/background.rs`, `src/jobs/mod.rs`, `src/store/jobs.rs`, `src/main.rs`

- [ ] **Step 1: `periodic_units(core) -> Vec<(Stage, &'static str, Duration)>`**

One list, in `background.rs`, replacing five ticker preambles. It is where every gate that used to be an early `return` now lives — and getting this wrong re-creates the exact bug `spawn_repair_ticker`'s comment records, so each gate keeps its original condition verbatim:

| Stage | Gate today | Period today |
|---|---|---|
| `Consolidate` | `consolidate.enabled && synthesizes()` | `consolidate.interval_hours` |
| `ArmDedupe` | `consolidate.enabled && max_dedupe_per_tick > 0 && synthesizes()` | `consolidate.dedupe_interval_mins` |
| `Retention` | `feedback.retain_days > 0 \|\| feedback.enabled` | `feedback.sweep_hours` |
| `Associate` | `associating()` | `associate.interval_mins` |
| `Pursuit` | `pursuit.enabled && associating()` | `(pursuit.idle_secs / 2).max(60)` |

Keep the existing `.max(1)` and `saturating_mul` guards: an operator-typed interval that wraps turns a long period into a hammering one.

- [ ] **Step 2: Re-arm on completion**

In `run_claimed`, beside the existing `embed::rearm_if_more` step and for the same stated reason — *after* `complete_job`, never before, because the queue is keyed by `(stage, target)`:

```rust
// A periodic unit re-arms itself one interval out. `run_after` is the
// cursor recording when it last ran, and it is already indexed, so no
// meta key is needed and no ticker has to hold the clock.
```

Re-arm on the **failure** paths too — §4.6. One failure ending a sweep forever is the one way this design could quietly stop the memory from learning; a test pins it.

- [ ] **Step 3: `arm_missing_periodic`**

On the repair tick: for each entry in `periodic_units`, if no row exists for `(stage, target)` in any state, arm it. This is what recovers a unit that died between claim and re-arm — the failure mode a ticker does not have, and the reason repair stays outside the schedule.

- [ ] **Step 4: Delete the five tickers and their call sites**

`spawn_consolidation_ticker`, `spawn_retention_ticker`, `spawn_dedupe_ticker`, `spawn_pursuit_ticker`, `spawn_associate_ticker` go, along with lines 266–273 of `src/main.rs` and their join handles in the shutdown path. `spawn_repair_ticker` stays untouched in its gating and its cadence.

Boot still arms everything immediately: the repair ticker's first tick fires at once, and Step 3 arms whatever is missing — which on a fresh boot is all of them. That is what preserves "the first tick fires immediately, so a restart picks the work up rather than waiting a day".

- [ ] **Step 5: Ordering — replay before pursue**

`associate::run`, on completion, arms `Pursuit` instead of `Pursuit` keeping a period of its own. A sitting is then scored against links this run folded in. Keep `Pursuit` in `periodic_units` as its own floor anyway: with `associating()` on but the association sweep failing, pursuits must not stop entirely.

- [ ] **Step 6: Test**

- A completed sweep leaves exactly one pending copy of itself with `run_after` one interval out.
- A **failed** sweep re-arms too.
- A periodic unit deleted outright is re-armed by `repair_once`.
- `a_crashed_capture_is_repaired_with_the_sweep_switched_off` still passes.
- `Dedupe` runs only after the sweep that arms it; `Pursuit` only after `Associate`.

## Task 5: The account

**Files:** `src/store/sweeps.rs` (new), `src/jobs/mod.rs`, `src/store/feedback.rs`

- [ ] **Step 1: `record_run(stage, started_at, ended_at, outcome, detail)`**, `detail` being the counts the sweep functions already return — links strengthened and pruned, pairs judged, merges written, clusters named, rows expired. No new counting.
- [ ] **Step 2: Wrap the periodic dispatch arms** in `run_claimed` so both the `Ok` and the `Err` paths write a row. A sweep that fails is exactly the run the operator needs to see.
- [ ] **Step 3: `last_day()` and `history(limit)`** — the summary block and the list under it. There is no "night" and nothing invents a cycle identity; §4.5 is quotable in the doc comment.
- [ ] **Step 4: Trim to 2000 runs** on the retention unit. Housekeeping about housekeeping does not get a policy key.
- [ ] **Step 5: Test** — a completed sweep writes exactly one row with its counts; a failed one writes `outcome = 'failed'`; the trim keeps the newest 2000.

## Task 6: Ops shows the last day and the history

**Files:** `src/web/ui.rs`, `src/web/templates/ops.html`

- [ ] **Step 1:** The summary block — *412 links strengthened, 8 forgotten, 2 merges, 1 gap named* — over `last_day()`.
- [ ] **Step 2:** The history beneath it, which is the thing a single overwritten summary could never give: whether this started yesterday or has been going wrong for a week.
- [ ] **Step 3:** Test — a base with runs renders both; a base with none renders neither, and no empty table.

**Commit 1 ends here.** Subject in the repo's voice, e.g. `feat(queue): the tickers become units that reschedule themselves`.

---

# Phase 2 — One queue for what is missing (spec §6)

Visible on day one on any base with recorded feedback.

## Task 7: `GapKind::Unmatched`

**Files:** `src/store/gaps.rs`, `src/core/mod.rs`

The similarity is not on `search_events`. It is `search_candidates.similarity` (nullable, `PRIMARY KEY (event_id, rank)`), so the source query is an aggregate, not a column read:

```sql
SELECT e.id, e.query AS text, e.query_vec, e.judged_at
  FROM search_events e
 WHERE e.dismissed_at IS NULL AND e.embed_model = ? AND e.vec_dim > 0
   AND (e.verdict IS NULL OR e.verdict <> 'gap')
   AND (SELECT MAX(c.similarity) FROM search_candidates c
         WHERE c.event_id = e.id AND c.similarity IS NOT NULL) < ?
 ORDER BY e.judged_at DESC, e.id DESC LIMIT ?
```

- [ ] **Step 1:** Add the variant, its `as_str`/`parse` string (`"unmatched"`), and a third `macro_rules!` beside `ask_gaps_sql!` and `search_gaps_sql!` — the macro exists so the sweep's projection and the page's cannot drift, and a third source drifting would be the same bug.
- [ ] **Step 2:** Bind `weak_below` from config. Exclude `verdict = 'gap'` so a judged search is not both a `Search` gap and an `Unmatched` one.
- [ ] **Step 3:** `MAX(...)` over zero rows is `NULL` and `NULL < ?` is not true, so a search that recorded no candidates is not a gap. That is the right answer — nothing came close is a claim about what was measured — and it needs saying in a comment, because it looks like an oversight.
- [ ] **Step 4:** Per-kind cap and capped-detection in `open_gaps`, exactly as the two existing kinds get, plus the same treatment in `open_gap_refs` and `calibration_vecs`. `MAX_OPEN_GAPS` is per kind, so the third kind gets its own count.
- [ ] **Step 5:** `dismiss_gap` for `Unmatched` writes `search_events.dismissed_at` — the same column `Search` uses, which is correct: it is the same row, dismissed for the same reason.
- [ ] **Step 6: Test** — a search whose best similarity is under `weak_below` becomes a gap; one with a single hit above it does not; one already judged `gap` appears once, not twice; dismissal sticks. Run it over the existing log shape, retroactively, since that is the claim §6.2 makes for it.

## Task 8: `GapKind::Pursuit`

**Files:** `src/jobs/pursuit.rs`, `src/store/pursuits.rs`, `src/store/gaps.rs`, `src/store/schema.sql`

- [ ] **Step 1: Settle D3.** Add `query_vec BLOB`, `vec_dim INTEGER NOT NULL DEFAULT 0`, `embed_model TEXT` to `pursuits`, all nullable/defaulted so existing rows are valid.
- [ ] **Step 2:** Write them when a pursuit closes, from the leading clustered event the sweep already holds.
- [ ] **Step 3:** Source query — `state = 'unsatisfied' AND vec_dim > 0 AND embed_model = ?`, text from the clustered `queries` JSON (first entry, or joined — pick one and say why in the comment; the label prompt keeps the first twelve members, so the text wants to be the query, not a summary).
- [ ] **Step 4:** Dismissal writes `pursuits.state = 'dismissed'`, which already exists in that column's enumeration.
- [ ] **Step 5: Test** — a pursuit closed `unsatisfied` after this lands is a gap; one closed before it (no vector) is not; dismissal moves the state.

## Task 9: A capture covers a gap

**Files:** `src/jobs/gaps.rs`, `src/store/coverage.rs` (or `store/gaps.rs`), `src/store/schema.sql`

- [ ] **Step 1: Settle D4** and add `gap_coverage`.
- [ ] **Step 2:** When a corpus reaches `ready`, on the background handle: one filtered vector query per open gap, **against that corpus's new artifacts only**. No model call. Bounded by the open-gap cap that already exists.
- [ ] **Step 3:** A gap whose best new hit reaches `weak_below` gets a `gap_coverage` row. Silent and reversible; the source row is untouched; nothing is deleted on a score.
- [ ] **Step 4:** `open_gaps`/`open_gap_refs` exclude anything with a coverage row — one `NOT EXISTS`, applied in the shared macro so all four kinds get it at once.
- [ ] **Step 5: Test** — a capture answering an open gap closes it and says which; one that does not, does not; deleting the covering capture reopens the gap (the cascade); the check makes no model call (assert against a `Core` with no synthesizer).

## Task 10: One list on the capture page

**Files:** `src/web/ui.rs`, `src/web/templates/_gaps.html`, `capture.html`, `ops.html`

- [ ] **Step 1:** Kind badges — *judged*, *asked*, *nothing near*, *pursued*.
- [ ] **Step 2:** The coverage line on capture: which gaps this capture closed.
- [ ] **Step 3:** Ops loses its pursuits table and links into this list. `the_pursuit_section_is_not_there_when_pursuits_are_off` (`ui.rs:8096`) moves with it rather than being deleted.
- [ ] **Step 4: Test** — all four badges render; the gaps block is still absent with feedback off (`the_capture_page_shows_no_gaps_block_when_feedback_is_off`).

**Commit 2 ends here.** e.g. `feat(gaps): four ways to say nothing was there, one list`.

---

# Phase 3 — The sitting (spec §5)

Carrying ships on. Priming ships off, behind `sitting.prime`, and is switched on — if ever — by a separate commit carrying the harness numbers in its message.

## Task 11: The sitting itself

**Files:** `src/core/sitting.rs` (new), `src/core/mod.rs`, `src/config.rs`

- [ ] **Step 1:** `Sitting { touched: VecDeque<Touched>, queries: VecDeque<String>, last_at: i64 }`, `CARRY = 20`, a `Mutex<HashMap<String, Sitting>>` on `Core` keyed by `sessions.id`.
- [ ] **Step 2:** Expiry at `pursuit.idle_secs` — the same number the sweep already uses, so the live definition and the reconstructed one agree by construction. Dropped lazily on read; no sweep, no table, no migration.
- [ ] **Step 3:** In memory only. The doc comment carries the price plainly: a deploy mid-afternoon costs the operator their carried context, and a working memory that survives a restart is a long-term memory, which engram has one of.
- [ ] **Step 4:** Writes on the paths that already record engagement — `mark_artifact_seen`, `record_interaction`, an ask's citations. A lock and a push; no vector call, no store write.
- [ ] **Step 5: Test** — expires at `idle_secs`; caps at `CARRY`; **never writes activation** (assert directly — this is the guard most likely to be lost to a refactor); web door only, with no path from `/mcp` or the API reaching it.

## Task 12: Carrying

**Files:** `src/web/ui.rs`, `src/web/templates/search.html`, `ask.html`, `_ask_carried.html`

Nothing here changes an order.

- [ ] **Step 1:** Search → ask: the ask box prefilled with the sitting's last query when it is empty and the sitting is warm.
- [ ] **Step 2:** Ask → capture: the keep-this-answer link gains the question it answered, as the note. The operator still decides, and the trace still records that a model wrote the text — that line does not move.
- [ ] **Step 3:** The rail of what this sitting has touched, on search and ask. Six at most, each a link, **absent on a cold sitting** — not an empty box.
- [ ] **Step 4: Test** — warm sitting prefills, cold one does not; a non-empty ask box is never overwritten; the rail is absent, not empty, when there is nothing to show.

## Task 13: Priming, off

**Files:** `src/core/search.rs`, `src/config.rs`, `config.example.toml`

- [ ] **Step 1:** `[sitting] prime = false`.
- [ ] **Step 2:** A hit in `touched` is primed by the same rank-based bounded rule `search::prime` already applies, **inside the same budget**: extend the existing function rather than running it twice. A hit cannot be lifted `prime_lift` places for activation and again for the sitting.
- [ ] **Step 3:** Index 0 stays untouchable — the existing walk already starts at index 2 and floors the target at 1; do not weaken it.
- [ ] **Step 4:** A hit the sitting lifted says so, in the same place a hit lifted by activation already says so.
- [ ] **Step 5: Test** — with `prime` on, a hit in both `touched` and activation moves at most `prime_lift` places *in total*, and rank 0 never moves; with `prime` off, ranking is byte-identical to today's.

## Task 14: The harness

- [ ] **Step 1:** `cargo test --test eval` with `sitting.prime = false`, recorded.
- [ ] **Step 2:** The same with `true`, recorded.
- [ ] **Step 3:** The default moves only in a separate commit whose message carries both numbers. If it does not move, that is a result too and belongs in `ROADMAP.md`.

**Commit 3 ends here.** e.g. `feat(sitting): what this sitting has touched, carried between the doors`.

---

## Final verification

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test 2>&1 | tail -30
```

The four that matter most, by name:

| Test | What it pins |
|---|---|
| `a_crashed_capture_is_repaired_with_the_sweep_switched_off` | Repair stayed outside the schedule |
| the claim's `EXPLAIN QUERY PLAN` | The covering index survived priority and ageing |
| within-class `attempts, seq, id` | A large ingest still cannot starve a small one |
| the sitting never writes activation | Working memory stayed a read |

Then `ROADMAP.md`: three seams marked closed, and the four items §2 lists as unchanged — engagement at every door, access reconsolidation, error-driven re-synthesis, the corpus map — left where they are, now easier.
