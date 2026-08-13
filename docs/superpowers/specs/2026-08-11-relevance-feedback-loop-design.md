# Relevance feedback loop — design

Date: 2026-08-11
Status: approved, ready for an implementation plan

## Why

Every retrieval decision on the roadmap — reranking by default, server-side
grouping, whether caveats belong in the embedded text — is currently settled by
argument rather than measurement. The evaluation harness exists
(`tests/eval.rs`, `src/eval/`) but is unpopulated, and the stated reason for
leaving it unpopulated is that no decision turns on it yet. That is circular:
the decisions that need it are parked behind it.

Hand-writing query/artifact pairs does not escape the problem. A query composed
while looking at the artifact reuses the artifact's vocabulary, and every
retrieval system passes such a pair. `src/eval/mod.rs:44` already says this.

The only uncontaminated source of a query is a real search, typed before the
searcher knew what would come back. The only reliable source of a relevance
label is a person. This feature captures the first and collects the second,
separated in time so the label cannot contaminate the query.

## The invariant

**Captured feedback never influences the ranking of an individual search.** It
acts only through globally-scoped parameters, and only through a proposal the
operator has accepted.

This continues a decision the codebase already made: `hit_count` is recorded but
is "deliberately never a term in search scoring, or a popular result would keep
boosting itself further while a correct but rarely-queried artifact never gets
the chance" (`src/vector/mod.rs:23`).

The invariant fixes the dependency direction: the search path *writes* to the
feedback store and never reads from it. Reads happen while judging, sweeping and
exporting — all outside the request path.

## Scope

In scope:

1. Config-gated capture of real searches, with their candidate lists.
2. A judging view that turns captured searches into labelled pairs.
3. A background sweep that proposes global parameter changes, with an operator
   accept/undo step and a config overlay.
4. Export to the harness format, plus the harness bug fix that currently makes
   any evaluation report zero.

Out of scope, decided explicitly:

- **No content-side effect.** A judgement does not attach the query to the
  artifact, does not flag knowledge gaps to the ranker, and does not change what
  any vector is built from.
- **No click or open tracking.** Judging is explicit; the weak signal would only
  reintroduce the position bias the design removes elsewhere.
- **No model call anywhere in this feature** — not on capture, not on judging,
  not during a sweep.
- **`ask` is not captured.** Its correct answer is a synthesis across several
  artifacts, so "which one was it" has no well-defined meaning there.

## Discovered constraints

Three facts from the current code shape the design:

1. `recency_weight` and `pinned_boost` are applied **server-side** by Qdrant's
   formula query (`scoring_formula`, `src/vector/qdrant.rs:321`, used at
   `:1599`). A sweep therefore cannot re-score a cached hit list; it must issue
   real Qdrant queries per parameter value.
2. `weak_below` only labels results (`src/core/search.rs:395`). It cannot move
   MRR or recall, so it needs its own objective.
3. `tests/eval.rs` is broken. `index()` inserts frozen artifacts via
   `store.insert_artifacts`, which assigns fresh ids (`src/store/artifacts.rs:181`),
   while scoring compares against the ids from `artifacts.json`. Every pair
   scores as a miss and every run reports 0.00. This has gone unnoticed because
   the test is `#[ignore]`d and returns early without pairs.

## Architecture

| Unit | Purpose | Depends on |
|---|---|---|
| `store::feedback` | Persistence: search events, candidate lists, verdicts, proposals, overrides, history. Pure data; knows nothing of Qdrant or search. | SQLite |
| capture hook in `core::search` | Writes an event after candidates are assembled, via `Background::spawn`. | `store::feedback` |
| `web::judge` | Judging page and fragments. | `store::feedback`, `store::artifacts` |
| `jobs::tune` | Periodic sweep, produces proposals. Reuses `eval::metrics` unchanged. | `store::feedback`, `vector`, `eval::metrics` |
| `Tunables` | Ranking parameters that can change at runtime, shared between config, the Qdrant client and search. | — |
| `--export-eval` | Writes `pairs.json` and `artifacts.json`. | `store::feedback`, `store::artifacts`, `eval` |
| `config::FeedbackConfig` | One config section, disabled by default. | — |

