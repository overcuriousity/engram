# Associative memory — Design

Date: 2026-08-16
Status: draft
Adds `src/jobs/associate.rs`, `src/store/links.rs`; touches `src/core/search.rs`,
`src/web/ui.rs`, `ROADMAP.md`. Restates one guiding constraint. See §3.

## 1. Why

engram is a memory. From here on it is designed as an *expansion* of a
biological one: it keeps the one capability the brain lacks — verbatim recall
with provenance — and borrows the brain's mechanisms for everything that decides
how a memory is reached.

Today every relation engram knows is **semantic**: cosine between embeddings,
read as nearest neighbours in the detail pane and as duplicate candidates in
consolidation. That is one of the two association systems a brain runs. The
other binds things that were *used together*, however far apart they sit in
meaning: the config passage and the troubleshooting passage for one subsystem
are strangers to the embedding and inseparable to the person who needed both to
answer one question. Complementary Learning Systems names the split — slow
semantic proximity in cortex, fast episodic binding in hippocampus — and
engram has only the cortex.

The hippocampal signal is already being written. With `feedback.enabled`, every
search stores its ranked candidates (`search_events`, `search_candidates`), and
`/ui/judge` turns some of them into confirmed answers. Nothing reads that log
for anything but evaluation. It is a record of what fired together, and it is
not being wired together.

Also missing is **activation**: a brain's memory has a current accessibility —
recently and often recalled things come first — and engram's has none. The
vector payload keeps `hit_count` and `last_seen_at`, but they inform the stale
list and `resurface`, not retrieval, and they do not decay.

## 2. Goal

Two associative mechanisms, both learned from use, neither touching what is
stored:

1. **Hebbian links.** Artifacts that keep appearing together in the same
   search grow a link; links that stop being used fade and are pruned. Each
   link remembers the queries that bound it. Strong links between different
   corpora earn one model call that names the relation, or hands a
   disguised duplicate to consolidation.
2. **Activation.** Each artifact carries a decaying accessibility, raised by
   being captured, retrieved, opened and confirmed.

Both show up in exactly two places the reader already uses: the **search
results** — associated artifacts recalled beside the ranked hits, and a bounded,
visible priming of the order — and the **detail pane** — a "seen together" list
with the binding queries, cross-corpus links emphasised.

### Non-goals

- **A graph view.** The association graph is a data structure, not a screen.
  The search box stays the application. The corpus map on the roadmap is
  unaffected and not part of this.
- **Rewriting any artifact from search.** Reconsolidation of content is what
  gives brains false memories; the trace stays fixed. See §3.
- **Ranking by popularity.** Activation may prime, within a hard bound, and it
  is shown when it does. It cannot bury an exact match.
- **Any inference in the query path.** Unchanged. The judge runs in the
  background, on few links, through the existing queue and pacing.
- **The sleep cycle, session priming, access reconsolidation, error-driven
  re-synthesis.** Named follow-ons; see `ROADMAP.md` [Associative Memory].

## 3. The constraint this restates

`ROADMAP.md` states: *Fidelity outranks convenience — a paraphrase or synthetic
summary must never silently replace or outrank the original wording.* The
consolidation design lifted the "replace" half narrowly. This design restates
the whole principle so both halves say what they were protecting:

> **The trace is fixed; access is plastic.** Content is verbatim and never
> changes silently. Everything about how it is found — associations,
> activation, what surfaces first — learns from use, within visible bounds.

Three guards keep "plastic" from becoming "unreliable":

1. **Bounded.** Priming moves a hit at most `prime_lift` positions and never
   displaces rank 1. Association adds hits beside the ranked list; it never
   removes or reorders one.
2. **Visible.** A primed hit says so; an associated hit says which hit recalled
   it. Nothing about the order is silent.
3. **One-way.** Associated hits do not feed the learning that produced them
   (§5.2). Priming does not raise activation by more than an ordinary
   retrieval would. Loops that reinforce themselves are the failure mode of a
   Hebbian system, and both are closed by construction.

What remains true and is not weakened: retrieval returns whole artifacts, never
generated prose; a captured artifact is never rewritten in place; nothing is
deleted on a score.

## 4. Data model

