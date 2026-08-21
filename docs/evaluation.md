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

Pairs are made by judging, at `/ui/judge`: a recorded search comes back with its
candidates shuffled and unlabelled, and you say which one you needed. The
shuffling is not decoration — a label assigned while reading the answer
contaminates the question, which is the same reason the query is recorded in the
moment and the verdict is not.

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
| Recency weight | `ENGRAM__VECTOR__RECENCY_WEIGHT` | How much age counts against a hit. `0.0` turns it off. Fused ranks sit between ~0.1 and 1.0, so the default breaks near-ties without overturning a clear match. | MRR |
| Recency half-life | `ENGRAM__VECTOR__RECENCY_HALF_LIFE_DAYS` | Age at which a hit has lost half that boost. | MRR |
| Pinned boost | `ENGRAM__VECTOR__PINNED_BOOST` | Extra score for a `pinned` tag, so a decision you made beats the decay curve. | MRR |
| Per-source cap | `ENGRAM_EVAL_CAP` (a number, or `none`) | Chunks one document may contribute. Default 3; `none` lets one document fill the list. Raising it usually lifts recall and costs diversity. | recall@10 |
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
| `/ui/judge` | Where pairs come from. Its counter is not a stand-in for the measurement — it *is* recall@10 and MRR over the positions those searches actually gave. |