### The Tunables rebuild

`recency_weight` and `pinned_boost` are baked into `QdrantVectors` at connect
(`src/vector/qdrant.rs:472`); `weak_below` sits in `Core`. Both move into a
shared `Arc<Tunables>` read atomically, so an accepted proposal takes effect
without a restart. This is a contained change to two structs and their
constructors. The cheaper alternative — "applies after restart" — was considered
and rejected: it turns a one-click action into a chore, which means fewer
proposals get accepted.

`Tunables` holds four values: `recency_weight`, `pinned_boost`, `weak_below` and
`cap_per_corpus`. The last is a constant today (`MAX_PER_CORPUS = 3`,
`src/core/search.rs:14`); to be tunable it becomes a config key
`vector.cap_per_corpus`, defaulting to 3 so behaviour is unchanged. In
`tuning_overrides` it is stored as a REAL like the others, where **0 means the
cap is off**.

## Data model

Migration `migrations/0013_feedback.sql`.

```sql
CREATE TABLE search_events (
  id          TEXT PRIMARY KEY,            -- uuid v7, chronologically sortable
  query       TEXT NOT NULL,
  door        TEXT NOT NULL,               -- 'ui' | 'api' | 'mcp'; Door::Judge
                                           -- exists but is never persisted
  filters     TEXT NOT NULL DEFAULT '{}',  -- json: tags, category, limit
  query_vec   BLOB NOT NULL,               -- f32, little-endian
  vec_dim     INTEGER NOT NULL,
  embed_model TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  judged_at   INTEGER,                     -- NULL = pending
  verdict     TEXT,                        -- 'hit' | 'gap' | 'discard'
  expect_id   TEXT,                        -- artifact id when verdict='hit'
  skips       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_events_pending ON search_events(judged_at, skips, created_at DESC);
CREATE INDEX idx_events_verdict ON search_events(verdict);

CREATE TABLE search_candidates (
  event_id    TEXT NOT NULL REFERENCES search_events(id) ON DELETE CASCADE,
  rank        INTEGER NOT NULL,      -- 0-based, as retrieved
  artifact_id TEXT NOT NULL,         -- deliberately no FK, see below
  score       REAL NOT NULL,
  similarity  REAL,
  shown       INTEGER NOT NULL,      -- 1 = was in the answer the searcher saw
  PRIMARY KEY (event_id, rank)
);

CREATE TABLE tuning_proposals (
  id                 TEXT PRIMARY KEY,
  created_at         INTEGER NOT NULL,
  param              TEXT NOT NULL,
  from_value         REAL NOT NULL,
  to_value           REAL NOT NULL,
  n_tune             INTEGER NOT NULL,
  n_holdout          INTEGER NOT NULL,
  metric             TEXT NOT NULL,  -- 'mrr' | 'weak_accuracy'; see Two objectives
  base_primary       REAL NOT NULL,  -- all four measured on the holdout half
  proposed_primary   REAL NOT NULL,
  base_secondary     REAL,           -- recall@10 when metric='mrr', else NULL
  proposed_secondary REAL,
  state              TEXT NOT NULL,  -- 'open'|'accepted'|'dismissed'|'superseded'
  decided_at         INTEGER
);

CREATE TABLE tuning_overrides (
  param       TEXT PRIMARY KEY,
  value       REAL NOT NULL,
  proposal_id TEXT,
  applied_at  INTEGER NOT NULL
);

CREATE TABLE tuning_history (        -- append-only; carries the undo
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  param       TEXT NOT NULL,
  from_value  REAL,                  -- NULL = the config file's value
  to_value    REAL,
  proposal_id TEXT,
  at          INTEGER NOT NULL,
  actor       TEXT NOT NULL          -- 'operator' | 'auto'
);
```

### Why the query vector is stored

A sweep must issue real Qdrant queries per parameter value. Without a stored
vector that means re-embedding every judged query on every sweep — a burst of
inference for a measurement, and a different result each run unless the model is
bit-deterministic. Stored, a sweep costs no inference and is repeatable. Cost is
about 3 KB per event at 768 dimensions.

