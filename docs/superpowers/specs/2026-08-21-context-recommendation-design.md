# A recommendation under the search box, from context — Design

Date: 2026-08-21
Status: draft
Adds `src/core/context.rs` (bundle, encoder, clock) and a sweep unit; touches
`src/store/schema.sql`, `src/vector/mod.rs`, `src/vector/qdrant.rs`,
`src/vector/memory.rs`, `src/web/ui.rs`, `src/config.rs`, `ROADMAP.md`.
Adds one named vector to the collection, two tables, one endpoint, and no model
call anywhere.
See §11 for what it is not allowed to break.

> **Amended 2026-08-26: the `scope` block is gone.** §6 called it an interim
> measure that would go to 0 once each user had their own collection, and
> `2026-08-24-multi-user-tenancy-design.md` gave them one. It was removed rather
> than zeroed, because a weight of 0 leaves the dimensions and the reasoning in
> place: the layout is ten blocks and 45 dimensions, `encode` takes no `scope`
> argument, and `context_score` is the cosine over the whole vector rather than
> over everything after the block. The rung thresholds are unchanged — the gate
> never read the block that went. Isolation is the exact cut in `Core::offer`,
> which §11 already named as the thing that guarantees it. The changed width
> means an existing collection needs `--reindex` before it will accept the new
> vectors; that copies the dense vectors and re-embeds nothing. Read §6 and §11
> below as the record of why the block existed, not as a description of the
> code.

## 1. Why

The area under the search box is empty, and the base knows something that could
fill it.

A person's use of a memory is not uniformly distributed over the week. The same
question recurs in the same situation: the recycling centre's opening hours,
looked up on the phone, on a Friday afternoon, before driving there. That
regularity is already recorded — `search_events` carries the query, its
embedding, `scope` and `created_at`; `interaction_events` carries what was
opened after it. Nothing reads either for this purpose, and nothing could:
`created_at` is a bare Unix integer with no timezone attached, so "Friday at
15:00" is not a question the base can currently ask of its own log.

So this is two things. A small one: give the application a working notion of
local time. A larger one: record the situation an interaction happened in, learn
which situations recur for which artifact, and offer that artifact before it is
asked for — with the reasons visible.

The constraint from the top of `ROADMAP.md` holds throughout: no model call and
no embedding at read time. The learning is a sweep; the read is one vector
query over a small subset.

## 2. What is built

1. A **signal bundle** collected in the browser on every page view — time zone,
   local time, viewport, device, locale, network, power, environment — recorded
   raw and in full.
2. A **context profile** per `(scope, artifact)`, built by a sweep: the past
   situations in which that artifact was opened, agglomerated into at most a
   handful of clusters, weighted and decayed.
3. Each cluster centroid stored as one element of a **multivector** named `ctx`
   on the artifact's existing Qdrant point, scored with `max_sim`.
4. A **query at page load**: the current bundle, encoded, against `ctx`. The
   best artifact is offered under the search box, with the blocks that decided
   it named beneath.
5. A **ladder**, so the area is never empty and never lies about why something
   is there.

### Why a vector and not a scoring function

Two other shapes were considered and rejected.

*Context bins* — count opens per (weekday × daypart × device) bucket. Readable
and cheap, but bins are hard: 14:55 and 15:05 fall in different ones, and a real
pattern reads as two weak ones. It also does not survive the number of
dimensions wanted here; crossing eight of them yields thousands of buckets
holding one event each.

*A weighted sum of named per-dimension terms* — no vector, score each candidate
by summing per-dimension similarity. The explanation falls out for free, but the
model is additive by construction: it cannot represent "on the phone the hour
matters, on the desktop it does not", and it needs one hand-set weight per
dimension, which does not scale to the number of signals a browser exposes.

The vector was chosen for the dimension count and for the conjunctions the
additive model cannot hold. Its known weakness — that the weights do not
disappear but hide in the encoding, where nobody can tune them — is answered in
§6 by normalising each block before scaling it, which puts the weights back into
config as named numbers.

## 3. Time

Small, deliberately.

- `chrono` and `chrono-tz` enter the tree. Storage stays Unix seconds; nothing
  in the existing schema changes format. `store::now()` stays.
- **The zone comes from the client**, not from config:
  `Intl.DateTimeFormat().resolvedOptions().timeZone` yields the device's IANA
  zone, which is correct per device and handles DST without an offset anywhere
  in the code.
