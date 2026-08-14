# Independently schedulable inference — Design

Date: 2026-08-13
Status: draft
Refines the job model in `2026-08-09-engram-design.md`.
Supersedes the corpus-level `Synthesize` job and `fail_pending_segments`.

## 1. Why

On 2026-08-12 a single document — `019ff75a`, "Fundamentals of Secure System
Design" — took from roughly 19:00 to 22:27 to segment. Thirty-three of its
thirty-four windows parsed on the first try. Window 3 needed twelve.

The reason is granularity. One job covered the whole corpus, so one window the
parser could not read consumed the corpus's entire attempt budget, and the pass
stopped at the failure rather than continuing past it. Windows 4 to 33 were not
attempted during any of those twelve rounds; the journal shows them starting at
20:48, seconds after window 3 finally parsed at 20:47:35. Between attempts the
whole document waited out a backoff that doubles toward a six-hour ceiling.

Two smaller faults compounded it. `enqueue_after` re-arms the existing row, so
the failing corpus kept its original row id and reclaimed the front of a
strictly id-ordered queue every time its backoff expired, ahead of everything
captured since. And several of those attempts cost nothing at all: the endpoint
replayed cached completions — identical token counts, identical parser errors at
identical byte offsets, 678 tokens "generated" in 33 ms — so the retry could not
have produced a different answer.

The 2026-08-13 changes (per-window step-over, `ORDER BY attempts, id`,
`.instrument`) treat the symptom at corpus granularity. This design removes the
cause: **a job becomes one inference call.**

## 2. Goal

Every inference call in the application is an independently schedulable unit:
its own attempt budget, its own backoff, paced by one global cooldown, and
yielding to interactive work. No unit can hold up another.

Explicit non-goals, decided during design:

- **Document affinity.** Measured and rejected. See §8.
- **Aborting in-flight calls** to make `ask` faster. Rejected: no wasted work.
- **Parallelism.** One worker, one GPU. The scheduler picks one unit at a time.
- **Making retries differ** (seed/nonce) so a caching endpoint cannot replay a
  failure. Real, and out of scope here; recorded in §10.

## 3. The unit model

One job is one inference call. Everything that is not an inference call becomes
local work that *enqueues* units.

| Stage | Target | Inference calls |
|---|---|---|
| `Synthesize` | corpus | **none** — splits text, upserts segments, enqueues one `SegmentWindow` per pending window |
| `SegmentWindow` | `{corpus_id}#{idx}` | one window |
| `Title` | corpus | one |
| `Embed` | corpus | one batch, re-arms while chunks remain |
| `Consolidate` | collection | **none** — scoring, clustering, auto-supersede are local; enqueues `Judge` units |
| `Judge` | pair id | one pair |

Two of these need justification.

**`Embed` stays targeted at the corpus.** A batch is a set of up to 32 chunks,
not an entity, so there is no row to carry scheduling state. The handler embeds
one batch and re-enqueues itself if chunks remain, with `seq` incremented so
successive batches of a large corpus interleave with other work rather than
monopolising `seq = 0`. Each batch is then independently scheduled, paced and
preemptible without inventing a batch entity.

**`Consolidate` loses its inference.** Today one sweep makes up to
`max_judgements` (20) judge calls in a loop — the second-worst blocker after
synthesis. The sweep now does only local work and arms one `Judge` unit per
pending pair. `max_judgements` stops being a loop bound and becomes a cap on how
many units a sweep arms.

**`Enrich` remains an alias for `Synthesize`.** It has no distinct behaviour
today — `run_one` and `Core::reprocess` both match `Synthesize | Enrich` — and it
stays an alias for the planning stage. It is the operator's "re-segment this
document" verb.

That interaction is worth pinning, because `reprocess` clears the segment rows.
Planning then re-splits and enqueues units for `idx 0..M`. Since `enqueue` is
idempotent per `(stage, target_id)`, units for indices that still exist re-arm
the same rows rather than duplicating. Units for `idx M..N` from a longer
previous split are left pointing at segments that no longer exist, and are
dropped on their next claim by the `NotFound` path.

The paraphrase retry stays inline, so a `SegmentWindow` unit may cost two calls
back to back. It is bounded at two, and splitting it needs somewhere durable to
stash the first reply — for a retry that, against a caching endpoint, currently
returns the identical reply. See §10.

### Lifecycle

```
ingest
  └─ Synthesize (local)  ──enqueues──> SegmentWindow ×N
                                          │
                          each: 1 call, own attempts, own backoff
                                          │
                          last one to settle does the local work
                          (renumber, coverage, status) and enqueues:
                                          │
                              ┌───────────┴───────────┐
                            Title                   Embed
                          (1 call)            (1 batch, re-arms)
```

