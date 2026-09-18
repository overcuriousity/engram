# Evaluation

Ranking has knobs — fusion, the per-source cap, recency weight, reranking,
priming — and hand-testing cannot judge any of them, because the queries anyone
thinks to type reuse words they remember from the passage they are looking for.
A knob change either moves a number or it is a preference. The rule this
repository holds itself to:

> A default that changes ranking moves only against evidence, and the commit
> that moves it says what the evidence was.

There used to be an offline harness beside the runtime loop: a frozen export of
the base, replayed under `cargo test` with the numbers printed for a commit
message. It is gone. It opened nothing — `mark: false`, no confirmations, no
citations — so every artifact sat at its capture baseline, engagement was zero
across the corpus, and every knob that reads engagement returned the same
numbers at every rung. Priming is the clearest case, and priming is the knob
that ships on. A rule pointing at an instrument that is structurally blind to
the knob is worse than no rule, because it reads like a guarantee.

What is left is the runtime loop, and it is not blind: it replays the `Priming`
a real search recorded — activation, sitting and due — so it asks what a rung
would have done to a list somebody actually used. The trade is that its figures
are per base and private. There is no number comparable across months, and no
number to put in a commit message. Where a shipped default moves, the commit
says that, and says what stands in for a number: measured by the loop, per
base, revertible.

---

## 1. What is measured

Two numbers, over your own searches, both read off `/ui/insights`.

| Metric | Question it answers | Read it when |
|---|---|---|
| **recall@10** | Was the answer in the first ten results at all? | Changing what is *retrieved*: embeddings, fusion, the candidate pool, the per-source cap. |
| **MRR** | How far down was it? | Changing what is *ordered*: recency, pinning, reranking, priming. |

Both, never one. A change can lift recall and hurt MRR — a wider candidate pool
finds more and buries it deeper — and which of those matters is a judgement
about what a search page is for. `mrr` counts a miss as zero rather than
excluding it: a ranking that answers one query perfectly and fails nineteen must
not be able to report a perfect score.

---

## 2. Where the evidence comes from

Nothing here is fixtures. Under a result opened from the rail there is a bar —
*Was this what you were looking for? Yes · No · Not sure* — and on a rail that
matched nothing there is *Nothing here has it*, which records a gap. Every
judged pair comes from somebody pressing one of those. A long read does not
count as a *Yes*: what it measured was a pane left open, which is an abandoned
tab about as often as it is an answer. *Not sure* leaves the search pending. A
verdict is given at the moment of the search, against the list the search
actually gave, by the person who made it. Position bias remains: a person is
likelier to confirm what came first, so recall@10 and MRR read off these
verdicts lean slightly towards the ranker.

The idle pass replays those verdicts beside what use left behind: an excerpt
an answer actually drew on, and a result somebody opened. All three are the
same claim — this query was answered by that artifact — and a verdict that
confirms the very open it was given under is counted once. Only positive
evidence enters, and only under the live generation: evidence gathered while
a different embedding or chat model was configured belongs to another era and
stops counting.

With `evolve.autonomous` on, that evidence feeds an idle pass that adopts
settings on its own. It walks the ladders in
`src/core/ranking.rs` — recency weight, per-source cap, candidate pool depth,
recency half-life, the priming lift, and the sitting flip above a non-zero lift
— one knob at a time, replays only stored query vectors through the live index,
and stops when anybody comes back. The reranker flip and the associative band
move beside it on their own rules, against different baselines, because their
promises are not comparable with a ladder row's.

What checks a move the base made is the probe anchor (`src/eval/rehearsed.rs`),
which replays the base's own probes under the candidate and the running
configuration and refuses one that loses; the lived watch
(`src/eval/lived.rs`), which reverts a generation whose observations get worse
than its parent's; and the verdict anchor (`src/eval/anchor.rs`), which
suspends adoption entirely when the base's own evidence stops agreeing with the
people using it. Three instruments, all inside the base, none of them frozen.

A grade is satisfied by anything that **superseded** the artifact it names.
Merging moves knowledge into a new artifact and search correctly returns that
one; scoring only the original would report a retrieval regression that is
really a bookkeeping change — exactly when it matters most.

---

## 3. The knobs

Every ranking setting comes from configuration. The ones on a ladder are moved
by the idle pass; the rest move by hand, on an argument the commit states.