- A `Clock` with `System` and `Fixed(i64)` variants, held where this feature
  needs it — the sweep, the encoder's entry point, the endpoint. The other
  `now()` call sites in the tree are **not** touched. They work; rewriting them
  is a diff across the tree for nothing.
- `artifacts` gains `updated_at`, bumped in the same UPDATE that bumps
  `embed_rev`. Unrelated to this feature, and the one place where the base
  cannot currently answer "when did this last change".

## 4. Capture

### Client

One script, one object, sent on page view: IANA time zone and offset, local
time, `language` and `languages`, viewport width/height and aspect,
screen dimensions, `devicePixelRatio`, colour scheme, platform and UA client
hints, `hardwareConcurrency`, `deviceMemory`, touch support, orientation,
battery level and charging state, network type where exposed, media-device
counts.

Not collected: canvas, WebGL, font enumeration, plugin lists. Not out of
squeamishness — they are the wrong tool. Those attributes are what identify a
device across a population; here the population is one person and the session is
already authenticated, so they are constant and carry no information about
*which situation* this is. They are also actively unstable in a hardened
browser: Brave randomises canvas, WebGL and the plugin list per session and
origin, so a device identity built on them would rotate and every day would look
like a new device.

### Server

```sql
CREATE TABLE context_events (
  id          INTEGER PRIMARY KEY,
  scope       TEXT,
  at          INTEGER NOT NULL,
  -- The whole bundle as received, including fields the encoder ignores.
  bundle      TEXT NOT NULL,
  -- Hash over the stable fields only: platform, UA family, screen dimensions,
  -- hardwareConcurrency, deviceMemory, language.
  device_key  TEXT,
  -- Denormalised because the sweep reads them on every row.
  local_hour  INTEGER,
  weekday     INTEGER,
  tz          TEXT
);
CREATE INDEX idx_context_scope_at ON context_events(scope, at);
```

Joined to `search_events` and `interaction_events` through `scope` and `at`,
never through a stored id — the same rule `interaction_events` already states
for pursuits (`schema.sql:461`).

The bundle is stored **whole**, including what the encoder does not read today.
That is what makes §6's versioning cheap: a new block is a reindex plus a sweep,
not the loss of history.

Retention: `context_events` does not hang off `feedback.retain_days`. A weekly
pattern needs weeks, and that key defaults to keeping forever but is an operator
switch. It gets its own window, longer.

## 5. The profile

### Sources

A sweep reads three, joined on `scope` and `at`:

- `interaction_events` where `kind = 'opened'` — the artifact was read;
- `context_events` — the situation it was read in;
- `search_events` + `search_candidates` as the **bridge**: where the open
  followed a search, the event inherits that search's identity, so a recurring
  search resolves to the artifact it led to rather than to a rerun of the query.

### Clustering

Per `(scope, artifact)`, encoded bundles are agglomerated in one deterministic
pass: an event joins the nearest cluster when cosine exceeds
`cluster_merge_at`, otherwise it opens its own; when the count exceeds
`max_clusters`, the two nearest merge. One pass and no randomness, because
otherwise it is not testable.

A cluster holds its centroid, a weight, `last_at`, `encoder_version`, and the
**representative raw bundle** — the member nearest the centroid, which is what
the display quotes.

Events enter weighted by age, with a half-life. A pattern that stops fades
rather than standing forever, and a cluster whose weight falls below
`min_weight` is dropped. That is also what protects against the single
accident: one event never reaches the threshold.

Multiple clusters per artifact are the point. The recycling centre looked up on
Friday afternoons *and* occasionally on Monday mornings is two situations; a
mean of them is a situation that never happened.

### Storage, split

Centroids go to Qdrant as the `ctx` multivector (§7). The bookkeeping —
weights, `last_at`, representative bundle, which centroid is which — goes to
SQLite:

```sql
CREATE TABLE context_clusters (
  id              INTEGER PRIMARY KEY,
  scope           TEXT,
  artifact_id     TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Position of this centroid within the point's `ctx` multivector. Unique per
  -- artifact and NOT per (scope, artifact): the multivector is one array on one
  -- point, shared by every scope that has opened this artifact, so a slot
  -- numbered per scope would have two owners writing index 0.
  slot            INTEGER NOT NULL,
  centroid        BLOB NOT NULL,
  weight          REAL NOT NULL,
  last_at         INTEGER NOT NULL,
  encoder_version INTEGER NOT NULL,
  representative  TEXT NOT NULL,
  UNIQUE (artifact_id, slot)
);
```

