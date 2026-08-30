# The evaluation harness

Two benchmarks, over your own base, both `#[ignore]`d so an ordinary
`cargo test` never touches them.

They exist for one reason. Ranking has knobs — fusion, the per-source cap,
recency weight, reranking, priming — and hand-testing cannot judge any of them,
because the queries anyone thinks to type reuse words they remember from the
passage they are looking for. A knob change either moves a number or it is a
preference. Hence the rule this repository holds itself to:

> A default that changes ranking moves only after the harness has been run.

That rule is the whole of what the harness is *for*. It is not a test suite —
it asserts almost nothing — and a bad score is not a failure. It is the one
figure comparable across months.

---

## 1. What each one measures

### `evaluate_retrieval` — did search put the answer on the page?

Replays judged searches against a freshly indexed copy of your artifacts and
scores where the expected artifact landed.

| Metric | Question it answers | Read it when |
|---|---|---|
| **recall@10** | Was the answer in the first ten results at all? | Changing what is *retrieved*: embeddings, fusion, the candidate pool, the per-source cap. |
| **MRR** | How far down was it? | Changing what is *ordered*: recency, pinning, reranking, priming. |

Both, never one. A change can lift recall and hurt MRR — a wider candidate pool
finds more and buries it deeper — and which of those matters is a judgement
about what a search page is for, so the harness reports both rather than
choosing for you. `mrr` counts a miss as zero rather than excluding it: a
ranking that answers one query perfectly and fails nineteen must not be able to
report a perfect score.

**The miss list under the numbers is the part to read.** An aggregate says
something moved; the list says *what* moved, which is what a knob change is
actually judged on. Each miss is named by the first 48 characters of its own
query — no artifact text is ever printed.

### `evaluate_ask` — was the answer honest?

Asks each judged question again and scores the answer against what a person
said about the original.

| Metric | Question it answers |
|---|---|
| **citation recall** | Of the excerpts the operator marked as carrying the answer, how many did the model actually cite? |
| **abstention accuracy** | Did it say *not in the knowledge base* exactly when there was nothing there? Reported as two counts, because the two mistakes are not equal: answering when it should have abstained is confabulation, abstaining when it should have answered is timidity. |
| **unsupported literals** | Commands, paths and flags in the answer that appear in no cited excerpt. Prose is left alone on purpose — a bare number inside a sentence of explanation is not the kind of claim this looks for, and a guard that fires on ordinary writing is one you learn to ignore. |
| **claims supported** *(opt-in)* | With `ENGRAM_EVAL_CLAIMS=1`, the synthesize model traces every claim in the answer to an excerpt. One extra call per answered question. |

---

## 2. Where the data comes from

Nothing here is fixtures. The corpus is whatever you actually want to search,
and it is **not in this repository and must not be**.

Pairs are made mostly by searching. Under a result opened from the rail there is
a bar — *Was this what you were looking for? Yes · No · Not sure* — and on a rail
that matched nothing there is *Nothing here has it*, which records a gap. Every
pair comes from somebody pressing one of those. A long read used to count as a
*Yes* on its own; it does not any more, because what it measured was a pane left
open, which is an abandoned tab about as often as it is an answer. *Not sure* is
not a verdict either: it leaves the search for the deck, which is where a
question nobody could answer in the moment belongs. Only what none of these
labelled reaches `/ui/judge`,
where a recorded search comes back with the top five of its pool in the order
the search gave them, the rest behind a fold, and you say which one you needed.
Showing the order is a known cost: a person is likelier to confirm what came
first, so the recall@10 and MRR read off deck verdicts lean slightly towards
the ranker. Five in order is a question a person answers; twenty shuffled was
one nobody answered twice. Scores are still withheld, and the query is still
recorded in the moment and the verdict is not.

```bash
engram --export-eval ~/engram-eval
```

writes three files. The export reads SQLite only: no inference, no Qdrant, and
artifacts keep their real ids, so re-exporting does not invalidate pairs you
already judged.

| File | What it holds |
|---|---|
| `artifacts.json` | Every artifact, frozen, so a run costs no completions and two runs rank exactly the same text. |
| `pairs.json` | `{query, expect}` — a query, and the artifact id that answered it. |
| `questions.json` | `{question, verdict, expect[]}` — `right`, `wrong` or `nothing_here`, plus the excerpts marked as carrying the answer. |