`embed_model` guards it: if the embedding model changes, older vectors are not
comparable and those events drop out of sweeps. They stay usable for **export**,
because the harness embeds afresh and the query text does not age.

### Why the candidate pool is wider than the answer

Up to `feedback.candidates` (default 20) rows are stored, not just the ten
returned. This is free: search already over-fetches `CANDIDATE_MULTIPLIER = 3`.

The wider pool is the position-bias countermeasure. The judging card offers the
whole pool, shuffled and without ranks, so the operator can confirm an artifact
that ranked 14th — which is the only way a ranking failure becomes measurable.

`artifact_id` carries no foreign key on purpose: deleting an artifact must not
erase the history of what was once returned. Dangling ids are skipped at judging
and export time.

### Effective configuration

The effective value of a tunable is the override if one exists, otherwise the
value from `config.toml`. The config file is never rewritten. `--print-config`
marks overridden values, so "what is in force right now" stays a question one
command answers.

## Capture

Hooked into `core::search` after candidates are assembled and before capping and
truncation, written through `Background::spawn` — the same mechanism
`mark_seen` already uses next to it (`src/core/search.rs:181`). Search cannot get
slower or fail because of it; write errors are logged at `warn` and go no
further. Shutdown already drains `Background` (`src/main.rs:293`).

**Door.** Passed as an explicit parameter to the search entry points rather than
added to `SearchQuery`, which is deserialised from the query string: a default
there would silently record an API search as a UI search when a caller forgets.
Three call sites (`web::ui`, `web::api`, `mcp`) and no `Default` impl, so the
compiler enforces it.

**What is captured.** Every search with a non-empty query, regardless of `mark`,
with prefix coalescing (below).

Empty result lists **are** captured, deliberately diverging from the neighbouring
rule for `mark_seen` (`an_empty_result_list_is_not_marked_seen`). A search that
found nothing is the most direct evidence of a knowledge gap the system will
ever get.

Blank or whitespace-only queries are not captured; the search page fires one on
load.

**Prefix coalescing.** Capturing only `mark = true` searches would lose the most
valuable case: `mark` is set on open, expand and submit, so a search where the
operator found nothing useful and gave up would never be recorded. Instead every
search is captured and the store folds typing bursts together: if an incoming
event shares a door with the previous event, arrives within
`feedback.coalesce_secs`, and the previous query is a strict prefix of the new
one, it replaces that event and its candidate rows in one transaction. `daten` →
`datenträ` → `datenträger nicht erkannt` becomes a single event holding the final
wording. Entirely server-side, no JavaScript.

**Not captured:** whether anything was opened afterwards. That is click data by
the back door, and shown on a judging card it would restore exactly the bias the
shuffled pool removes.

**Retention.** `feedback.retain_days` (default 0 = unlimited), swept by the
existing consolidation ticker. Ops offers "delete all captured searches", which
removes events, candidates and open proposals; accepted overrides and history
survive, because they describe how the application is currently configured.