Two reasons for the split: Qdrant holds numbers and cannot produce a reason, and
this table survives a `--reindex` while the vectors are rewritten.

### The self-reinforcement guard

`interaction_events` gains two `kind` values:

- `recommended_shown` — something was offered, with artifact and cluster;
- `recommended_open` — the offer was clicked.

A `recommended_open` does **not** count as an ordinary open into the profile, or
counts at `recommend.self_weight` (default `0`). Without this, the first lucky
guess grows into a habit that the system taught itself.

Both rows exist for a second reason: shown against clicked, broken down by
ladder rung, is a hit rate. It is the only number that can later settle whether
the weights in §6 are right. `[sitting] prime` has sat at `false` for months
because nobody measured it (`ROADMAP.md:103`); a recommender with no visible hit
rate becomes the same case.

## 6. The encoder

A bundle becomes a fixed-length `f32` vector composed of named **blocks**.

**The rule that makes this work: each block is normalised to length 1, then
scaled by its weight from config.** A block therefore contributes exactly its
weight, regardless of how many dimensions it happens to use. Seven one-hot slots
for weekday do not outweigh two for the hour because there are seven of them.
This is what turns the encoding's implicit weighting back into named numbers an
operator can change.

| Block | Encoding | Dims | Default weight |
|---|---|---|---|
| `time_of_day` | circular, `sin`/`cos` of the hour angle | 2 | 1.0 |
| `weekday` | one-hot | 7 | 1.0 |
| `weekend` | weekday / weekend | 2 | 0.3 |
| `device` | one-hot over `device_key`, plus an unknown slot | ~8 | 0.8 |
| `viewport` | `log(w)`, `log(h)`, aspect, DPR | 4 | 0.4 |
| `locale` | one-hot language + one-hot time zone | ~8 | 0.3 |
| `network` | one-hot: wifi / cellular / wired / unknown | 4 | 0.6 |
| `power` | charging flag + battery level in steps | 3 | 0.2 |
| `environment` | colour scheme, touch, orientation, audio outputs | 5 | 0.2 |
| `month_cycle` | circular over the month | 2 | 0.0 (off) |

Roughly 45 dimensions.

*As shipped, this table also carried a `scope` block: a pseudo-random direction
per subject over ~8 dimensions at weight 10, so that a foreign cluster could
never win `max_sim` while every subject shared one collection and a payload
filter — which acts on the point, not on elements of the set — could not make
the cut. It was removed on 2026-08-26; see the amendment at the top.*

Two rules that are easy to get wrong:

**Circular where there is a circle; one-hot where there is not.** The hour is a
circle, so `sin`/`cos`: 23:30 and 00:30 are an hour apart, and 14:55 against a
15:00 pattern costs almost nothing. The weekday is *not* a useful circle —
"Friday is three from Tuesday" means nothing, and the pattern is *exactly
Friday*. One-hot, with a separate weak weekday/weekend block for the part that
genuinely is gradual.

**Absent is not zero-valued.** The Battery API does not exist on the desktop. A
missing value zeroes its whole block rather than inventing a default, so it
contributes nothing in either direction. An invented default would manufacture
similarity.

**Not included:** the query embedding. It is the bridge at profile-build time —
which artifact belongs to which situation — not a context dimension. A thousand
text dimensions beside fifty-five context ones would drown everything else.

**Versioning.** The layout fixes `CTX_DIM` at collection creation, so a new
block invalidates every stored centroid. Each cluster carries
`encoder_version`; on mismatch the sweep rebuilds from the raw bundles in
`context_events`. That is what §4's whole-bundle storage buys.

**And the explanation comes back.** Because blocks are named and separately
normalised, each block's contribution is computable at read time as
`w_b * cos(block_now, block_cluster)`. The per-dimension breakdown that the
rejected additive approach would have produced by construction falls out of this
one as a by-product.

## 7. Qdrant

### Collection

`collection_body` (`src/vector/qdrant.rs:210`) gains a third entry:

```json
{
  "vectors": {
    "dense": { "size": dim, "distance": "Cosine" },
    "ctx":   { "size": CTX_DIM, "distance": "Cosine",
               "multivector_config": { "comparator": "max_sim" },
               "hnsw_config": { "m": 0 } }
  },
  "sparse_vectors": { "sparse": { "modifier": "idf" } }
}
```