| Knob | Where | What it moves | Watch |
|---|---|---|---|
| Recency weight *(ladder)* | `[vector] recency_weight` | How much age counts against a hit. `0.0` turns it off. Fused ranks sit between ~0.1 and 1.0, so the default breaks near-ties without overturning a clear match. | MRR |
| Recency half-life *(ladder)* | `[vector] recency_half_life_days` | Age at which a hit has lost half that boost. | MRR |
| Per-source cap *(ladder)* | `[vector] per_source_cap` | Chunks one document may contribute. Default 3; `0` lets one document fill the list. Raising it usually lifts recall and costs diversity. | recall@10 |
| Candidate pool *(ladder)* | `[vector] candidate_multiplier` | How many times the answer size retrieval fetches for the cap or the reranker to narrow. | recall@10 |
| Priming lift *(ladder)* | `[associate] prime_lift` | How many places an accessible hit may climb. `0` turns priming off; it ships at `1`. | MRR |
| Sitting priming *(ladder)* | `[sitting] prime` | Whether what this sitting has touched takes part in the lift. Shares `prime_lift`'s budget, so it does nothing at `0`. | MRR |
| Reranker *(own rule)* | `[infer.rerank]` present or absent | A cross-encoder over the candidate pool. Scored against the rank that was served. | MRR first, recall second |
| Associated band *(own rule)* | `[associate] spread_max` | How many linked artifacts hang under the list. Scored on what the band earned while serving. | neither; read the band's use |
| Priming margin | `[associate] prime_margin` | How much more accessible a hit has to be before it climbs. On no ladder and measured by nothing. Change it on an argument, and say the argument. | nothing |
| Pinned boost | `[vector] pinned_boost` | Extra score for a `pinned` tag, so a decision you made beats the decay curve. Measured by nothing. | nothing |
| Embedding model and templates | `[infer.embed]` | The whole retrieval geometry. Needs `--reindex`. No runtime sweep can reach it; a change is an era, and the loop stops counting evidence from the one before. | nothing |
| Weak threshold | `[vector] weak_below` | Similarity under which a hit is labelled *loose*. Changes no order — it changes what the page claims, and what becomes an `unmatched` knowledge gap. | neither; read the page |

---

## 4. Tuning at runtime

The ranking tunes itself, and nothing else tunes it. Once the base has been
quiet for `evolve.idle_secs`, the idle pass replays the live generation's
evidence — every answer confirmed on the bar and every positive observation
use left behind, up to five hundred of each — under the neighbouring rungs of
every ladder, against the live index. It needs no export, no frozen corpus and
no re-embedding: every pair carries the vector its query was searched with, so
a whole ladder is seconds of vector reads. It reads and never records —
`Door::Judge`, `mark: false` — and it takes the background lane rather than
the interactive one, stopping between pairs the moment anybody comes back.

Its figures are a **replay**, not the page's. The Retrieval measure on
`/ui/insights` is recall@10 and MRR over the positions the searches actually
gave; a pass's are those searches run again, now, under each setting, through
a door that leaves priming out except where the search recorded it. Both are
honest and neither substitutes for the other.

A candidate is adopted only when **at least two pairs are net better, neither
aggregate is worse, and at least ten pairs were replayed**. That floor is the
whole safety of running it automatically: on fifty pairs a single flipped pair
is two points of recall, and an aggregate delta alone cannot tell one from a
real improvement. Ties keep the current values. What clears the gate is then
replayed on the base's own probes and refused if it loses there; what is
adopted is watched while it serves and reverted when it does not hold.

Every pass is recorded in `eval_runs` with the settings that produced it and
the pairs that moved, quiet or not, and the generation it adopted or refused
names that row. That is the rule at the top of this file about never writing
a number without its configuration, made structural rather than asked for.
`config.toml` is never written: the file is the operator's starting point, and
`/ui/insights` says which generation is live.

What this does **not** cover: the embedding model and its templates — they
change the vector geometry rather than the order over it, and no runtime
replay can reach them — pinning, and the ask side. Those are measured by
nothing. Say so in a commit that moves one rather than implying a number that
was never taken.

---

## 5. What it cannot measure

Being clear about this matters more than the numbers, because the temptation is
to run *something* and call the question answered.

- **A run as a run.** The idle pass replays through `Door::Judge` with the
  `Priming` the original search recorded — activation, sitting and due —
  handed back by `Origin::primed_as`. Every search records what the sitting
  held whether or not `[sitting] prime` is on, which is what makes the flip
  measurable at all. What has no instrument is anything reading
  `Origin::session` that the recorded `Priming` does not carry — working
  memory, and the shape of a sequence of queries as a sequence.
- **Anything across months.** Every figure is a replay over one base's own
  searches under that base's own settings. Two bases are not comparable, and
  one base before and after a change of embedder is two eras the loop refuses
  to add up. A commit that moves a default carries no aggregate, because none
  can be taken.
- **The ask side.** Citation recall, abstention and faithfulness have no
  instrument since the harness went. The badge on an unsupported literal is a
  guard at the moment of answering, not a measurement over time.
- **Whether an artifact is any good.** Recall and MRR measure whether the
  right artifact was *found*, never whether it was worth finding.
- **A base too small to have an opinion.** Under twenty pairs the arithmetic
  works and the result means nothing.

---

## 6. Where the pieces live

| Path | What |
|---|---|
| `src/eval/metrics.rs` | `recall_at` and `mrr`. |
| `src/eval/sweep.rs` | The pairs, the gate and the candidate chooser the idle pass replays with. See 4. |
| `src/core/ranking.rs` | The ladders the idle pass may walk, priming among them. |
| `src/eval/lived.rs` | What a generation earned while it was serving, against the one it replaced. No replay. |
| `src/eval/rehearsed.rs` | A candidate scored on the base's own probes. Refuses and reverts; never adopts. |
| `src/eval/anchor.rs` | Whether the self-generated evidence still agrees with human verdicts. Suspends the loop when it stops. |
| `src/store/generations.rs` | The named settings a base retrieves under, and what it adopted, reverted or refused. |
| `src/store/eval_runs.rs` | Every pass, with the settings that produced it and the pairs that moved. |
| `/ui/insights` | Where the base says which generation is live and what it adopted, reverted or refused. The Retrieval measure is not a stand-in for the measurement — it *is* recall@10 and MRR over the positions those searches actually gave. The pairs themselves come from the verdict bars under results and from use. |