### 4.1 Links

```sql
CREATE TABLE IF NOT EXISTS artifact_links (
  a_id        TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  b_id        TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Strength as of `bumped_at`. Read through decay (§5.3); never decayed
  -- in place, so learning is one UPDATE and forgetting costs no writes.
  weight      REAL NOT NULL,
  bumped_at   INTEGER NOT NULL,
  -- Distinct normalised query texts that bound this pair. What separates a
  -- link from one search typed twice.
  queries     INTEGER NOT NULL DEFAULT 1,
  -- Up to three binding queries with counts, JSON: [{"q":..,"n":..}].
  -- The link's own explanation, free.
  cues        TEXT NOT NULL DEFAULT '[]',
  -- 'learning' | 'related' | 'unrelated' | 'dismissed'
  state       TEXT NOT NULL DEFAULT 'learning',
  -- The judge's one line, for `related`.
  reason      TEXT,
  -- Revisions the judge read. A re-embed of either side reopens the
  -- verdict: the text changed under it.
  judged_rev_a INTEGER,
  judged_rev_b INTEGER,
  judge_attempts INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (a_id, b_id),
  CHECK (a_id < b_id)
);
CREATE INDEX IF NOT EXISTS idx_links_b ON artifact_links(b_id);
CREATE INDEX IF NOT EXISTS idx_links_state ON artifact_links(state, weight DESC);
```

Its own table, not new `artifact_pairs` states. A pair can be both — filed by
`Relate` at 0.89 and judged distinct, *and* co-retrieved and related — and one
row cannot hold two verdicts. Every dedupe query would also have to start
excluding link states. Separate concerns, separate rows.

`a_id < b_id` makes the pair canonical, so a lookup by either side is two
indexed reads and there is no "which way round" bug.

### 4.2 Activation

```sql
-- artifacts
activation    REAL    NOT NULL DEFAULT 1.0,
activated_at  INTEGER NOT NULL DEFAULT 0
```

Same lazy shape as links: a value and its stamp, decayed on read. `ADD COLUMN`
with defaults; the migration backfills `activated_at = created_at`. In SQLite
rather than the vector payload because the query path already needs one SQLite
read for links (§6.2), and the same read returns activation — one crossing,
not two. Qdrant's `hit_count`/`last_seen_at` keep doing what they do.

### 4.3 Watermarks