`m: 0` means no HNSW and an exact scan. That is right here rather than thrifty:
candidates are only artifacts ever opened — hundreds to a few thousand at 55
dimensions — and an index would be rebuilt on every sweep write to beat a scan
it cannot beat at that size.

A point is exactly one artifact (`point_uuid(&p.payload.artifact_id)`,
`:1135`), so the named vector is cleanly one context set per artifact. Points
without `ctx` are skipped in that space, which means the candidate set is
"anything ever opened" without a filter.

This is not the multivector the roadmap cut. That cut was ColBERT-style
late-interaction reranking — one reduced-width vector per *token*, thousands per
artifact. This is two to five per artifact.

### Writing

The sweep uses `POST /collections/{alias}/points/vectors`, not `upsert`.
`upsert` replaces the entire payload; the comment at `:1100` describes exactly
what that costs (`last_seen_at` cleared, `status` cleared, hidden artifacts back
in search). The vector endpoint does not touch payload. Removing a set is
`POST /points/vectors/delete` with `{"vector": ["ctx"]}`.

### Migration

`--reindex` (`:820`) gains a third case beside "copy dense, recompute sparse":
**copy `ctx` when the dimension matches, skip it when it does not.** A changed
encoder layout changes `CTX_DIM`, the old sets are discarded, and the next sweep
rebuilds them from raw bundles. No embedding call in either case.

### Reading

One call per page view:

```json
POST /collections/{alias}/points/query
{ "query": <ctx_vector_now>, "using": "ctx", "limit": 10,
  "with_payload": true,
  "filter": { "must_not": [ {"key":"status","match":{"value":"superseded"}},
                            {"key":"status","match":{"value":"deprecated"}} ] } }
```

`must_not` rather than a positive match, for the reason `build_filter` gives at
`:161`: a point carrying no `status` key reads as active, and a positive clause
would drop every hand-written one.

**A consequence to state plainly.** Qdrant returns the score but not *which*
cluster won — `max_sim` yields only the maximum. The display needs the winning
cluster and its representative bundle. So: top-K from Qdrant, then load those K
artifacts' clusters from `context_clusters` and compute the block breakdown
locally; the argmax reproduces Qdrant's choice. At K=10 and at most five
clusters of 55 dimensions this is free.

That places the same arithmetic in two places. It is the price of holding the
vectors in the index and the reason outside it. A test pins that both pick the
same artifact; if they drift, the line shows a reason for a hit it does not
explain.

### Trait

`VectorStore` gains `set_context_vectors(artifact_id, Vec<Vec<f32>>)` and
`context_query(vec, limit, filter)`. `vector/memory.rs` needs both, including
`max_sim` — a loop over `cosine()`, which already exists at
`src/vector/mod.rs:326`. Without it no test runs without a live Qdrant.

Cost per page view: one round trip, one exact scan over the profiled subset, no
embedding, no model call.

## 8. The display

### Why it is fetched rather than rendered

The bundle originates in the browser; the server does not have it at first
render. `search_page` (`src/web/ui.rs:1219`) renders a placeholder **with
reserved height** — no layout shift — and htmx fills it:

```html
<div hx-post="/ui/context" hx-trigger="load"
     hx-vals='js:{bundle: engramContext()}' hx-swap="outerHTML">
```

One endpoint, two jobs: it writes the `context_event` and answers with the
fragment. Recording happens even when nothing is recommended.

Without JavaScript the area stays empty. With a partial bundle — Battery API
blocked, `connection` unsupported — the affected blocks zero out per §6 and the
rest works.

### The ladder

| Rung | Condition | Line |
|---|---|---|
| **Pattern** | `ctx` score ≥ `strong_at` | "Fridays around 15:00, on the phone" |
| **Similar** | ≥ `weak_at` | "Similar to Friday afternoons" |
| **Sitting** | nothing in `ctx`, sitting open | "Touched in this sitting" |
| **Forgotten** | otherwise | "Not seen in a long time" — this is `resurface` |

Something is always shown, and the wording says what it rests on. **Forgotten**
is deliberately not phrased like a pattern. The distance between "Fridays around
15:00" and "not seen in a long time" is the whole honesty of the feature; blur
it and the area is furniture within a fortnight.

### The reason, kept small

```
Pattern · weekday, hour, device · like 08.08., 15:04      [Details ▾]
```

Three parts, all cheap:

1. the rung name — four fixed strings;
2. the blocks that decided it — sort contributions, take three, join their
   labels. Each block carries a `&'static str`. No sentences, no values in prose;
3. the representative event's timestamp — one date format.