The settle step is a query plus idempotent enqueues, so the last window
triggering it is race-free under one worker.

## 4. Claiming, pacing and priority

### The gate

One component in `Core`, acquired around **inference calls** rather than around
jobs, so the two local stages never wait for a cooldown they do not earn.

```
InferenceGate
  ├─ background().await   → a permit, returned when no interactive call is in
  │                         flight, AND cooldown has elapsed since the last call
  │                         ended, AND the circuit breaker is closed
  ├─ interactive()        → RAII lease, returns immediately, never waits
  └─ any call completing stamps `last_finished`
```

`core.ask()` holds an `interactive()` lease for its whole duration, since it
makes more than one call. The five inference-making paths — window, title, embed
batch, per-chunk embed, judge — await `background()` immediately before their
call and report the outcome through the permit it returns.

The permit is what makes the cooldown a property of the endpoint rather than of
a worker. `server.workers` defaults to 2, and a gate that only *checks* lets
both read the same unchanged `last_finished` — neither has finished — and put
two generations on the one GPU. Only one background call runs at a time, and the
next waiter measures its cooldown from when that call ended.

`cooldown_secs` moves from `[infer.synthesize]` to a global setting: a minimum
gap between any two background calls, whatever their role. The GPU does not care
which role loaded it. `ask` ignores the gap entirely. The old key is kept on
`SynthesizeRole` purely so that startup can complain about it — unknown keys
parse silently, and an operator's thermal pacing turning itself off across an
upgrade is not something to find out about from a warm room.

An `ask` arriving mid-window still waits out the in-flight call — up to ~73s,
observed. Nothing cancels. What the gate buys is that the worker will not pile a
new unit onto the endpoint in front of it.

### The circuit breaker

Removing the pass-abort (§6) introduces a regression: with the endpoint down,
thirty-four units would each burn a 900-second timeout — 8½ hours of waiting
where the old code stopped after one. The breaker is where that belongs.

Three consecutive transport failures (`Error::Inference`, **not**
`MalformedLlmOutput`) trip it; `background()` then holds everything for a probe
interval of 60 seconds before letting a single call through. Any success resets
it; a failure re-trips it. Three matches the reasoning behind the constant it
replaces: one failure is noise, three in a row is a fact about the endpoint. The
probe interval is short because `mikoshi` returns 502 while loading a model —
the outage this most often is heals in minutes, and the per-unit backoff in §5
already covers longer ones. This is strictly better than today, where
a dead endpoint is handled inside `synthesize::run` and nowhere else — embed and
judge each hammer it independently.

### Claim ordering

Plain `ORDER BY attempts, id` at unit granularity reproduces head-of-line
blocking one level down: a 34-window document takes 34 consecutive row ids, so a
capture behind it waits for all 34. Since affinity is worth nothing (§8),
interleaving is free.

One integer on the job row:

```
seq  = position within the batch of units enqueued together
       (window index; judge-pair index; embed batch number; 0 for singletons)

ORDER BY attempts, seq, id
```

This gives round-robin across documents with no GROUP BY and no rotation cursor:

```
A.w0  B.w0  A.w1  B.w1  A.w2  B.w2 ...
```

Every document's first window runs before any document's second, so a capture
made during a large ingest produces artifacts within a call or two. `attempts`
still leads, preserving the fairness fix: a unit that keeps failing sinks below
fresher work but is never starved, since it runs as soon as nothing fresher is
ready.

`seq` matters for judge units too — a sweep arming twenty would otherwise put all
twenty at `seq = 0` and jump the entire window queue.

### Reconcile

`reconcile.rs` gains one case: for a corpus whose segments exist but whose window
units are missing, arm the per-window units directly. This is what makes a
materialised queue safe — units stay derivable from domain state, so drift
self-heals on the sweep exactly as it does today.

## 5. Attempt budgets and settling

Engram's principle is that there is no terminal state: work stays queued at the
backoff ceiling so a healed endpoint is picked up. That holds. But per-unit
budgets expose a problem the corpus-level design papered over — **if no unit ever
terminates, the corpus never settles**, never embeds, and the artifacts from
thirty-three good windows never become searchable.

`MAX_ATTEMPTS` stays 5, but it now means five attempts *at one window* rather
than five at a whole document. Window 3 would have had its own five before the
corpus settled `partial`, instead of consuming the budget for thirty-four.

So a window is *settled* when it is `done`, **or** `failed` with
`attempts >= MAX_ATTEMPTS`. The job row stays queued at the six-hour ceiling —
the work is never abandoned — but the corpus settles around it and reports
`partial`. If that window later succeeds, the settle re-runs: renumber, recompute
coverage, re-enqueue embed for the new artifacts.

Every `SegmentWindow` handler evaluates the settle condition after writing its
result. Idempotent, so a late recovery re-running it is harmless.

