# Runtime tuning sweeps, and a judge page worth returning to

## Why

The judge page produces the only ground truth this system has about its own
ranking, and today that truth leads nowhere at runtime: the numbers it earns
are spent only by someone who knows to leave the UI for a terminal, export a
corpus, and run an `#[ignore]`d benchmark. This design closes the loop inside
the running server — judgements automatically buy parameter sweeps, sweeps
produce recommendations, and a recommendation is applied with one click — and
redesigns the judge page so the loop is visible and the work of judging feels
worth doing.

The cargo harness (`tests/eval.rs`) is untouched. It remains the instrument
for long-term comparability and for the knobs a runtime sweep cannot reach.

## Part 1 — the tuning subsystem

### The sweep needs no snapshot

The cargo harness freezes the corpus because its numbers must be comparable
across months. A recommendation only compares candidate configurations
against each other, in one run, on one corpus state — baseline included. So
the sweep reads the **live index**, read-only, with the same discipline as
the judge assign search: `mark: false`, `Door::Judge`, never captured. No
snapshot, no `engram_eval` collection, no re-embedding. Each pair's query is
embedded once (query cache) and re-searched per grid candidate; the whole
sweep is seconds of vector reads.

### Trigger

After every verdict that changes the judged count, the judge handlers check:

- `judged >= feedback.tune.min_judgements` (default 50 — this replaces the
  `FIRST_SWEEP_AT` constant, as its own comment anticipates), and
- at least `feedback.tune.resweep_after` (default 10) new judgements since
  the last sweep run.

If both hold, a background sweep job is enqueued. Failures are logged and
never block judging.

### What is swept

The grid is deliberately minimal — the two knobs the evaluation doc names as
the primary movers, and nothing that only matters under data conditions most
corpora do not have:

| Knob | Values |
|---|---|
| `vector.recency_weight` | 0.0, 0.05, 0.1, 0.15, 0.25 |
| per-source cap | 2, 3, 5, none |

Twenty combinations, the current configuration always among them as the
baseline. Adding a knob later is a line in the grid, not a redesign.
Explicitly out of scope: embedding model and templates (they change the
vector geometry and cannot be swept at runtime), priming, pinned boost,
recency half-life, and everything on the ask side.

### Scoring

Pairs are every judged event with an expectation: hits and finds. The
expected artifact is resolved through supersession exactly as the harness
does (bounded to 8 hops). A pair whose artifact no longer exists is skipped
and counted — a background job must not die on housekeeping — and the skip
count is part of the run record. Each candidate configuration gets recall@10,
MRR, and its miss list.

### The recommendation gate

A candidate is recommended only if, against the baseline:

- **at least two pairs are net better** — pairs whose rank improved (into
  the window, or upward within it) minus pairs whose rank worsened is two or
  more — and
- **neither aggregate is worse.**

Ties keep the current values. The two-pair floor is the overfitting brake:
on a fifty-pair corpus a single flipped pair can fake any aggregate delta.
If nothing passes the gate, the run is still recorded and the page says so —
explaining the silence is part of the traceability the feature owes.

### Recording

A new `eval_runs` table holds every sweep: created_at, parameters (JSON),
recall@10, MRR, miss list (JSON, query prefixes only — no artifact text),
pairs used, pairs skipped, whether it was recommended, and `applied_at` when
a recommendation was taken. This table is the provenance that used to live in
commit messages.

### Applying

The scoring parameters (recency weight, per-source cap) move into a
`RankingParams` value in `Core` behind a lock. The live search path reads it
there; the sweep passes explicit candidate values to the same code — one
mechanism serves measurement and hot-swap.

The apply action, from the recommendation banner:

1. rewrites `config.toml` in place via `toml_edit`, preserving comments and
   formatting — the file stays the single source of truth and a restart
   reads the same values;
2. swaps the live `RankingParams`;
3. stamps `applied_at` on the run and flashes the before/after numbers.

## Part 2 — the judge page redesign

Layout: **cockpit on top** (variant A of the mockups). The interaction
feedback: **the quiet ticker** (variant A) — no card transitions, no
milestone accents.

### Header strip

One slim row replaces today's stacked header:

- the progress bar, now labelled by what it actually buys: progress to the
  **next sweep** (`n/50` before the first, `n/10 new` after), with a short
  hint saying what a sweep is;
- recall@10 and MRR, each carrying a one-line plain-language explanation
  (`title` attribute and a hint on first visit) — closing the gap where the
  page shows numbers it never explains;
- a session counter (“14 today”, from judgement timestamps) and the queue
  remainder (“7 waiting”).

The verdict counters (hits · finds · gaps · discarded) and the ask stats
stay, one muted line, with the same explanation treatment.

### The verdict moment

After each verdict, only the existing numbers respond: the MRR figure blinks
once, a small rising `▲ +0.01` appears beside it and fades, and the progress
bar animates its increment. The flash line, its inverted loudness, and the
undo button are unchanged — the diagnosis text is already the page's voice
and the redesign must not drown it. Nothing celebrates a particular verdict;
what animates is progress toward the sweep, never agreement with the ranker.

### The recommendation banner

Between header and card, three states:

- **hidden** below the judgement floor;
- **quiet note** when the last sweep found nothing: “last sweep: 2 days ago,
  no improvement found”;
- **recommendation**: “recency 0.05 instead of 0.15 — MRR 0.54 → 0.61,
  recall unchanged”, with the miss-list diff expandable underneath and an
  apply button. The diff is mandatory UI, not an extra: the evaluation doc's
  rule that aggregates alone prove nothing holds here too.

### History and misses

At the bottom, collapsed as today: the miss list, and a new tuning history —
each applied change with date, parameters, before/after numbers, and the run
that justified it.

## Configuration

```toml
[feedback.tune]
min_judgements = 50   # floor before the first sweep; replaces FIRST_SWEEP_AT
resweep_after = 10    # new judgements between sweeps
```

Tuning is active whenever `learn.enabled` is — no separate switch. The grid
is code, not configuration.

## Error handling

- Sweep job failures: logged, judging unaffected, no banner state change.
- Apply with a stale run id, or a run already applied: refused with a flash,
  nothing written.
- `config.toml` unwritable: the apply fails whole — no hot-swap without the
  file write, so memory and disk never disagree.
- Skipped pairs are visible in the run record; a sweep that skipped
  everything records itself and recommends nothing.

## Testing

- Unit: the recommendation gate (two-pair floor, tie-keeps-current, no
  aggregate regression), grid construction, `toml_edit` round-trip preserving
  comments, `RankingParams` swap visible to the search path.
- Integration (fake embedder + `MemoryVectors`, deterministic): judge to the
  threshold → sweep runs → run recorded; a corpus arranged so a candidate
  wins → banner shows the recommendation with the miss diff → apply →
  `config.toml` changed, live params changed, run stamped; a corpus where
  nothing wins → “no improvement found”.
- UI: header explains its numbers; the ticker animates via CSS classes the
  templates set (no assertion on animation, assertion on the hooks); the
  assign-search non-capture discipline extended to sweep searches.

## Out of scope

The ask harness and its knobs; embedding model/template sweeps; priming and
pinned knobs; any change to `tests/eval.rs` or `--export-eval`; streaks,
levels, badges, or any reward tied to verdict outcomes.