Roughly fifteen lines, and they never change: a new block in §6 brings its label
and is done. Generated prose per block was the first draft and was cut — it
would have coupled every new dimension to a sentence template.

`Details` is `serde_json` over the raw bundle plus the contribution numbers,
inside a `<details>`. No formatting code, and it satisfies "the parameters must
be visible" completely: whoever wants to know exactly, expands it. It is also
the answer to what is being collected — inspectable rather than promised.

### Disappearing on input

The first keystroke in the search field removes the area — removed, not hidden —
and it does **not** come back when the field is cleared. The offer is for the
state "no intent expressed yet"; once there is an intent it is wrong, and
reappearing because someone corrected a typo is flicker. It returns on the next
page view.

### Recording

The response carries artifact id, cluster id and rung. `recommended_shown` is
written server-side when answering; `recommended_open` on the click. Neither
counts as an ordinary open (§5).

The shown-against-clicked rate, broken down by rung, belongs on **Ops** —
`ROADMAP.md` under `[What the base says about itself]` is about mechanisms whose
effect nobody can see, and this would be one.

## 9. Config

Everything under `[recommend]`: `enabled`, the block weights from §6,
`cluster_merge_at`, `max_clusters`, `half_life_days`, `min_weight`,
`strong_at`, `weak_at`, `self_weight`.

Cut so that it folds into a later named mode: **one** gate (`enabled`), and
everything else is a number. `ROADMAP.md` under `[Core Platform]` already
objects to eight gates over one faculty; this does not add a ninth.

## 10. Tests

Small, and only where something can be silently wrong.

1. **Encoder**, a pure function, table-driven: 23:30 is near 00:30; 15:05 is
   near 15:00; a seven-slot block contributes exactly its weight; a missing
   value zeroes its block instead of inventing a default.
2. **Clustering**: six Friday-15:00-phone events plus one Monday outlier gives
   one cluster above threshold and the outlier below `min_weight`.
3. **`max_sim` agreement**: the local recomputation of §7 picks the same
   artifact as the store, against `vector/memory.rs`.
4. **The guard**: a `recommended_open` raises no cluster weight. A named test,
   as the sitting has one saying it writes no activation (`ROADMAP.md:97`).
5. **The acceptance test**, which is the example the feature was asked for: seed
   six Fridays, set the clock to the seventh Friday at 14:52, send the phone
   bundle, assert the artifact comes back at rung **Pattern** with weekday, hour
   and device named.

The ladder's four rungs are covered by 5 and by one case with an empty base,
which must fall to `resurface` and say so.

## 11. What this is not allowed to break

- **Payload.** The sweep writes vectors through `points/vectors`, never
  `upsert`. Clearing `status` or `last_seen_at` puts hidden artifacts back into
  search (`qdrant.rs:1100`).
- **Ranking.** This adds no stage to search. The recommendation is its own
  surface and disappears the moment a query exists. Nothing here moves a hit's
  position in a result list, which is why it does not wait on the harness.
- **Read-time cost.** One vector query, no embedding, no model call.
- **`scope` isolation.** One person's clusters must never be offered to
  another, and it needs a test. *As shipped this was two things, the block's
  weight and the exact cut in `Core::offer`; only the exact cut remains, which
  was always the guarantee — a near-orthogonal direction is a probability.*
- **The reason must match the hit.** If the local recomputation and the store
  disagree, the line explains a different artifact than the one shown. Test 3.

## 12. Cold start, and what is deferred

**Cold start.** A weekly pattern needs weeks; that cannot be argued away. But
`search_events` and `interaction_events` carry `at` and `scope` from the
beginning. There are no bundles for them — but there *is* time, and because §6
zeroes absent blocks rather than defaulting them, old events feed in without
special handling: weekday and hour contribute, device and network contribute
nothing. Weekday and time-of-day patterns therefore stand from the first sweep,
and the remaining blocks fill in as new events arrive.

This is not a backfill path that has to be removed later. It is the ordinary
sweep reading older rows.

**Deferred, on purpose:**

- *Learned block weights.* The defaults in §6 are chosen, not measured. The
  shown/clicked rate is the instrument that would let them be fitted; fitting
  them before that data exists would be guessing with extra steps.
- *Conjunctions across scopes.* The vector can hold them; nothing yet learns
  which ones matter.
- *Dropping the `scope` block.* It goes when each user has their own
  collection, not before. **Done, 2026-08-26** — see the amendment at the top.