### Per-stage exceptions

Two units give up rather than retry forever, both matching existing behaviour:

- **Title** — a name is cosmetic and the corpus keeps its snippet fallback.
  Today the code logs the failure and moves on. As a unit: exhaust
  `MAX_ATTEMPTS`, then complete the job. Retrying forever spends real calls on
  decoration.
- **Judge** — after `MAX_ATTEMPTS` the job completes and the pair stays pending,
  so a later sweep re-arms it. Same as today's `record_judge_attempt`.

`SegmentWindow` and `Embed` keep never-abandon: both carry knowledge that would
otherwise be lost.

### Stale units

Re-segmenting can change the window count, leaving `corpus#idx` jobs pointing at
windows that no longer exist. `run_one` already drops those —
`Err(Error::NotFound)` completes the job instead of retrying it.

## 6. What this deletes

The granularity change removes code that exists only to ration a shared budget:

- `fail_pending_segments` and the tried/untried partition. Its purpose was
  rationing one attempt budget across windows the model never saw; per-window
  budgets make the question meaningless.
- `REFUSALS_BEFORE_GIVING_UP_ON_THE_PASS` and the
  `MalformedLlmOutput`-versus-endpoint branch inside `synthesize::run` (added
  2026-08-13). Both exist because one job covered thirty-four windows and had to
  choose between stepping over a failure and abandoning the pass. With per-unit
  granularity there is no pass: each window fails on its own, backs off on its
  own, and `attempts`-first ordering sinks it automatically. The endpoint-down
  case moves to the breaker, where it covers all four call sites instead of one.
- `segments.attempts`. `jobs.attempts` becomes the single counter per window.
  Two counters for one thing is what produced the original confusion — the job
  counted 13 attempts while window 3 counted 12.

Roughly six to eight existing tests are deleted or rewritten. The diff will look
larger than the behaviour change is.

## 7. Schema

| Table | Change |
|---|---|
| `jobs` | add `seq INTEGER NOT NULL DEFAULT 0` |
| `segments` | drop `attempts` |

`migrate()` applies `schema.sql` on every connect and cannot alter a table.

**Corrected during implementation.** This section originally claimed `seq` was
"additive (safe)". It is not: `CREATE TABLE IF NOT EXISTS` is a no-op against a
table that already exists, so a column added to `schema.sql` never reaches a
deployed base — and the column check at the end of `migrate()` then refuses to
start it. Deploying as originally specified would have taken engram down.

`migrate()` therefore grows a short list of columns that arrived after their
table was deployed, appended with `ALTER TABLE ... ADD COLUMN` when absent. Two
constraints on that list, both learned the hard way: SQLite can only append, and
only with a default; and the appends must run **before** `schema.sql`, because
the file builds an index over `seq` and an index cannot name a column that does
not exist yet.

Dropping `segments.attempts` remains impossible, so the column is left in place
and unused rather than dropped, and removed whenever the database is next
recreated. Recorded so the next reader does not mistake a dead column for live
state.

Index: `idx_jobs_claim` becomes `(state, attempts, seq, id, run_after)`. As with
the 2026-08-13 change, it is dropped by its old name first — `CREATE INDEX IF NOT
EXISTS` on an existing name is a silent no-op, so an existing deployment would
otherwise keep the narrower index.

### Deployment path

Existing databases hold corpus-level `Synthesize` job rows. On start, `reconcile`
sees corpora with unfinished segments and arms per-window units; the old
corpus-level rows complete on their next claim, because the new `Synthesize`
handler is the planning step and is idempotent. No manual SQL, no data migration
— but §9 test 16 pins it rather than assuming it.

## 8. Measured: affinity does not matter

Design initially assumed that scheduling windows in document order preserved
llama.cpp prefix reuse, and that interleaving would cost throughput. Measurement
says otherwise:

```
per-call prompt: ~3203 tokens
  shared by ALL documents (any order):  653  (20%)   system prompt
  shared only within one document:      200  ( 6%)   document opening
  unique to this call regardless:      2350  (73%)   window body + overlap
```

The 653-token system prompt is identical for every synthesize call in the
application and stays warm under any ordering. Affinity protects only the
200-token document opening — 6% of the prefill — and prefill is the cheap half:
observed calls spend 20–73 seconds generating 400–3000 output tokens, so they are
decode-bound. Affinity is worth a fraction of a percent of wall clock, and it is
the single thing that would complicate the scheduler most. Dropped.

## 9. Testing

Virtual time throughout for the gate: `#[tokio::test(start_paused = true)]`. No
wall-clock sleeps, and assertions become exact rather than "elapsed >= 40ms". The
existing `a_cooldown_paces_the_windows_it_segments` converts to it.

**Gate**

