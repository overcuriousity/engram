# Ask harness, ask feedback and knowledge gaps — Design

Date: 2026-08-17
Status: draft
Adds `src/store/asks.rs`, `src/store/gaps.rs`, `src/core/gaps.rs`,
`src/eval/claims.rs`; touches `src/core/ask.rs`, `src/infer/prompt.rs`,
`src/eval/`, `tests/eval.rs`, `src/web/ui.rs`, `src/web/judge.rs`,
`src/core/background.rs`, `src/store/schema.sql`, `ROADMAP.md`.
Roadmap: [Ask] item 1, and the "nothing here is surfaced as such" clause of it.

## 1. Why

Ask is the one door in engram that generates at read time, and today nothing
about it is measured. A search has the judging loop: the query is recorded
before any result is seen, the verdict comes later from a person, `--export-eval`
freezes both, and `cargo test --test eval` turns a knob change into a number.
Ask has none of that. The roadmap is explicit that everything after this item —
situation vectors, streaming, the retrieval loop — is a guess until this one
exists: *"Nothing after this item is a number until this one exists."*

The verdict also carries the sharpest signal engram has about what to capture
next. A question the base cannot answer is a hole with a shape; a search judged
`gap` is the same hole seen from the other door. Both exist today and neither is
shown anywhere.

## 2. Goal

1. Every question asked on `/ui/ask` is recorded — question, answer, the
   excerpts the model was shown — and can be judged on the page: right, wrong,
   nothing here, and which citations carried the answer.
2. `--export-eval` writes the judged questions beside the judged searches, and a
   second ignored harness measures **citation recall**, **abstention accuracy**
   and **faithfulness** — mechanically by literals, and optionally by an
   efficient-model claim check.
3. Unanswered questions and gap searches are grouped into **knowledge gaps** —
   clustered by their stored query vectors, named by the efficient model once
   per cluster at write time — and shown where capture happens.

### Non-goals

- Judging questions asked over the API or MCP. They are not recorded at all:
  the operator asked for the smallest data footprint, and a verdict needs a
  person on a page anyway.
- Any change to what ask retrieves or how it packs. This item measures; item 4
  moves.
- Answers stored as artifacts. Cut in the roadmap, still cut.
- Automatic closing of a gap. A gap is covered when the operator says so.

## 3. The constraints this restates

**Inference at write time, not read time.** Recording a question costs one
SQLite insert. Judging costs an update. Naming a gap cluster costs one
efficient-tier call, in the background, once per new cluster. The claim check
runs only inside the harness, only when asked for.

**The trace is fixed; access is plastic.** Nothing here rewrites an artifact or
an answer. What is stored is what happened; what changes is what the pages
claim about it.

**Lean beats clever.** One new store module for asks, one for gaps, one pure
clustering function, constants where the roadmap says a default must not move
before the harness has run.

## 4. Data model

### 4.1 Ask events

```sql
CREATE TABLE IF NOT EXISTS ask_events (
  id           TEXT PRIMARY KEY,
  question     TEXT NOT NULL,
  scope        TEXT,                    -- the authenticated subject
  filters      TEXT NOT NULL DEFAULT '{}',
  query_vec    BLOB NOT NULL,           -- the vector ask retrieved with
  vec_dim      INTEGER NOT NULL,
  embed_model  TEXT NOT NULL,
  answer       TEXT NOT NULL,
  abstained    INTEGER NOT NULL,        -- the answer opened with ABSTAIN_PREFIX
  dropped      INTEGER NOT NULL,
  truncated    INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  judged_at    INTEGER,
  verdict      TEXT,                    -- right | wrong | nothing_here
  dismissed_at INTEGER                  -- "covered", for a nothing_here gap
);
CREATE INDEX IF NOT EXISTS idx_asks_verdict ON ask_events(verdict, dismissed_at);
CREATE INDEX IF NOT EXISTS idx_asks_created ON ask_events(created_at);

CREATE TABLE IF NOT EXISTS ask_citations (
  event_id    TEXT NOT NULL REFERENCES ask_events(id) ON DELETE CASCADE,
  n           INTEGER NOT NULL,         -- the [n] the model was shown
  artifact_id TEXT NOT NULL,
  score       REAL NOT NULL,
  carried     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (event_id, n)
);
```

