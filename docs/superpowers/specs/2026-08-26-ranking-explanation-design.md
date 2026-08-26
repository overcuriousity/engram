# Why this hit is where it is — Design

Date: 2026-08-26
Status: draft
Adds `src/core/explain.rs`; touches `src/core/search.rs`, `src/core/mod.rs`,
`src/mcp/mod.rs`, `src/web/api.rs`, `src/web/ui.rs`,
`src/web/templates/_results.html`, `src/vector/qdrant.rs`,
`tests/integration_qdrant.rs`, `ROADMAP.md`.
No new store table, no migration, no model call, no second vector search.
Nothing about the order of results changes — §8 says how that is pinned.

## 1. Why

A rank is the product of eight stages layered in the order they were built.
Each says what it did in its own way or not at all: the rail badges some of it,
MCP's meta line a different some, the API a third. `ROADMAP.md:498` names this
and names the price — it is the only honest way to keep adding stages to a
ranking the operator is asked to trust, and every ranking item still on that
list adds one.

There is a second, narrower reason to build it now. `cap_per_corpus` is applied
client-side over a candidate pool of `limit * CANDIDATE_MULTIPLIER`
(`src/core/search.rs:12`, the multiplier is 3). When one corpus fills that pool
the cap has nothing left to redistribute: it displaces hits and then refills
from exactly what it displaced. The list is then dominated by one document
*despite* a configured `per_source_cap`, and nothing anywhere says so.
`ROADMAP.md:460` calls the fix — server-side grouping — a correctness ceiling
rather than an improvement, and gates it on a measurement nobody can currently
take.

The tuning sweep cannot take it either, and it is worth saying why, because a
run of quiet sweeps reads as evidence that the cap is fine. Its grid walks
`CAPS = [Some(2), Some(3), Some(5), None]` (`src/eval/sweep.rs:33`). When the
pool is saturated all four produce the same list, so the axis is flat — and a
flat axis is the *signature* of the defect, not its absence. Its metric cannot
see the failure either: recall@10 and MRR are measured against the expected
artifact of a judged pair, and what saturation costs is the relevant artifact
in the *other* corpus, which never surfaced, was therefore never judged, and
cannot appear in `judged_pairs`. The failure removes the evidence that would
measure it.

An explanation that names what the cap did closes both gaps with one object.

## 2. What is built

1. **A `HitExplanation` on every result**, carrying what each stage did to that
   hit.
2. **A `SearchExplanation` beside `SearchTiming`**, carrying what cannot belong
   to a hit — how wide the pool was, how many corpora it held, and how many
   hits the cap displaced and refilled.
3. **Reconstruction of the three stages that run inside Qdrant**, from payload
   fields already fetched, at no additional query cost (§5).
4. **One rendering, read by all three doors** — the rail, MCP's meta line, the
   API (§7).

Computed always, rendered on request (§8).

## 3. The pipeline, in the order it runs

Written down because it is currently only discoverable by reading
`search_inner` end to end, and because the stage order is not the order the
`ROADMAP` lists.

| # | Stage | Where | Observable? |
|---|---|---|---|
| 1 | RRF fusion of the dense and sparse prefetch branches | Qdrant (`src/vector/qdrant.rs:1613`) | rank only |
| 2 | Recency decay | Qdrant, in `scoring_formula` (`src/vector/qdrant.rs:328`) | reconstructed |
| 3 | Pinned boost | Qdrant, same formula | reconstructed |
| 4 | `cap_per_corpus` | engram (`src/core/search.rs:1042`) | directly |
| 5 | Reranker | engram (`src/core/search.rs:1089`) | directly |
| 6 | `prime` — activation and sitting | engram (`src/core/search.rs:1147`) | directly |
| 7 | `mark_past_cliff` | engram (`src/core/search.rs:1203`) | directly |
| 8 | One-hop association | engram (`src/core/search.rs:1222`) | directly |

Two things follow from the table and both shape the design.

The cap runs **before** the reranker, not after. It is applied in vector order
so that what leads per source is that source's best, and the reranker then
reorders whatever survived. An explanation that implied the reverse would be
worse than none.

