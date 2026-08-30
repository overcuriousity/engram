# Reaping the retired — a graveyard, not a museum — Design

Date: 2026-08-30
Status: implemented (2026-08-30)

> **Implementation notes.** The judge rides `core.judge` — the completer
> dedupe already uses — so the spec's `reap.tier` key was dropped rather than
> building per-sweep tier machinery no other sweep has; the rescue rewrite
> rides the same completer with `RESCUE_SYSTEM` and the generation parser.
> The `retired_at` backfill is a stamping pass inside the sweep
> (`stamp_unaged_retired`), not a migration: a pre-column retired row gets a
> fresh clock on first sight, same effect as §2's "backfilled to now". The
> spec's FTS remarks predate the current schema, which has no FTS — the wipe
> is one UPDATE and the Qdrant point delete. `bury` takes the reason inside
> `meta_json` rather than as a third argument. Rescue re-points only a
> candidate with no `superseded_by`; an already-superseded one keeps its old
> winner and loses to the rewrite as a live neighbour on the second pass.
> Sweep gating is `reap.enabled && core.judge.is_some()`. Adds
> `src/jobs/reap.rs`; touches `src/store/schema.sql` (two columns, one
> table), `src/store/mod.rs` (ADDITIVE), `src/store/artifacts.rs`,
> `src/store/sweeps.rs` (`last_sweep_run`), `src/store/jobs.rs` (one stage),
> `src/core/{mod,background,ingest}.rs`, `src/jobs/{mod,consolidate}.rs`
> (drift repair deletes a reaped row's point), `src/infer/prompt.rs`,
> `src/web/{api,ui}.rs`, `src/cli/status.rs`, `src/config.rs`, `README.md`.

## 1. Why

Retirement in engram is a one-way door into a waiting room nobody visits. A
deprecate or a supersede takes an artifact out of search, out of `relate`, out
of dedupe's interest — and then keeps its full text, its FTS entry and its
Qdrant point forever, on the theory that an operator might one day press
Restore. The operator has said they will not: the direction of the base is
away from manual housekeeping, and a pile that only ever grows and is only
ever read by accident (the forgotten list must actively filter it out;
`consolidate` must actively stop it winning clusters) is not memory, it is a
museum with the lights off.

Two failure modes follow from keeping everything:

**Worthless rows cost forever.** A superseded duplicate whose successor has
carried its content for months still holds a vector payload, an FTS shadow
kept out by filters, and a row every lifecycle-aware query must step around.
None of that will ever be read again, and nothing today is allowed to say so.

**Valuable rows die silently.** A *deprecated* artifact has, by definition, no
successor. If it was retired for being stale rather than for being wrong, the
one true thing it said is now invisible and will stay invisible — the base
holds the knowledge and cannot surface it, which is worse than not holding it.

So: a periodic sweep that revisits the retired, judges each one — mechanically
where mechanics suffice, with one model call where they do not — and then
acts on the verdict without asking anyone. Worthless: the live text is wiped
and a copy laid in a graveyard table. Valuable: the content is rewritten into
a live `Synthesized` artifact and the original becomes an ordinary future
candidate. No operator queue, no review list, no housekeeping page.

## 2. What is built

1. **One sweep stage, `Reap`** — collection-targeted like `Consolidate`, one
   in the queue at a time, armed on its own interval with the standard
   empty-run backoff. Module `src/jobs/reap.rs`.
2. **One column, `artifacts.retired_at`** — stamped by every transition out
   of `active` (`deprecate`, `supersede`), cleared by every transition back
   (`reactivate`, `unsupersede`). Backfilled to *now* for already-retired rows
   at migration, which starts their clock fresh — safe, merely slow.
3. **One table, `graveyard`** — where wiped text goes. Same database, no FTS
   triggers, never embedded, invisible to every search path.
4. **One model question per surviving candidate** — "does this still state
   anything the live base does not," answered against the candidate's live
   neighbours and (when superseded) its successor.
5. **One rescue path** — a valuable verdict is handed to the existing
   synthesis machinery, which writes a live `Synthesized` artifact and
   supersedes the retired one with it.

## 3. Candidate rules — the free pass

SQL and one Qdrant payload read; no model call. A retired artifact becomes a
candidate only when every rule holds:

- `status != 'active'` and `retired_at` older than `reap.min_age_days`
  (default 90).
- Unseen since retirement: the Qdrant payload's `last_seen_at` is null or
  predates `retired_at`. The stamps live only on the point, so the rules pass
  retrieves payloads by point id for the age-qualified set — a round trip,
  not a query per row.
- No open moment/reminder names it (`moments` join).
- Not itself named as a live merge's source whose merge is still active and
  younger than `reap.min_age_days` — a fresh merge's undo window stays intact;
  an old one has forfeited it (see §5).

What the rules alone may *never* do is tombstone. Two populations part ways
here:

- **Superseded, successor alive**: the strongest case — content nominally
  carried forward. Still goes to the model, because a supersede is a claim,
  not a proof, and the judge is cheap at this volume.
- **Deprecated, or superseded with a dead successor**: possibly the last
  carrier of its content. Model always judges; rules only nominate.

Per run the candidate set is capped at `reap.max_judged_per_run` (default 20),
oldest `retired_at` first, so model spend per sweep is bounded and the
backlog drains steadily rather than in one expensive night.

## 4. The verdict — one call per candidate

Prompt material: the candidate's text and title; its successor's text if
superseded; the top-k nearest *active* neighbours by its stored vector
(`include_deprecated` off — the judge compares against what a searcher can
actually reach). One question, two verdicts:

- **`worthless`** — everything it states is stated by the live base. Act: §5.
- **`valuable`** — it states something the live base does not. Act: §6.

A refused, malformed or failed call leaves the row untouched; it is simply a
candidate again next interval. No retry bookkeeping — the sweep's cadence is
the retry.

Direct wipe, by decision: there is no grace pass between verdict and wipe.
The graveyard is the insurance instead.

## 5. Tombstone — wipe live, copy cold

In one transaction:

1. Insert into `graveyard(id, title, text, meta_json, reaped_at)` — the full
   text, plus a JSON snapshot of provenance, tags, corpus/span, status,
   `superseded_by`, and the judge's one-line reason. The copy is written
   before anything is destroyed.
2. Update the artifact row: `text = ''`, a `reaped_at` stamp; `id`, `status`,
   `superseded_by`, `title`, provenance and `artifact_sources` links stay —
   the stub keeps every thread other rows hold into it, so `superseded_by`
   pointers never dangle and merge history stays readable.
3. The FTS `AFTER UPDATE OF text` trigger replaces the FTS entry with the
   empty text on its own.

Outside the transaction, under the same `lifecycle_dirty` protocol the embed
path uses: delete the Qdrant point. The flag is set before the delete and
cleared on acknowledgement, so a delete that fails leaves a marked row the
repair pass notices, not a ghost point scoring against live artifacts.

Undo-merge on a reaped source is gone — the text no longer lives in
`artifacts`. That is the accepted price of §3's fourth rule: only merges older
than the reap age can lose sources, and an operator who has not unmerged in
ninety days is not going to. (A manual restore from the graveyard remains
possible by hand — one INSERT and a re-embed — but no code path, page or
button is built for it. Building one would be the housekeeping this design
exists to remove.)

`graveyard` rows are permanent. No second-order reaper; the table is cheap
text in a database that already holds the base, and its whole purpose is that
nothing judged twice is ever judged wrong invisibly.

## 6. Rescue — rewrite, supersede, move on

A `valuable` verdict hands the candidate to the synthesis path:

- **Source-text candidate** (`Captured`, `Passage`, `Note`): the model writes
  a fresh statement of what remains true, as a `Synthesized` artifact whose
  `artifact_sources` names the candidate.
- **Model-written candidate** (`Merged`, `Synthesized`): the rewrite's sources
  are the candidate's own roots through `artifact_sources`, followed
  transitively to source text — never the candidate's text itself. This is
  the `is_model_written` rule doing exactly the job it was written for: no
  paraphrase of a paraphrase ever poses as an original.

The new artifact enters the base like any synthesis: embedded, searchable,
dedupe material. The candidate is then superseded by it — which makes the
candidate an ordinary reap candidate again in `min_age_days`, and on that
second visit the successor is alive and carrying the content, so the likely
verdict is `worthless` and the loop closes at the graveyard.

Rescues per run are capped at `reap.max_rescues_per_run` (default 3): one bad
judging batch may not flood the live base with model-written text.

## 7. Config

```toml
[reap]
enabled = true
interval_mins = 1440          # daily; backoff stretches quiet bases
min_age_days = 90
max_judged_per_run = 20
max_rescues_per_run = 3
tier = "cheap"                # judge tier; rescue synthesis uses the
                              # synthesis path's own tier
```

`enabled = false` stops arming the stage; nothing else changes, and the
retired simply wait, as they do today.

## 8. Visibility

- The sweep reports through `sweep_runs` like every other: judged, reaped,
  rescued, skipped, failed.
- `--status` gains one line under jobs: `reap  N judged · N reaped · N
  rescued` for the last run, and the standing count of retired rows not yet
  of age.
- Log lines per action with artifact id and the judge's reason. No UI page.

## 9. Testing

The suite follows the house pattern — fake embedder, fake completer, real
store:

- Rules: an active artifact is never a candidate; a young retired one is not;
  a retired-but-seen one is not; one named by an open reminder is not; a
  fresh merge's source is not, an old merge's source is.
- Verdict handling: `worthless` reaches the graveyard in one transaction and
  the FTS entry empties; a failed call leaves the row intact and unmarked;
  a failed Qdrant delete leaves `lifecycle_dirty` set.
- Tombstone integrity: `superseded_by` into a reaped stub still resolves;
  the detail pane renders the stub without a corpus read; search never
  returns it even with `include_deprecated` (empty text, no vector).
- Rescue: a model-written candidate's rewrite cites roots, not the candidate;
  the rescued original is superseded by the rewrite; the rescue cap holds.
- The closed loop: rescue, age past `min_age_days`, second sweep reaps the
  original against its living rewrite.

## 10. What it must not break

- **`include_deprecated`** still surfaces retired-but-unreaped artifacts;
  reaped stubs are gone from it by consequence (no vector, empty FTS), not by
  a new filter.
- **The forgotten list, `relate`, dedupe, consolidate** already exclude the
  retired; a reaped stub must not re-enter any of them through the empty-text
  side door — every path that reads `text` must tolerate the stub.
- **`lifecycle_dirty`** keeps its exact meaning: a row whose Qdrant payload
  may disagree. The reap's point-delete uses it; the repair pass must treat
  "row reaped, point present" as drift to fix by deletion.
- **Restore semantics** for the unreaped retired are untouched: `reactivate`
  and `unsupersede` work exactly as today right up until the wipe.