1. `background()` returns immediately when idle and cooldown is zero.
2. `background()` waits exactly the cooldown after a previous call ends.
3. `background()` blocks while an `interactive()` lease is held; proceeds on drop.
4. `interactive()` never waits — asserted during an active cooldown.
5. Breaker trips after N consecutive `Error::Inference`; a success resets it;
   `MalformedLlmOutput` does **not** trip it.

**Unit model**

6. `Synthesize` makes zero inference calls and enqueues one unit per window.
7. Draining a multi-window corpus reaches `ready` with N window calls, one title,
   and the expected embed batches.
8. **Two corpora, one poisoned window**: the healthy corpus reaches `ready` while
   the poisoned unit is still retrying. This is the journal incident as a test;
   it cannot pass today.
9. Claim order interleaves: `A.w0, B.w0, A.w1, B.w1`. Pure store test.
10. A window past `MAX_ATTEMPTS` leaves the corpus `partial` **and the good
    windows' artifacts still embed**.
11. Settle re-runs: the stuck window later succeeds, corpus reaches `ready`, new
    artifacts embed.
12. A `corpus#idx` unit whose window no longer exists is dropped, not retried.
13. Title exhausting its attempts leaves the corpus `ready` and unnamed.
14. `Consolidate` makes zero inference calls and arms one `Judge` unit per pair.
15. A corpus with >32 pending chunks yields multiple embed claims, one batch each.

**Migration**

16. Seed the old shape — corpus-level `Synthesize` row plus segment rows — run
    `reconcile`, assert per-window units appear and the old row retires.

## 10. Open: window size, to be settled by measurement

Window size is arithmetic today: `max_output_tokens / output_ratio` =
`16384 / 8.0` = 2048 input tokens, yielding ~8.1 artifacts per call (measured:
34 windows, 277 artifacts on corpus `019ff75a`).

Every structural failure in the 2026-08-12 journal is a long-output failure —
truncation at the 16384 ceiling, and array corruption at columns 6720, 7822,
11973. Short replies (176, 190, 211 tokens) show none. Much of `prompt.rs`
(`salvage_truncated`, `salvage_objects`, the repair call) exists solely to
survive this. Smaller windows should reduce it, and would also cut the worst-case
`ask` wait from ~73s to ~25s.

Cost is lower than it looks: total output tokens are roughly constant regardless
of how they are split, so what multiplies is prefill — the cheap half. Estimated
+20–40% wall clock, not the 8× a naive reading suggests.

**Before the scheduler is tuned, settle this by measurement, not argument.**
Re-ingest corpus `019ff75a` at `output_ratio` 8.0, 16.0 and 32.0 and compare:

| Signal | Source | Baseline at ratio 8.0 |
|---|---|---|
| coverage | `corpora.coverage` | **0.5539** |
| literal-flag rate | artifacts carrying `FLAG_LITERALS` | **8 of 277 — 2.9%** |
| malformed-output count | journal parse-failure lines per ingest | capture from a clean re-ingest |
| retrieval | `recall@k` / MRR from the eval harness | run once against the current base |

The first two are read off the current database and recorded here. The
malformed-output count cannot be taken from the 2026-08-12 journal: cache
replays duplicate several of its parse-failure lines, so the honest baseline is
a clean re-ingest at ratio 8.0 rather than a count from that excerpt.

Note the flag rate is already low at 2.9%, so it has little room to improve and
is the weakest of the four signals. Coverage and malformed-output count are the
ones that should decide the ratio.

Decision rule: adopt the largest `output_ratio` at which malformed-output count
is near zero and coverage does not regress. If coverage and flag rate do not move
while malformed count drops, output length was a structural problem only — a
useful negative result, and the ratio should then be chosen purely on structural
reliability.

Nothing in §3–§7 depends on the answer: the unit is one window and one call
whether a window is 512 or 2048 tokens. Smaller windows mean more units, which
makes independent scheduling more valuable, not less.

## 11. Not in scope

**Retries cannot differ.** `llm.mikoshi.de` serves cached completions: an
identical prompt within roughly half an hour returns byte-for-byte, in
single-digit milliseconds. Any retry re-sending the same prompt cannot produce a
different result — it burns an attempt and doubles a backoff for nothing. This
affects the inline paraphrase retry (§3), which is currently a guaranteed no-op.
The fix is a seed or nonce on retry, or disabling response caching for the
synthesize role. Deferred by decision on 2026-08-13; recorded here because the
per-unit backoff in §5 will otherwise look inexplicably patient.

**Duplicate JSON keys still cost a window.** `duplicate field \`tags\`` killed
window 3 for twelve attempts. serde's derived `Deserialize` rejects repeated
keys; routing each object through `serde_json::Value` first accepts them
last-key-wins. Verified, deferred by the same decision.