```sql
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

Two keys: `associate.events_after` (last `search_events.created_at` replayed)
and `associate.judged_after` (last `judged_at` replayed). Nothing else uses
the table yet; it exists because these two cursors have no row to live on.

## 5. Learning

### 5.1 The sweep

```
Stage::Associate, target_kind = "collection"
```

Its own ticker (`associate.interval_mins`, default 30), the pattern of
`spawn_retention_ticker`. Each run:

1. **Replay new events.** `search_events` with `created_at` past the watermark
   and older than `feedback.coalesce_secs` — a folding event is still moving.
   Per event, every pair of `shown` candidates gets `weight += 1`; the pair's
   `queries` and `cues` are updated with the normalised query text (lowercased,
   whitespace collapsed) if it is new to that link. Advance the watermark.
2. **Replay new verdicts.** Events with `judged_at` past the second watermark
   and `verdict = 'hit'`: every shown pair containing `expect_id` gets a further
   `weight += 2`. Confirmation is the strong signal; co-appearance is the weak
   one. Advance the watermark. (`gap` and `discard` teach nothing about pairs.)
3. **Bump activation.** For each replayed hit verdict, `expect_id` gains
   `activation.confirmed`. Retrievals and opens are bumped where they happen
   (§5.4), not here.
4. **Prune.** Delete `learning` links whose decayed weight is under
   `associate.prune_below`. Judged links are never pruned by decay: a verified
   relation is about content, not use.
5. **Reopen.** For `related` and `unrelated` links where either side's
   `embed_rev` differs from the judged revision: `state = 'learning'`, reason
   cleared. The judge read text that no longer exists.
6. **Arm the judge.** Links with `state = 'learning'`, decayed weight ≥
   `judge_min`, `queries ≥ judge_min_queries`, both sides active, and
   `corpus_id` differing (a merged artifact, `corpus_id NULL`, always counts
   as differing) — arm one `Stage::LinkJudge` per link, up to
   `associate.judge_per_sweep`. Same-corpus links are shown, never judged: two
   passages of one document being related is not information.

Cost: pure SQLite. With ten shown candidates an event contributes 45 upserts.
One operator's day of searching is a few thousand rows of work.

### 5.2 What is not learned from

- **Associated hits.** The candidates a search records are the ranked ones. An
  artifact recalled by association (§6.2) is not written to
  `search_candidates` and does not bump edges. Otherwise a link would recall an
  artifact, be strengthened by having done so, and recall it harder next time.
- **Coalesced keystrokes.** Already handled: a typing burst is one event.
- **Doors** — all captured doors count. A query from Claude through MCP is a
  use.
- **Unshown candidates.** The stored pool is wider than the answer for
  evaluation's sake; only what the person (or agent) actually saw fires
  together.

### 5.3 Decay

Effective weight at time *t*: `weight · 2^(-(t - bumped_at) / half_life)` with
`associate.half_life_days` (30). Bumping folds the decay in:
`weight = effective(now) + Δ; bumped_at = now`. Activation uses the same rule
with `activation.half_life_days` (14).

Lazy decay is what makes forgetting free: no sweep walks every row to subtract
from it. The only write on a quiet link is its eventual deletion.

### 5.4 Activation bumps

| Event | Δ | Where |
|---|---|---|
| captured | starts at 1.0 | insert |
| returned by a search (`query.mark`) | `activation.retrieved` (1.0) | `mark_seen`, same background task that touches the payload |
| opened in the detail pane | `activation.opened` (0.5) | `mark_artifact_seen` |
| judged the answer | `activation.confirmed` (3.0) | sweep step 3 |
| recalled by association or shown by `resurface` | 0 | — |

The last row is the one-way guard from §3: being surfaced *because* of
activation or links does not raise activation.

## 6. Retrieval

### 6.1 Priming

After ranking and the per-corpus cap, before truncation to `limit`, one SQLite
read fetches `(activation, activated_at)` for the candidate ids. Each hit gets a
normalised activation `a ∈ [0, 1]` — its decayed value over the maximum in this
list. A hit may then rise past a neighbour above it when its `a` exceeds the
neighbour's by `associate.prime_margin` (0.5), at most `prime_lift` (2)
positions, and never past rank 1. A hit that moved carries `primed: true`.

Rank-based rather than score-based on purpose: hybrid scores are fused ranks
and mean nothing across queries, while "moved up two places" means the same
thing every time and can be tested with a table.

### 6.2 Association

For the top `associate.spread_from` (3) ranked hits, fetch their links with
`state IN ('learning', 'related')` and decayed weight ≥ `associate.show_min`
(2.0), strongest first, whose other side is active and not already in the
result list. Up to `associate.spread_max` (3) are appended after the ranked
hits, outside `limit`, each carrying `via: <artifact_id>` and, for a judged
link, `reason`. They are `Touch::shown`, not retrieved (§5.4), and not
recorded as candidates (§5.2).

One hop only. Spreading further is what a graph view would be for, and there
is none.

### 6.3 Failure

The SQLite read fails → no priming, no association, results as today, one
warning. The associative layer can only add.

## 7. The judge

```
Stage::LinkJudge, target_kind = "link", target = "<a_id>|<b_id>"
```

One call, both artifacts, the binding queries as context. It answers one of:

- `related` — with one line: what the relation is, in the reader's terms. The
  line is stored and shown; the link stops decaying.
- `unrelated` — a coincidence of retrieval. Stored so it is not asked again;
  hidden from the detail pane; reopened only if either text changes.
- `duplicate` — the two say the same thing and the embedding failed to notice.
  Filed as a `Pending` row in `artifact_pairs` with `detail = 'link'`, and
  consolidation takes it from there under its own guards; the link is marked
  `related` with the reason "same content; handed to consolidation" so the
  reader still sees the connection while dedupe decides.

An unreadable answer counts against `judge_attempts`; at three the link is set
`unrelated` with reason `unreadable` — shelved, not retried forever, and
reopened by the same re-embed rule. A dead endpoint leaves the unit queued at
the backoff ceiling like every other unit, and the link stays visible as
`learning` in the meantime.

Prompt in `src/infer/prompt.rs` beside the dedupe prompt; parsing in
`src/jobs/associate.rs`.

## 8. Surface

**Search results** (`SearchResult`): `primed: bool` (skipped when false),
`via: Option<String>`, `reason: Option<String>`. The web UI renders associated
hits below the ranked list under a rule reading *recalled by association*, each
row naming what recalled it; a primed hit gets a small marker. API and MCP get
the fields and nothing else.

**Detail pane and artifact page**: a *Seen together* section beside the
existing nearest neighbours: linked artifacts with `state IN ('learning',
'related')` and decayed weight ≥ `show_min`, strongest first, capped at
`RELATED_LIMIT`. Each row: title, the judge's reason where there is one,
otherwise the top binding query as *when asking: …*, and the corpus title.
Cross-corpus rows render emphasised; same-corpus rows muted. A row has a
dismiss control (`state = 'dismissed'`: never shown, never judged, never
pruned; the row keeps learning weight so the decision is auditable, but the
state is final for that pair — undo from Ops is out of scope).

Superseded or deprecated endpoints are filtered at read time, so undoing a
merge brings its links back without a write.

**Ops**: three numbers — links, of which related, judge queue — in the
existing stats block. No new page.

## 9. Configuration

```toml
[associate]
enabled = true                # requires feedback.enabled; logs and stays off otherwise
interval_mins = 30
half_life_days = 30
prune_below = 0.5
show_min = 2.0
judge_min = 4.0
judge_min_queries = 3
judge_per_sweep = 10
spread_from = 3
spread_max = 3
prime_margin = 0.5
prime_lift = 2