**Visibility.** One line on Ops while capture is running ("searches are being
recorded — 34 captured, 12 unjudged"). Nothing on the search page; an indicator
next to the search box would sit in view during every keystroke.

## Judging

Its own page at `/ui/judge`, linked from Ops with a count. Judging is an activity
done in a stretch; Ops is an overview.

**The card.** The query verbatim, with date and door. Below it the stored
candidate pool: shuffled, without rank numbers, without scores or similarity
values — all of which are the ranker's opinion, which is precisely what must not
be heard here. Each entry shows title, two lines of text, category and tags, and
expands to the full artifact text.

Keyboard-first: `↑`/`↓` move a cursor, `Enter` selects, `1`–`9` shortcut the
first nine, `N`, `S`, `X` for the three special cases. Each answer posts via htmx
and swaps in the next card. The target is one judgement in five seconds.

| Key | Meaning | Result |
|---|---|---|
| digit / Enter | "That was the one." | `verdict='hit'`, `expect_id` set |
| `N` | "None of these." | Opens the normal search inline to find and assign the right artifact → also `hit`, but with an artifact the ranker did not return. If nothing fits: `verdict='gap'` |
| `S` | "Can't remember." | Stays pending, `skips` incremented, sinks in the order |
| `X` | "Not a real search." | `verdict='discard'` |
| `U` | "That was the wrong key." | Clears `judged_at`, `verdict` and `expect_id`; the event returns to the queue on the card it was judged from |

The `N` path is what makes the exercise worth anything: it is the only source of
pairs whose confirmed answer lay outside what the system offered, and therefore
the only thing that can distinguish a ranking failure from a success.

**Reading before confirming.** Each candidate carries an expander that fetches
the full artifact text in place, on the card and in the `N` results alike. The
snippet is 140 characters — enough to recognise an artifact, not enough to be
sure of one — and the click after it writes a line the ranker is scored against.
The reading view says nothing about rank, score or whether the search showed the
artifact at all; leaking any of that here would undo what the shuffle protects.
`U` covers what reading cannot, which is the misfired digit.

**The assignment search inside `N` runs as `Door::Judge` and is never captured.**
Otherwise judging would flood the dataset with queries composed in full knowledge
of the answer — the contaminated kind.

**Order.** Newest first. A judgement is worth something because the operator
remembers the situation, and that memory is the most perishable part of the
dataset. Skipped events sink via `ORDER BY skips ASC, created_at DESC`.

**Edge cases.** Deleted artifacts are skipped and not displayed; a pool made
entirely of vanished artifacts leaves an `N`/`X` card. An empty pool (the search
found nothing at the time) opens directly into the `N` path. With nothing
pending, the page says so and links back to search. The view is read-only with
respect to artifacts: no editing, deleting or verifying from here.

### Gamification

One rule, because the wrong mechanics damage the data they exist to collect:

**Reward what is valuable and cannot be faked. Never speed, never agreement with
the ranker.**

A scheme that rewards fast clicking produces fast clicking; one that rewards
streaks produces invented judgements on tired evenings; one that celebrates every
hit equally nudges the operator toward agreeing with the top suggestion — the one
behaviour that destroys the dataset.

**The scoreboard is the real measurement.** Because every candidate's rank is
stored, the rank of a confirmed artifact is known the instant it is confirmed —
no Qdrant, no embedding. So the score shown is recall@10 and MRR themselves:

```
  47 judged        recall@10  0.68        MRR  0.54
  ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░  47 / 50 until the first sweep
```

After each card the value visibly moves: `MRR 0.54 → 0.57`. No invented currency.

**Immediate diagnosis, not confetti.** A line after each judgement saying what
was just learned, with emphasis deliberately inverted: rank 1 gets a small grey
"found as expected"; rank 14 gets "the ranking got this wrong — this is what
we're here for"; an `N` assignment outside the pool gets "a find: search would
never have shown you this"; a `gap` gets "a hole: your base doesn't know this
yet". The least informative card of the day is the one that ranked first, and the
interface should say so.

**Counters that mean something:** `31 hits · 9 finds · 5 gaps · 2 discarded`.
Finds and gaps are the rare, expensive events; giving them their own visible slot
is the actual incentive, because a find only happens when the operator honestly
presses `N` instead of conveniently taking number two.

**Levels that are not invented:**

| Level | Threshold | Unlocks |
|---|---|---|
| Field notes | 10 judgements | The miss list becomes visible: judged queries whose confirmed artifact fell outside the top 10, read straight from the stored ranks |
| Measurement | 50 judgements | The sweep runs and proposals can appear (`min_judgements`) |
| Burden of proof | 100 judgements | The holdout carries weight; `auto_apply` may be switched on |

These are defensible because they are true statements about statistical power. At
twelve judgements a proposal *is* noise. The lock is real, so unlocking it is too.

**A gap closes the loop.** Judging `gap` offers "write it down now" — a jump to
`/ui/capture` with the query pre-filled as the title hint. It does not violate the
invariant: nothing happens automatically, the operator writes something.

**Explicitly absent:** timing, daily streaks that an honest `S` would break,
points, badges, leaderboards.

### Three numbers, three names

Three figures could all be called "MRR" and none are comparable:

1. **Field value** — from the stored ranks at search time. The number on the
   judging page. Describes how good search was in actual use.
2. **Sweep value** — from a replay against today's collection under changed
   parameters. The number on proposal cards.
3. **Harness value** — against the frozen corpus, in the terminal. The only
   figure comparable across months.

Every display carries its name.

## The tuning engine

A `Stage::Tune` job driven by a ticker modelled on `spawn_consolidation_ticker`
— same `UNIQUE(stage, target_id)` guarantee, same shutdown handling. It runs only
when `feedback.tune.enabled`, and returns immediately while fewer than
`min_judgements` usable judgements exist.

Usable means: `verdict='hit'`, the expected artifact still exists and is active,
and `embed_model` matches the current model.

### Two objectives

**Objective A — ranking.** Parameters `recency_weight`, `pinned_boost`,
`cap_per_corpus`, measured by MRR and recall@10 over confirmed pairs. Requires
real Qdrant replays.

**Objective B — weak labelling.** Parameter `weak_below`. It changes no ordering,
so MRR can never move; optimising it against MRR would compute zero forever.
Instead it is scored as a classification: `gap` judgements are the positives,
`hit` judgements the negatives, and the objective is balanced accuracy of the
"weak" label. This needs no queries at all — the top hit's similarity is already
in `search_candidates`.

### Replay

The sweep must not reimplement search, or it measures a program nobody uses. The
existing path is split so both go through one function:

```
today:  search()  = embed + retrieve + cap + label
after:  search()  = embed  →  search_with_vector(vec, filters, tunables)
        replay()  =           search_with_vector(vec, filters, candidate)
```

### Search space: one parameter per sweep

Fixed candidate lists in code, tried coordinate-wise:

| Parameter | Candidates |
|---|---|
| `recency_weight` | 0.0 · 0.025 · 0.05 · 0.1 · 0.2 |
| `pinned_boost` | 0.0 · 0.1 · 0.15 · 0.25 |
| `cap_per_corpus` | 1 · 2 · 3 · 5 · off |
| `weak_below` | 0.0 · 0.25 · 0.30 · 0.35 · 0.40 · 0.45 |

Never a joint grid: with fifty to a hundred queries, a search across three axes
will always find a combination that wins on paper and fails in use. At most **one
proposal per sweep** — the largest confirmed gain — which also keeps the history
readable: one change, one effect.

Cost: roughly 100 judgements × 5 candidates ≈ 500 Qdrant requests and no
inference. Seconds locally, and it runs on the job worker.

### The bar a proposal must clear

The dataset is split deterministically by a hash of the event id against
`holdout` — deterministic so a second run splits identically and newly arriving
judgements do not disturb the existing split.

"Gain" means the difference in the objective's **primary metric** — MRR for
objective A, balanced accuracy for objective B.

A proposal is created only if **all** hold:

- gain on the tuning half ≥ `min_gain`
- gain on the holdout half ≥ `min_gain`, **with the same sign**
- the holdout has at least 20 pairs
- for objective A only: recall@10 does not regress on the holdout

The double check is the whole difference between measurement and self-deception:
a win that appears only on the half that was optimised is fitted noise.

### Proposal lifecycle

An Ops card:

```
  recency_weight   0.05 → 0.0
  Holdout (24 pairs):  MRR 0.61 → 0.68     recall@10 0.71 → 0.75
  [ accept ]   [ dismiss ]
```

**Accept** writes `tuning_overrides`, appends to `tuning_history`
(`actor='operator'`) and updates the shared `Tunables` immediately. **Dismiss**
sets the proposal to `dismissed`, and the same (param, to_value) pair is not
proposed again for 30 days, so the system does not ask the same question daily. A
new sweep supersedes a still-open proposal for the same parameter rather than
queueing.

**Undo** restores the previous value and appends a new history row, so how a
setting reached its current state stays readable even across three steps.

### auto_apply

`feedback.tune.auto_apply = true` accepts automatically, additionally guarded by
at least 100 judgements ("burden of proof"), the same bar, an entry with
`actor='auto'`, and the same undo. Default off. It is the switch that turns this
into fully automatic tuning, and it is designed in now so enabling it later costs
no rebuild.

### What the sweep cannot do

It measures against **today's** collection. Artifacts added after a judgement may
legitimately outrank the confirmed one, which the sweep reads as a regression. It
is tolerable because the sweep only compares parameter values against each other
on the same day, but it is the reason the frozen harness remains the only figure
comparable across months.

## Export and harness repair

```
engram --export-eval ~/engram-eval
```

- **`artifacts.json`** — every active artifact from SQLite with its real id. No
  inference, no Qdrant. Superseded and deprecated are excluded so the benchmark
  sees the same corpus the real search does. `source` becomes the corpus title so
  the per-corpus cap applies.
- **`pairs.json`** — every `hit` judgement as `{ query, expect, note }`, with date
  and door in `note`.

The export warns about and skips pairs pointing at vanished artifacts. Because
the ids come from the production database and are stable, re-exporting is safe —
the reason `eval-prepare` invalidated all pairs does not apply here.

**The harness fix.** `tests/eval.rs` gains the translation table it lacks:
`insert_artifacts` returns the inserted artifacts in input order, giving
`frozen_id → new_id`, and `pair.expect` is translated before comparison. No
production code involved.

Plus a test for this class of bug: a run with two artifacts and the deterministic
fake embedder, where the query is verbatim one artifact's text, asserting rank 0.
It tests the wiring, not the quality — but the wiring was what was broken, and it
was covered by nothing.

## Configuration

```toml
[feedback]
enabled       = false   # off by default; query wording is personal
candidates    = 20      # stored candidates per event
coalesce_secs = 15      # window for prefix folding
retain_days   = 0       # 0 = unlimited
sweep_hours   = 6       # how often retention is enforced

[feedback.tune]
enabled        = false
interval_hours = 24
min_judgements = 50
holdout        = 0.5
min_gain       = 0.03   # required gain in the objective's primary metric,
                        # on both halves of the split
auto_apply     = false
```

With `feedback.enabled = false` nothing is written; the tables exist from the
schema and stay empty, so toggling needs no migration step. Retention runs on
its own ticker rather than inside the consolidation sweep: a window over
personal data must not lapse because an unrelated feature was switched off.

## Testing

Everything below runs without infrastructure, like the rest of the suite.

| Area | Coverage |
|---|---|
| `store::feedback` | Prefix folding: replaces within the window, not outside it, not across doors, not for a non-prefix. Candidates are swapped with the event. |
| Capture | Event written with the wide pool and correct `shown` flags · nothing when disabled · empty result list **is** captured · blank query is not · `Door::Judge` never is. Via `Background::wait_idle`, like the existing search tests. |
| Judging | Verdict transitions · ordering with `skips` · deleted artifact skipped · empty pool goes straight to the `N` path. Via `oneshot`, like the existing web tests. |
| Sweep logic | Split is deterministic and stable as data grows · the bar rejects a tune-half-only win · sign flips are rejected · `superseded` on a new sweep · the 30-day dismissal cooldown. Against a stub returning fixed rankings, no Qdrant. |
| `Tunables` | After accepting, the next search uses the new value · undo restores the old one · the config file is untouched. |
| Qdrant integration | One `#[ignore]`d end-to-end sweep beside the existing integration tests. |

## Documentation

`config.example.toml` gains the new keys with the reasoning behind each default,
as is the convention there. The README config table gains `feedback.*`, and the
"Asking for something" section gains a neighbour explaining in three sentences
what capture and judging are for. `ROADMAP.md` loses the paragraph describing the
eval pairs as "unpopulated by design" — that justification no longer holds.

## Build order

Five stages, each independently runnable and useful:

1. Migration + `store::feedback`
2. Capture + config — **the clock starts here**; data accumulates while the rest
   is built
3. Judging view including the gamification
4. Export + harness repair
5. `Tunables` rebuild + sweep + proposal card on Ops

Stage 2 landing early is the only place where the order really matters. Every day
without capture is a day of real queries that cannot be recovered later.