The query vector is stored for the same reason `search_events` stores one: a
gap is clustered later against other gaps, and re-embedding at that point would
be an inference call to recover something the request already had.

`search_events` gains `dismissed_at INTEGER` so a gap search can be covered
too. New tables come from `schema.sql`; the new column is an entry in
`ADDED_COLUMNS` in `store/mod.rs`, which appends it to an existing database
(nullable, no backfill — nothing predating it was covered).

### 4.2 Gap clusters

```sql
CREATE TABLE IF NOT EXISTS gap_clusters (
  key         TEXT PRIMARY KEY,         -- hash of the sorted member keys
  label       TEXT NOT NULL,
  labelled_by TEXT NOT NULL,            -- 'model' | 'terms'
  members     TEXT NOT NULL,            -- JSON: [{"kind":"ask"|"search","id":…}]
  created_at  INTEGER NOT NULL
);
```

Membership decides identity: a cluster whose members change is a new cluster
with a new label; the old row is deleted by the sweep. This is what makes "one
call per new cluster" a true bound — the same members never get named twice.

## 5. Recording

`Core::ask(&self, req: &AskRequest, origin: impl Into<Origin>)`, mirroring
`search_with`. The retrieval inside still goes through `Door::Ask` (never
captured as a search). The *question* is recorded when `feedback.enabled` and
`origin.door == Door::Ui`. Every other door answers and forgets.

Recorded after the answer, synchronously — the id is returned to the page. One
insert plus one row per citation; the completion has already taken seconds.

`AskResponse` grows:

- `abstained: bool` — see §6.
- `event_id: Option<String>` — `Some` only when recorded, and the page shows a
  verdict bar only then.

The `filters` JSON is the request's `tags`, `category`, `limit`, as the search
event records them.

## 6. Abstention

`ASK_SYSTEM` currently says *"If the excerpts do not contain the answer, say so
plainly rather than guessing."* Nothing can read that. It becomes:

> If the excerpts do not contain the answer, begin your reply with the exact
> words `Not in the knowledge base.` and say what is missing rather than
> guessing.

`pub const ABSTAIN_PREFIX: &str = "Not in the knowledge base"` sits beside
`ASK_SYSTEM` in `prompt.rs`, for the reason `Caveat:` does: the string the
model is told and the string the code looks for are one definition.

`pub fn abstained(answer: &str) -> bool`: strip leading whitespace and markdown
emphasis/heading characters, compare case-insensitively against the prefix.
The two no-model paths — no hits, and the best excerpt not fitting the window —
set the flag directly: the first is an abstention (`Not in the knowledge base —
nothing matches that question.`), the second is not (it is a configuration
failure and says so, `abstained: false`, no citations).

The page shows an abstained answer with a small badge, *nothing here*, so the
operator sees what the harness will count. Detection can miss when the model
paraphrases; the harness reports that as a wrong-abstention count rather than
hiding it, and the prompt is where it gets fixed.

## 7. Judging on the page

### 7.1 The verdict bar

`_answer.html` renders, under the answer and only when `event_id` is set:

```
Was this right?   [Right]  [Wrong]  [Nothing here]
```

After a click the bar shows the verdict and `undo`. Verdicts:

| verdict        | meaning                                                   |
|----------------|-----------------------------------------------------------|
| `right`        | the answer is correct as stated                           |
| `wrong`        | the base holds the answer and this is not it              |
| `nothing_here` | the base does not hold the answer (whatever the model said) |

### 7.2 Carriers

Each citation card gets a toggle, *carried the answer*. Any number may be on.
Toggling one on while the event is unjudged sets the verdict to `right`;
undoing the verdict clears every carrier. A `right` answer with no carrier is a
synthesis and stays valid: it counts for abstention accuracy, not for citation
recall.

### 7.3 Routes and store