[activation]
half_life_days = 14
retrieved = 1.0
opened = 0.5
confirmed = 3.0
```

`associate.enabled` without `feedback.enabled` is a warning at startup and
nothing else: there is nothing to learn from. `feedback` keeps its own switch
because recording queries is a privacy decision the operator makes separately.

## 10. Testing

Store: link upsert folds decay; canonical order; prune spares judged rows;
reopen on `embed_rev` change; watermarks advance only past settled events.

Sweep: two events with the same shown pair → weight 2, `queries` 1 if the text
matched, 2 if not; a hit verdict adds 2 to pairs containing `expect_id` and
raises its activation; a same-corpus link is never armed; a strong cross-corpus
link is armed once.

Retrieval: priming moves at most `prime_lift`, never past rank 1, marks what
moved; association appends at most `spread_max` outside `limit`, excludes
artifacts already ranked and inactive endpoints, marks `via`; associated hits
do not appear in `search_candidates` and do not bump activation; a SQLite
failure leaves the ranked list byte-identical.

Judge: each verdict's state transition; `duplicate` files a pending pair;
unreadable ×3 shelves; reopened link is re-armed.

The eval harness is unaffected by design (association is outside `limit`;
priming is bounded) — and it is how `prime_margin`/`prime_lift` get tuned:
run with priming off and on against the frozen corpus before either default
moves.

## 11. Rollout

Additive schema (`ADD COLUMN`, two new tables). Feature off until
`feedback.enabled`; existing installs see nothing change until they opt in.
No backfill of links from the historical log — the watermarks start at the
first sweep, and the log is a record, not a curriculum. (An operator who wants
the history replayed sets `associate.events_after = 0` in `meta`; documented,
not automated.)

## 12. Risks

- **The layer crossing.** One indexed SQLite read joins the query path. It is
  bounded (ids in hand, one statement), optional (§6.3), and it is the price
  of any usage-derived signal — Qdrant cannot hold a graph. Measured before
  merge: `total_ms` with and without.
- **Cue leakage.** Binding queries are the operator's own words and are shown
  back only to the operator; API/MCP results carry `reason`, not `cues`.
- **Judge cost creep.** Bounded by `judge_per_sweep` and by the count of
  distinct cross-corpus question shapes, not by search volume. Expected:
  single digits a day, often zero.
- **Priming that annoys.** Two places, visible marker, and off by setting
  `prime_lift = 0`. If eval shows it costs precision it is a one-line default.