Stage 8 appends hits that never competed for a place. They already carry `via`
and `reason` and get no rank number at any door. They keep that treatment: an
associated hit's explanation says it was recalled, and nothing else, because
there is no ranking story to tell about a hit that was not ranked.

## 4. The two objects

The split is forced by the question, not chosen. "How many corpora did the pool
hold" is a property of the search; no hit can answer it.

```rust
/// What one stage did to one hit. `None` where the stage did not apply.
pub struct StageEffect {
    /// Rank before the stage, where the stage reorders.
    pub from: Option<usize>,
    /// Rank after it.
    pub to: Option<usize>,
    /// Score contribution, where the stage is additive.
    pub delta: Option<f32>,
}

pub struct HitExplanation {
    /// Rank as retrieval returned it — fusion *and* the scoring stage, since
    /// Qdrant applies both before anything comes back.
    pub retrieved_rank: usize,
    pub recency: Option<f32>,
    pub pinned: Option<f32>,
    pub rerank: Option<StageEffect>,
    pub cap: CapEffect,
    pub prime: Option<StageEffect>,
    pub past_cliff: bool,
    /// Set for a hit stage 8 appended. Every other field is then absent.
    pub recalled_via: Option<String>,
}

/// What the diversity rule did to this hit.
pub enum CapEffect {
    NotApplied,
    Kept,
    /// Over its cap in one corpus, and put back only because the target was
    /// not reached — the case the cap silently fails in.
    Refilled,
}

pub struct SearchExplanation {
    pub candidates_fetched: usize,
    pub corpora_in_pool: usize,
    pub capped: Option<usize>,
    pub displaced: usize,
    pub refilled: usize,
    pub reranked: bool,
}
```

`retrieved_rank` has no `delta`, and the name is careful. The pre-recency RRF
rank is not obtainable without a second query, so the baseline this explanation
measures from is what retrieval *returned* — fusion and the scoring stage
together. Calling it a fused rank would claim a separation that costs a query
nobody is paying for. RRF fuses ranks in any case, so its output is an ordinal
with no score to attribute: a baseline does not need a difference against
itself.

`SearchExplanation` lives beside `SearchTiming` in the tuple `search_with`
already returns (`src/core/search.rs:877`). That is the existing channel for
facts about a search rather than about a hit, and `SearchTiming::reranked`
is already one of them — it is mirrored into `SearchExplanation` rather than
moved, so no caller of `SearchTiming` changes.

## 5. Reconstructing what Qdrant did

Stages 2 and 3 run inside Qdrant as one sum, and only the final score comes
back. Both are nonetheless computable locally from fields already in the
payload, at no additional query cost:

- `tags` (`src/vector/mod.rs:16`) carries `PINNED_TAG`, so the pinned term is
  `pinned_boost` when present and absent otherwise.
- `last_verified_at` (`src/vector/mod.rs:39`) is what the decay reads, and the
  formula is `exp_decay` with `midpoint: 0.5` and `scale: half_life_secs` —
  which is `0.5^((now - last_verified_at) / half_life_secs)`, multiplied by
  `recency_weight`. A point without the field defaults to `now`, giving `1.0`,
  exactly as `"defaults"` in the formula says.

Both read the same `RankingParams` that built the formula, from the same
`ranking` lock, so a runtime change to `recency_weight` cannot make the two
disagree.

**This is a re-implementation of another system's semantics, and that is the
one real risk in this design.** If Qdrant changes what `exp_decay` means, the
reconstruction lies — and an explanation that contradicts the ranking it
explains is worse than silence. A unit test against our own formula would only
pin our own belief. The contract is therefore tested in
`tests/integration_qdrant.rs`, against real Qdrant: score a set of points with
recency and pinning on, reconstruct the two terms locally, and assert the
reconstruction plus the fused base accounts for the returned score within a
tolerance. That test failing is the signal that the reconstruction has to be
withdrawn, and it fails loudly rather than drifting.

## 6. The cap stage, where the information is destroyed today

`cap_per_corpus` (`src/core/search.rs:203`) is a pure function returning a
`Vec`. It knows everything this design needs — which hits it displaced, which
it put back, how many corpora it saw — and discards all of it on return.