- `POST /ui/ask/{id}/verdict` — form `verdict=right|wrong|nothing_here|none`;
  returns the bar fragment (`_ask_verdict.html`).
- `POST /ui/ask/{id}/carried` — form `n=<citation number>`; toggles; returns
  the toggle fragment with the bar swapped out-of-band.

`src/store/asks.rs`: `record_ask`, `judge_ask`, `unjudge_ask`,
`toggle_carried`, `ask_stats`, `ask_event`(for the fragments), plus the
expiry hooks below. `feedback.rs` stays about searches.

The judge page header gains one line: `N questions judged · right / wrong /
nothing here`.

### 7.4 Retention

`expire_feedback(retain_days)` also deletes ask events that are unjudged and
past the window. A judged event is exempt for the reason a judged search is —
it is the operator's own work, and the only thing the feature produces.
`purge_feedback` takes ask events too. Both are one statement each in
`asks.rs`, called from the existing functions so the promise stays one promise.

## 8. Export

`--export-eval <dir>` writes `questions.json` beside `artifacts.json` and
`pairs.json`:

```json
[{ "question": "…", "verdict": "right", "expect": ["<artifact id>", "…"],
   "note": "judged 1723900000" }]
```

`expect` is the carried artifact ids. Carriers whose artifact no longer exists
are dropped with a warning, as pairs are; a `right` question that loses all its
carriers stays in — it still measures abstention. `EvalQuestion`,
`questions_path`, `load_questions`, `save_questions` in `eval/mod.rs`; `export`
returns a third count and `main.rs` prints it.

## 9. The ask harness

`evaluate_ask` in `tests/eval.rs`, `#[ignore]`, sharing `index()` and
`resolve_expected()` with the search harness. Requires Qdrant, the embedding
endpoint and the ask endpoint. Reads `questions.json`; when it is missing,
prints how to make one and returns, like `evaluate_retrieval`.

For every question it calls `core.ask` with `Door::Judge` as origin — never
recorded — and scores:

- **Citation recall** — over `right` questions with carriers: per question, the
  fraction of carriers (each resolved through supersede) that appear among the
  citations; reported as the mean, plus "all carriers cited n/m".
- **Abstention** — expected = verdict `nothing_here`; observed =
  `response.abstained`. Reported as *abstained when it should n/n* and
  *answered when it should n/n*; both failure lists printed by question prefix.
- **Faithfulness, literals** — over answered questions:
  `missing_literals(&answer, &[], &joined_excerpt_text)`. Reported as
  *answers with no unsupported literal n/m* and the literals, per question.
- **Faithfulness, claims** — only with `ENGRAM_EVAL_CLAIMS=1`. `core.judge`
  (the efficient tier) is given the answer and the numbered excerpts under a
  strict schema `{"claims":[{"claim": str, "supported_by": [int]}]}` and asked
  to split the answer into atomic claims and name the excerpts that support
  each. Reported as *claims supported n/m* and *answers fully supported n/m*.
  Prompt and schema in `infer/prompt.rs` beside the other judges; parsing in
  `eval/claims.rs`, tested against the fake completer. A claim naming an
  excerpt number that was not shown counts as unsupported.

One settings line first — ask model, embed model, question count, claims
on/off — because a number without the configuration that produced it cannot be
compared with anything.

Pure arithmetic lives in `eval/metrics.rs`: `fraction_cited(carriers,
citations)`, `Abstention::tally(expected, observed)`, `fully_supported(counts)`.

## 10. Knowledge gaps

### 10.1 What a gap is

An open gap is either an ask event with verdict `nothing_here` or a search event
with verdict `gap`, with `dismissed_at IS NULL` and a stored vector under the
current `embed_model`. `store/gaps.rs::open_gaps()` returns them as
`Gap { kind, id, text, vec }`.

### 10.2 Clustering

`core/gaps.rs::cluster(gaps: &[Gap], link_at: f32) -> Vec<Vec<usize>>` — pure,
single-linkage over cosine: two gaps join when their similarity is at least
`link_at`, and clusters merge transitively. `pub const GAP_LINK_AT: f32 = 0.55`,
a constant with its reasoning beside it, until the harness says otherwise. N is
tens; O(n²) in memory. Singletons are clusters of one.