A pair naming an artifact that no longer exists is a hard error rather than a
miss: it is a stale pair left by a deletion, and scored as a miss it would look
like a ranking problem forever. Re-export.

A grade is also satisfied by anything that **superseded** the artifact it names.
Merging moves knowledge into a new artifact and search correctly returns that
one; scoring only the original would report a retrieval regression that is
really a bookkeeping change — exactly when it matters most.

---

## 3. Running it

Needs a reachable Qdrant and a real embedding endpoint, both read from
`config.toml`. The fake embedder produces meaningless vectors, so a benchmark
built on it would measure nothing — which is why these are `#[ignore]`d.

```bash
# retrieval
ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture

# ask
ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval evaluate_ask -- --ignored --nocapture
```

Both use their own Qdrant collection, `engram_eval`, dropped before and after.
**Your real index is never touched**, and a run is never polluted by the one
before it.

With no corpus at `ENGRAM_EVAL_DIR` both print what is missing and return
without failing: most people running `cargo test` have no corpus, and a
benchmark that cannot run has nothing to report either way.

---

## 4. Tuning

Every ranking setting comes from configuration, so a sweep is a loop over
environment variables rather than a rebuild. The prefix is `ENGRAM__` and the
nesting separator is `__`:

```bash
for w in 0.0 0.05 0.15; do
  ENGRAM__VECTOR__RECENCY_WEIGHT=$w \
  ENGRAM_EVAL_DIR=~/engram-eval \
    cargo test --test eval -- --ignored --nocapture | head -5
done
```

The harness prints its settings line above the numbers on purpose: **a number
recorded without the configuration that produced it cannot be compared against
anything.** Keep the whole line when you write a result down.

| Knob | Variable | What it moves | Watch |
|---|---|---|---|
| Recency weight *(swept at runtime — see 4½)* | `ENGRAM__VECTOR__RECENCY_WEIGHT` | How much age counts against a hit. `0.0` turns it off. Fused ranks sit between ~0.1 and 1.0, so the default breaks near-ties without overturning a clear match. | MRR |
| Recency half-life | `ENGRAM__VECTOR__RECENCY_HALF_LIFE_DAYS` | Age at which a hit has lost half that boost. | MRR |
| Pinned boost | `ENGRAM__VECTOR__PINNED_BOOST` | Extra score for a `pinned` tag, so a decision you made beats the decay curve. | MRR |
| Per-source cap *(swept at runtime — see 4½)* | `ENGRAM_EVAL_CAP` here, `[vector] per_source_cap` in the server | Chunks one document may contribute. Default 3; `none` (`0` in the file) lets one document fill the list. Raising it usually lifts recall and costs diversity. | recall@10 |
| Reranker | `[infer.rerank]` present or absent in `config.toml` | A cross-encoder over the candidate pool. The settings line reports `rerank on/off`. | MRR first, recall second |
| Embedding model | `ENGRAM__INFER__EMBED__MODEL` (+ `DIM`) | The whole retrieval geometry. Needs `--reindex` against a real base; the harness reindexes its own collection anyway. | both |
| Embed templates | `ENGRAM__INFER__EMBED__QUERY_TEMPLATE`, `..._DOCUMENT_TEMPLATE` | The envelope each side is embedded in. Asymmetric models care a great deal. | both |
| Priming lift | `ENGRAM__ASSOCIATE__PRIME_LIFT` | How many places an accessible hit may climb. `0` turns priming off. | MRR |
| Priming margin | `ENGRAM__ASSOCIATE__PRIME_MARGIN` | How much more accessible it has to be before it climbs. | MRR |
| Weak threshold | `ENGRAM__VECTOR__WEAK_BELOW` | Similarity under which a hit is labelled *loose*. Changes no order — it changes what the page claims, and what becomes an `unmatched` knowledge gap. | neither; read the page |

For the ask harness, the levers are the excerpt budget and the answering model
(`[infer.tiers.deep]`, `max_output_tokens` — the ceiling comes out of the
context window, so raising it buys longer answers by showing the model fewer
excerpts, and `dropped` on the answer says how many).

### A tuning session, in order

1. Judge until you have enough pairs to move a number. **Twenty is a floor, not
   a target**: with ten pairs, recall@10 moves in ten-point steps and every
   result is noise.
2. Record a baseline, settings line and all.
3. Change **one** knob. Re-run. Compare both metrics and read the miss list.
4. Keep the change only if the number moved and the misses got less bad. A knob
   that moves nothing is a knob that should keep its default.