It grows a second return value carrying exactly that, and stays pure. Its
existing tests (`src/core/search.rs:1896` onwards) already pin its behaviour
including the merge case, where a hit counts against every entry of
`origin_corpora` but only when it took a place; those tests keep passing
unchanged and gain siblings for the new value.

One nuance the explanation must not blur. A hit put back by the refill is not
"kept": it is over its cap and present only because nothing else was available.
`CapEffect::Refilled` says that, and it is the value I will be counting when
the branch is deployed.

## 7. Three doors, one text

The item exists because the doors disagree. Building it a door at a time would
extend that disagreement for the length of the build, so all three land in this
branch.

**MCP.** `SearchParams` (`src/mcp/mod.rs:287`) gains `explain: Option<bool>`.
When set, `format_search_results` appends a per-hit block under the existing
meta line, and the response opens with one line from `SearchExplanation`. That
function is already a standalone, directly testable one (`src/mcp/mod.rs:16`),
and the reason is written down beside its sibling `format_answer`
(`src/mcp/mod.rs:501`): MCP is the door with no page, so an agent gets this
string and nothing else, and what it does or does not say is the whole of what
the caller knows.

**API.** `SearchResult` gains `explanation: Option<HitExplanation>`, serialised
only when present, like every other optional field on that struct. The search
handler (`src/web/api.rs:706`) returns `Json<Vec<SearchResult>>` today, which
has nowhere to put a search-level object. Under `?explain=1` — and only under
it — the response becomes `{"results": [...], "explanation": {...}}`. No
existing client sees the change, because no existing client passes the flag.

**Rail.** The compact form: the stages that moved this hit, on the row that
already carries the badges. The rail is a `listbox` whose rows are options
(`ROADMAP.md:448`), so this is text within the option, never a disclosure
inside it — an expander there breaks the arrow keys and the ARIA both.

## 8. Opt-in rendering, always-on computation

The computation is a handful of float operations and one pass over the pool.
Making it conditional would create a second code path through the ranking
stages, and the unexercised one would be the one that ships. So it always runs;
`explain` decides only whether anything is rendered.

`SearchQuery` gains `explain: bool`, defaulting to `false`.

**What must not change:** the order of results, with `explain` on or off, at
every door. This is the whole of what the branch promises not to break, and a
test asserts a byte-identical result order for the same query under both
values.

## 9. Testing

| What | Where |
|---|---|
| Reconstruction agrees with Qdrant's own scoring | `tests/integration_qdrant.rs` — the contract of §5 |
| Cap reports displacement, refill and corpus count | `src/core/search.rs` unit tests, beside the existing ones |
| A saturated pool yields `CapEffect::Refilled` | `src/core/search.rs` — the case the whole branch is for |
| Result order identical with and without `explain` | `src/core/search.rs` |
| An associated hit explains itself as recalled and nothing more | `src/core/search.rs` |
| The MCP meta line carries the block exactly when asked | `src/mcp/mod.rs` |
| `?explain=1` changes the envelope; the bare call does not | `src/web/api.rs` |

## 10. What this is not

**Not stored.** No column, no migration, no growth per search. An explanation
is computed for the request that asked for it and then gone. This was the
explicit decision: persisting it would let real traffic accumulate the
measurement on its own, and it was rejected as scope beyond the roadmap item.
The consequence is accepted and named here so it is not rediscovered later —
the corpus-concentration figure has to be gathered by deliberate searches
against a deployed instance, and each of those bumps activation, writes a
`search_event` and stamps `last_seen_at`, because `Door::Mcp` is `captured()`
(`src/store/feedback.rs:52`) and `search` sets `mark: true`. The probes will be
few and named in advance.

**Not a second query.** The reconstruction exists precisely so that a full
explanation costs no extra round trip. A reference query without
`scoring_formula` would be more exact and would double the cost of every
search, which `ROADMAP.md:59` forbids.

**Not a ranking change.** No harness run is owed, because nothing about the
order moves.

**Not OIDC.** `ROADMAP.md:703` — OAuth 2.1 for `/mcp` — is a separate branch
with its own spec. It touches authentication and belongs nowhere near this.

## 11. Cost

A branch. Several files across layers, tests of its own, no migration. The
integration test in §5 needs a live Qdrant, which `tests/integration_qdrant.rs`
already requires; nothing else here does.