### 10.3 Labels

The gap sweep runs on the retention ticker (`background.rs`), which already
owns the feedback tables' housekeeping and runs every `feedback.sweep_hours`;
it becomes a sleep phase later without moving. Each pass:

1. loads open gaps, clusters them, keys each cluster by the hash of its sorted
   member keys;
2. deletes `gap_clusters` rows whose key is no longer present;
3. for each new key, asks the efficient model (`core.judge`, under the
   background lane) for a domain name — 3 to 6 words, from the member
   questions, strict schema `{"label": str}`; on any failure, or when there is
   no model to ask, stores the three most frequent content terms across the
   members as the label with `labelled_by = 'terms'`. A terms-labelled cluster
   is retried by the model on the next pass, so a cold endpoint costs a delay,
   not a permanent worse label.

The prompt does not see answers, only questions: it names the hole, not the
guess.

### 10.4 The capture page

A *Knowledge gaps* block beside the capture box, only when `feedback.enabled`
and there is at least one open gap:

```
Knowledge gaps
▸ Forensic image mounting (4)
▸ FAT directory entries (2)
```

Each row expands (`<details>`) to its member questions, each with *ask again*
(a link to `/ui/ask?q=…`, which prefills the box) and *covered*
(`POST /ui/gaps/{ask|search}/{id}/dismiss` sets `dismissed_at`; the row swaps
itself out). Read straight from `gap_clusters` joined to its members — the
page never clusters or names anything; if the sweep has not run since the last
verdict, a new gap sits unclustered under its own question until it does, and
the block says "not yet grouped" for those.

## 11. Configuration

Nothing new. `feedback.enabled` gates recording, gaps and the page blocks;
`feedback.retain_days` / `sweep_hours` govern expiry and the gap sweep.
`GAP_LINK_AT`, `ABSTAIN_PREFIX` are constants. `ENGRAM_EVAL_CLAIMS=1` is a
harness switch, documented in the `tests/eval.rs` header and README.

## 12. Testing

- `prompt::abstained`: prefix, markdown-wrapped prefix, case, an answer that
  merely mentions the phrase later (false).
- `core::ask`: a UI ask is recorded with its citations; an API ask is not; a
  no-hit ask is recorded abstained; `event_id` is `None` when feedback is off.
- `store/asks.rs`: record/judge/unjudge/toggle; toggling on sets `right`;
  unjudge clears carriers; expiry keeps judged and takes unjudged; purge takes
  all.
- Routes: verdict and carried through `oneshot`, fragment contents, unknown id
  is 404.
- Export: a judged question with a carrier becomes an entry; a carrier whose
  artifact is gone is dropped; unjudged questions are not exported.
- Metrics: pure tests for each; claim parsing: a well-formed reply, a claim
  naming an unshown excerpt, malformed JSON.
- Gaps: `cluster` on hand-built vectors (two clusters and a singleton;
  transitive merge); the sweep names a new cluster once and not again; a
  dismissed gap leaves its cluster; the capture page renders the block, the
  "not yet grouped" case, and nothing when feedback is off.
- Full suite and clippy before each commit.

## 13. Rollout

One branch, `feat/ask-harness`, in the order: schema and store → recording and
abstention → verdict UI → export → metrics and harness → gaps store and
clustering → gap sweep and labels → capture page → docs (README, ROADMAP).
Each step green before the next.

## 14. Risks

- **The model ignores the sentinel.** Then `abstained` is under-reported; the
  harness shows it as "abstained when it should" falling, and the fix is the
  prompt. Nothing downstream is corrupted by a missed detection.
- **Carriers are a chore to mark.** One toggle per card, optional; a `right`
  without carriers is still a valid verdict.
- **Cluster labels drift as members join.** A changed membership is a new
  cluster and gets a new name; the old row goes. Labels are never edited in
  place, so a stale one cannot linger.
- **The claim check disagrees with the literal check.** They measure different
  things and are printed as two lines; neither is folded into the other.
