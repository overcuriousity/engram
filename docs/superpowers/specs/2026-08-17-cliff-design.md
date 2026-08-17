# The cliff, shown

Roadmap: [Retrieval] — "Hybrid scores mean nothing across queries, but the gap
between one hit and the next does. Where the relevance falls off, the rail says
so — a break, or hits past it greyed — so a page of ten results does not claim
ten answers. Same computation ask uses to stop packing."

## What it is

A ranked list is presented as if every position were an answer. It is not: past
some point the hits are what retrieval returned because it had to return
something. `weak` catches the case where *nothing* matched closely — cosine
below a threshold — but says nothing about a good list with a bad tail, which
is the ordinary shape.

The cliff is the one step in the ranked scores that accounts for more of the
fall than the rest of the list put together. Everything after it is
`past_cliff`. Nothing is reordered and nothing is dropped: an exact match is
never buried, and a result past the cliff is still on the page — greyed and
under a rule, so the page states what the search knows.

## The computation

`pub fn cliff(scores: &[f32]) -> Option<usize>` in `src/core/search.rs`. Pure,
so `ask` can call it later to stop packing at the same place.

- Scores are read in list order, and each gap is `max(0, s[i] - s[i+1])`.
  Priming may lift a near-tie past its neighbour; a negative gap is a near-tie,
  not a cliff.
- With fewer than three hits there is one gap and nothing to compare it to:
  `None`.
- Let `g` be the largest gap and `m` the mean of the others. The cliff sits
  after position `i` of `g` when `g > 0` and `g > CLIFF_FACTOR × m` (also when
  `m == 0`: a plateau followed by a drop is a cliff). Returns `Some(i + 1)`, the
  number of hits above it.
- `CLIFF_FACTOR = 3.0`. Not configuration — the roadmap's rule is that a
  default moves after the harness has run, and until then a constant with the
  reasoning beside it is more honest than a knob.

Why this and not a threshold on the score: RRF scores, reranker logits and the
recency-formula stage all live on different scales, and a threshold set for one
is nonsense for the others. A step compared against its own list's other steps
is scale-free. Why "largest gap vs. mean of the rest" and not "largest gap vs.
total spread": with three hits a single step is always ≥ 50 % of the spread, so
a spread-fraction rule fires on almost every short list.

What it looks like on the scores engram actually produces:

- Hybrid RRF, two hits present in both branches then dense-only: gaps
  ≈ [.0005, .016, .0003, …] → cliff after 2.
- Dense-only RRF, evenly falling `1/(60+r)`: gaps all within a few percent of
  each other → no cliff.
- Reranker `[0.95, 0.90, 0.30, 0.28, 0.10]` → cliff after 2.
- `[0.9, 0.5, 0.1, 0.05]`: largest gap 0.4 vs mean 0.225 → no cliff. Ambiguous
  lists say nothing, by design.

## Where it is applied

In `Core::search_with`, on the truncated ranked list — the hits the caller will
actually see, in their final order — before associated hits are appended
(those are not ranked against the query and never take part). Sets
`SearchResult::past_cliff` on every hit from the cliff on. Serialised as
`past_cliff: true` only when set, like `weak` and `primed`; JSON clients see
nothing new otherwise.

## Where it is shown

- **UI rail** (`_results.html`): a rule "Relevance falls off here" before the
  first past-cliff row, and those rows greyed (`.rail-past`). Ranks stay: a
  hit past the cliff still competed and still placed; the claim being withdrawn
  is "this is an answer", not "this is fifth".
- **MCP** (`format_search_results`): the meta line of a past-cliff hit gains
  "below the relevance cliff", because an agent reading a numbered list has no
  grey to see.
- **API**: the flag on the result.
- **Ask**: not yet. The retrieval loop of the [Ask] list packs to the cliff, and that
  changes answers; it waits for the ask harness (item 1) to exist. The function
  is where it will need it.

## Tests

- `cliff()`: the four score shapes above; the < 3 case; the plateau case;
  negative gaps ignored.
- `search_with` marks the tail past a cliff and never the head; associated hits
  never carry the flag; a list without a cliff carries none.
- Rail renders the rule exactly once and only when there is a cliff; MCP output
  names it.