5. Write the numbers into the commit message that moves the default. That is
   what the rule at the top asks for, and a commit that changes ranking without
   them is the thing this whole apparatus exists to prevent.

---

## 4½. Tuning at runtime

The two cheap knobs tune themselves. Once fifty judgements exist
(`feedback.tune.min_judgements`), every tenth further verdict
(`feedback.tune.resweep_after`) re-runs a background sweep of recency weight ×
per-source cap over every judged pair, against the live index. It needs no
export, no frozen corpus and no re-embedding: both knobs only reorder what
retrieval already returned, so a whole grid is seconds of vector reads. Like
the assign search, it reads and never records — `Door::Judge`, `mark: false`.
It asks the whole grid about one query before moving to the next, so each query
is embedded once however many pairs there are, and it takes the background lane
rather than the interactive one: nobody is waiting on a replay of questions
that were already answered, and thousands of searches on the fast lane would
hold every worker off for the length of the run.

Its two figures are a **replay**, not the header's. The counter at the top of
`/ui/judge` is recall@10 and MRR over the positions the searches actually gave;
a sweep's are those searches run again, now, under each setting, through a door
that leaves priming out. Both are honest and neither substitutes for the other
— read `MRR 0.50 → 0.60` against itself, never against the number above it.

A candidate is offered only when **at least two pairs are net better and
neither aggregate is worse**. That floor is the whole safety of running it
automatically: on fifty pairs a single flipped pair is two points of recall,
and an aggregate delta alone cannot tell one from a real improvement. Ties keep
the current values.

The recommendation appears on `/ui/judge` with the pairs that moved, and
applying it rewrites `config.toml` — beside the file and renamed over it, so a
crash mid-write leaves the operator's file as it was — and swaps the running
parameters in one step. Only the newest sweep's recommendation stands: a later
sweep looked at the same pairs over more evidence, so whatever it says, it says
last, including when what it says is nothing. Every sweep is recorded in
`eval_runs` with the settings that produced it, recommended or not, which is
section 4's rule about never writing a number without its configuration, made
structural rather than asked for.

What this does **not** replace: the harness below stays the instrument for
everything a runtime sweep cannot reach — the embedding model and its
templates (they change the vector geometry, not the order over it), priming,
pinning, the ask side, and any number that has to be comparable across months
rather than against today's other candidates.

---

## 5. What it cannot measure

Being clear about this matters more than the numbers, because the temptation is
to run *something* and call the question answered.

- **Anything about a sequence of queries.** The harness scores each pair
  independently against a static index. Features about continuity within one
  sitting — priming from the live sitting (`[sitting] prime`), working memory,
  anything reading `Origin::session` — cannot move a number here, because the
  harness searches through `Door::Ui` with no session attached. Measuring those
  needs a harness that scores *runs* of queries, which does not exist yet. Until
  it does, `[sitting] prime` stays off and unmeasured, and ROADMAP.md says so.
- **Anything needing engagement.** Activation, pursuits and promotion are
  driven by what a person opened and dwelt on. The harness opens nothing and
  sets `mark: false` deliberately — resurfacing reads `last_seen_at`, and a
  scored run is not someone reading their notes.
- **Whether an artifact is any good.** It measures whether the right artifact
  was *found*, never whether it was worth finding. Synthesis quality is what
  the ask harness's faithfulness metrics get at, obliquely.
- **A base too small to have an opinion.** Under twenty pairs the arithmetic
  works and the result means nothing.

---

## 6. Where the pieces live

| Path | What |
|---|---|
| `tests/eval.rs` | Both harnesses, the report formatting, and one non-ignored wiring test that runs without infrastructure. |
| `src/eval/mod.rs` | The on-disk shapes: `FrozenArtifact`, `EvalPair`, `EvalQuestion`. |
| `src/eval/metrics.rs` | `recall_at`, `mrr`, and the ask metrics. |
| `src/eval/export.rs` | `--export-eval`. |
| `src/eval/claims.rs` | Literal extraction and claim support. |
| `src/eval/sweep.rs` | The runtime sweep: the grid, the gate, and the job a verdict starts. See 4½. |
| `src/store/eval_runs.rs` | Every sweep, with the settings that produced it and whether it was applied. |
| `/ui/judge` | Where pairs come from, and where a sweep reports. Its counter is not a stand-in for the measurement — it *is* recall@10 and MRR over the positions those searches actually gave. |
