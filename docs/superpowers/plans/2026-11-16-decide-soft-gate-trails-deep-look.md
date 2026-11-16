# Decide's soft gate, trails and the deep look: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decide stops refusing when notes exist and tunes its own line; a note keeps the queries it was found for and search matches them; a quiet base looks deeper for what search missed. Nothing needs a person to judge it.

**Architecture:** Three parts on one branch. Part 1 changes `src/core/decide/` and its doors. Part A adds a `trails` table in the tenant store, an in-memory index on `Core`, and one step in `search_inner`, weighted by a new ranking setting the idle pass tunes. Part B adds a background stage, `Stage::Deepen`, armed from the `Decisions` tick, that asks the labeller's question of notes search did not show and turns finds into trails, gap coverage and sweep pairs.

**Tech stack:** Rust 1.94, sqlx 0.9 (SQLite, compile-time checked queries), axum, askama, htmx; tests with `cargo test`, `node --test`.

**Spec:** `docs/superpowers/specs/2026-11-16-decide-soft-gate-trails-deep-look-design.md`. Read it first; this plan argues from it and does not repeat its reasons.

## Global Constraints

- AGENTS.md and the dev manifest v2026.11.1 (profile everyday) hold. UI: tokens only, no inline styles or scripts, symbol and colour for status, layout from 390 px.
- No probability shown as the truth, no percent sign, no "likely" in any Decide output.
- Queries are `sqlx::query!`-family; a runtime query carries `// runtime: <reason>` within three lines above it (`every_runtime_query_says_why`).
- New tables go in `src/store/schema.sql` as `CREATE TABLE IF NOT EXISTS`; a new column on an existing table also gets an `ADDITIVE` entry in `src/store/mod.rs` (bump the array length).
- After any query or schema change: `cargo run -- --dev-schema target/sqlx-prepare.db`, then `DATABASE_URL=sqlite://target/sqlx-prepare.db cargo sqlx prepare -- --all-targets --features contained`; commit `.sqlx/`.
- `docs/api.md` fields are added, never renamed or removed.
- `bears` keeps its wording and `BEARS_VERSION` stays 1.
- Nothing may assume a kind of note or question.
- Exact values: `weak_take = 3`; `BEARS_BOUNDS = (0.20, 0.80)`; `STEP = 0.02`; `trails.weight = 0.5`, ladder `[0.0, 0.5, 1.0]`; `trails.match_at = 0.80`; `trails.per_note = 8`; `trails.max = 20000`; near-duplicate cosine `0.95`; leave-one-out cosine `0.999`; model tier factor `0.5`; retirement `missed >= 3 && missed > matched`; `deep_max = 24`; deep pool replay limit `50`, links of the first `5`; selectivity window `200`.
- Copy, verbatim: `Leaning, from loose matches.` / `Split, from loose matches.` / `Weighed the closest {n} of {read}` / `{rest} more were not weighed` / `Weighing the closest together` / `found before for «{query}»`.
- Every task ends green on: `cargo test`, `cargo test --features contained --lib`, `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-targets --features contained -- -D warnings`.
- Test snippets show the assertions that must hold. A helper they call that does not exist yet (`req`, `cited_observations`, `place_of`, `open_gap_ids`, …) is written beside the test; existing helpers (`base`, `deciding`, `none_bear`, `learning`, `three_base`, `decide_gaps`, `judged`, `labelled_search`) are reused.
- Commit messages follow the repository's form: `type(scope): a sentence saying what is now true`, with the session's attribution trailer.

## Review Focus

Inputs the spec implies and its "Done when" does not pin. Each has a test in the task named.

1. **A decision over fewer passages than `weak_take`** (one note in the base, nothing bears): weak, one evidence row, no panic. Task 2.
2. **A search that fell back to keywords** (no query vector): the trail step is skipped and results are as before. Task 10.
3. **A trail whose note is trashed, or fails the search's tag or category filter**: not joined. Task 10.
4. **Stored rows from before this change**: a `generations.params` JSON without `trail_weight` decodes to the config value; a `decide_events` row without `weak` reads as not weak. Tasks 2 and 9.
5. **A deep look whose source is deleted by retention before it runs**: the unit ends without retry and stores nothing. Task 14.

## File Structure

| File | Responsibility |
|---|---|
| `src/core/decide/mod.rs` | the weak path, `Core::bears_at`, situation retrieval |
| `src/core/decide/standing.rs` | the cap on a weak standing |
| `src/core/decide/wire.rs`, `verdict.rs` | `weak` on the wire; verdicts on a weak decision |
| `src/store/decisions.rs`, `gaps.rs` | `weak` stored; the gap predicate; the line's records |
| `src/jobs/tune.rs` | `next_bears_at`, the `decide_bears` move |
| `src/web/decide.rs`, `templates/_decision.html`, `_decide_rail.html`, `assets/js/decide.js`, `src/cli/decide.rs`, `src/mcp/mod.rs` | the weak copy at each door |
| `src/jobs/label.rs`, `src/store/search_labels.rs` | `labels_count` turned round; recent selectivity |
| `src/store/trails.rs` (new) | trails and their sources as rows |
| `src/core/trails.rs` (new) | the in-memory index, matching, minting, the reorder arithmetic |
| `src/core/search.rs`, `explain.rs`, `ranking.rs`, `src/store/generations.rs`, `src/eval/sweep.rs`, `src/config.rs` | the trail step and its setting |
| `src/store/deep_looks.rs` (new) | deep looks and their failures as rows |
| `src/jobs/deepen.rs` (new) | `Stage::Deepen`: pool, asking, what a find does |
| `src/jobs/decisions.rs`, `src/core/judging/mod.rs` | arming and budget |
| `src/web/insights.rs` | the new lines |

---

## Part 1: Decide

### Task 1: The line is read from one place

**Files:**
- Modify: `src/config.rs` (`DecideConfig`, its `Default`, validation near `:2070`), `config.example.toml`
- Modify: `src/core/decide/mod.rs:27-33,263-273,301-302,374`

**Interfaces:**
- Produces: `pub const BEARS_ID: &str = "decide_bears";` in `src/core/decide/mod.rs`
- Produces: `impl Core { pub async fn bears_at(&self) -> f64 }` — `thresholds_in_force(BEARS_ID, &settled_era(model))` → `act_at`, else `decide_cfg.bears_min`; on a store error, warn and use config (mirror `settled_at`).
- Produces: `DecideConfig.weak_take: usize` (default 3, valid 1..=8) and `DecideConfig.deep_max: usize` (default 24, valid 1..=50).

- [ ] **Step 1: Write the failing tests** in `src/core/decide/mod.rs` tests:

```rust
#[tokio::test]
async fn the_bears_line_is_the_tuned_one_else_config() {
    let core = test_core().await;
    assert_eq!(core.bears_at().await, 0.5);
    let era = settled_era(core.decider.model());
    let to = crate::core::judging::band::Thresholds { act_at: 0.46, ask_at: 0.46 };
    core.store.move_thresholds(BEARS_ID, &era, to, "test").await.unwrap();
    assert_eq!(core.bears_at().await, 0.46);
}
```

and in `src/config.rs` tests, beside the existing range cases (`:3552`): `("weak_take = 0", "decide.weak_take")`, `("weak_take = 9", "decide.weak_take")`, `("deep_max = 0", "decide.deep_max")`, `("deep_max = 51", "decide.deep_max")`.

- [ ] **Step 2: Run** `cargo test --lib the_bears_line_is` — FAIL, `bears_at` not found.
- [ ] **Step 3: Implement** the two config fields with validation, `BEARS_ID`, `Core::bears_at`. In `decide_events`, set `cfg.bears_min = core.bears_at().await;` beside the existing `cfg.settled_at = …` so the rest of the function reads the tuned line unchanged. Add the two commented keys to `config.example.toml` under `[decide]` with the spec's wording.
- [ ] **Step 4: Run** `cargo test --lib decide::` and `cargo test --lib config::` — PASS.
- [ ] **Step 5: Commit** `feat(decide): the bearing line is read as the settled line is, tuned or from config`

### Task 2: A decision over loose matches

**Files:**
- Modify: `src/core/decide/mod.rs` (`Decision`, the stream body `:386-459`, `record_decision`)
- Modify: `src/core/decide/standing.rs`
- Modify: `src/core/decide/wire.rs` (`Serialize for Decision`)
- Modify: `src/store/schema.sql` (`decide_events`), `src/store/mod.rs` (`ADDITIVE`), `src/store/decisions.rs` (`NewDecision`, `StoredDecision`, `record_decision`)
- Modify: every `Decision { … }` literal: `src/web/decide.rs:455,570`, `src/core/decide/wire.rs:514`, `src/mcp/mod.rs:1914`

**Interfaces:**
- Consumes: `DecideConfig.weak_take`, `Core::bears_at` (Task 1).
- Produces: `Decision.weak: bool`; `NewDecision.weak: bool`; `StoredDecision.weak: bool`; column `decide_events.weak INTEGER NOT NULL DEFAULT 0`.
- Produces: `pub fn capped(s: Standing, weak: bool) -> Standing` in `standing.rs` — `Settled` becomes `Leaning` when `weak`; everything else unchanged.
- Behaviour: when no read is bearing and `reads` is non-empty, the evidence is the `weak_take` reads with the highest `bears` (ties by rank); `Evidence.joint` follows packing as today; `decision.weak = true`. `not_held` only when no passage was read. A weak decision writes no `Cited` observation and calls `mark_artifacts_cited` with nothing. `decide_evidence.n` is set for every evidence row of a weak decision; `bearing` stays 0.
- JSON: `"weak": true` written only when true, after `standing`.

- [ ] **Step 1: Write the failing tests** in `src/core/decide/mod.rs` tests. `none_bear()` is the existing rule (bears 0.02, everything else 0.99).

```rust
#[tokio::test]
async fn nothing_bearing_weighs_the_closest_and_says_so() {
    let mut core = three_base().await; learning(&mut core);
    deciding(&mut core, none_bear());
    let d = core.decide(&req("Keep it?"), Door::Ui).await.unwrap();
    assert!(d.weak && !d.not_held());
    assert_eq!(d.evidence.len(), 3);
    assert!(d.evidence.iter().all(|e| e.joint));
    assert_eq!(d.standing[0].1, Standing::Leaning); // 0.99 would be settled
    let stored = core.store.decision(d.event_id.as_deref().unwrap()).await.unwrap().unwrap();
    assert!(stored.weak && !stored.not_held);
    assert!(stored.evidence.iter().all(|e| !e.bearing && e.n.is_some()));
    assert_eq!(cited_observations(&core).await, 0);
}

#[tokio::test]
async fn one_passage_that_does_not_bear_is_weighed_alone() {          // Review Focus 1
    let mut core = base(&[("Only", "one note")]).await;
    deciding(&mut core, none_bear());
    let d = core.decide(&req("Keep it?"), Door::Ui).await.unwrap();
    assert!(d.weak); assert_eq!(d.evidence.len(), 1);
}

#[test]
fn a_weak_standing_is_never_settled() {
    assert_eq!(capped(Standing::Settled, true), Standing::Leaning);
    assert_eq!(capped(Standing::Split, true), Standing::Split);
    assert_eq!(capped(Standing::Settled, false), Standing::Settled);
}
```

In `wire.rs` tests: a weak decision serialises with `"weak":true`; a decision that is not weak has no `weak` key. In `src/store/decisions.rs` tests: a row inserted by the pre-change column list (use `test_support::judged`) reads `weak == false` (Review Focus 4).

Rewrite, do not delete, the tests that pinned the old refusal: `nothing_bearing_means_no_verdict_whatever_the_model_would_say` (`:737`) becomes the first test above; the second half of `a_held_decision_marks_what_it_was_weighed_from_as_cited` (`:1432`) asserts a weak decision cites nothing; `api_decide_says_not_held_with_200` (`src/web/api.rs:4182`) moves to an empty base. `an_empty_base_is_not_held_after_the_prior_alone` stays as it is.

- [ ] **Step 2: Run** `cargo test --lib decide::` — FAIL on the new tests.
- [ ] **Step 3: Implement.** Add the column and its `ADDITIVE` entry (`"decide_events", "weak", "ALTER TABLE decide_events ADD COLUMN weak INTEGER NOT NULL DEFAULT 0"`). In the stream: replace the `bearing.is_empty()` early return with the fallback selection when `reads` is non-empty; apply `capped` to each standing; skip the cited list when weak. In `store::record_decision`, the observation loop's condition gains `&& !d.weak`.
- [ ] **Step 4: Regenerate `.sqlx/`** (Global Constraints), then run `cargo test --lib decide::`, `cargo test --lib store::decisions`, `cargo test --lib web::api` — PASS.
- [ ] **Step 5: Commit** `feat(decide): with no note over the line the closest are weighed and the decision says it is weak`

### Task 3: Gaps and verdicts for a weak decision

**Files:**
- Modify: `src/store/gaps.rs:138-148` (`decide_gaps_from!`)
- Modify: `src/core/decide/verdict.rs:132-209`
- Modify: `src/web/decide.rs` (`recorded()` carries `weak`)

**Interfaces:**
- Consumes: `StoredDecision.weak` (Task 2).
- Behaviour: gap predicate exactly `((not_held = 1 AND COALESCE(verdict, '') <> 'wrong') OR (weak = 1 AND COALESCE(verdict, '') <> 'right') OR (not_held = 0 AND verdict = 'nothing_here'))`. `judge_decision` on a weak decision stores the verdict (and `correct` when known) and returns before `confirm_cited` and before queuing pairs.

- [ ] **Step 1: Write the failing tests** in `src/core/decide/verdict.rs` tests, using its `recorded()` helper extended with a `weak` flag:

```rust
#[tokio::test]
async fn a_weak_decision_is_a_gap_until_judged_right() {
    // arrange: a weak decision d over three notes
    assert_eq!(decide_gaps(&core).await, vec![d.clone()]);
    core.judge_decision(&d, AskVerdict::Wrong, None).await.unwrap();
    assert_eq!(decide_gaps(&core).await, vec![d.clone()]);
    core.judge_decision(&d, AskVerdict::Right, None).await.unwrap();
    assert!(decide_gaps(&core).await.is_empty());
}

#[tokio::test]
async fn right_on_a_weak_decision_touches_no_note() {
    core.judge_decision(&d, AskVerdict::Right, None).await.unwrap();
    assert!(verified_ids(&core).await.is_empty());
    assert_eq!(pending_pairs(&core).await, 0);
}
```

Keep `a_not_held_decision_is_recorded_and_is_an_open_gap` and `…judged_wrong_is_no_gap…` on an empty base.

- [ ] **Step 2: Run** `cargo test --lib decide::verdict` — FAIL.
- [ ] **Step 3: Implement** the predicate and the early return.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib decide::` and `cargo test --lib store::gaps` — PASS.
- [ ] **Step 5: Commit** `feat(decide): a weak decision is an open gap until it is judged right, and a verdict on it touches no note`

### Task 4: The situation takes part in retrieval

**Files:**
- Modify: `src/core/decide/mod.rs:536-581` (`decide_passages`)

**Interfaces:**
- Produces: `fn interleave(a: Vec<SearchResult>, b: Vec<SearchResult>) -> Vec<SearchResult>` (private): `a[0], b[0], a[1], b[1], …`, a row whose `artifact_id` was already taken is skipped, the longer tail follows.
- Behaviour: with `asked.situation = Some(s)`, a second `search_with` on `counter.cut(s, n, true)` where `n` is what `search_inner` accepts (`0.8 * embedder.max_input_tokens()` less the query template's own tokens); same filters, door, limit. A `Validation` error from it is logged at debug and the decision goes on with `q`'s list. `query_vec` returned is `q`'s.

- [ ] **Step 1: Write the failing tests:**

```rust
#[tokio::test]
async fn a_passage_only_the_situation_finds_is_read() {
    // arrange: notes "Alpha: quartz" and "Beta: marmalade"; q matches Alpha only
    let mut r = req("quartz?"); r.situation = Some("marmalade".into()); r.limit = Some(2);
    let d = core.decide(&r, Door::Ui).await.unwrap();
    assert!(d.reads.iter().any(|p| p.title.as_deref() == Some("Beta")));
}

#[test]
fn interleaving_takes_each_note_once_and_keeps_the_longer_tail() {
    assert_eq!(ids(interleave(hits(&["a","b","c"]), hits(&["b","d"]))), ["a","b","d","c"]);
}
```

plus: a situation of 2000 tokens over a small embedder window still returns a decision.

- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --lib decide::` — PASS (`the_situation_leads_every_state` must still pass).
- [ ] **Step 5: Commit** `feat(decide): a caller's situation is searched with, beside the question`

### Task 5: The weak decision at every door

**Files:**
- Modify: `src/web/decide.rs` (`standing_copy` call site, `rail_fragment`, `rail_head`), `src/web/templates/_decision.html`, `src/web/templates/_decide_rail.html`
- Modify: `assets/js/decide.js`, `assets/js/decide.test.mjs`, `assets/app.js:758-803` (pass through what `progressText` needs)
- Modify: `src/cli/decide.rs` (`render`), `src/mcp/mod.rs` (`format_decision`)
- Modify: `docs/api.md:56`, `README.md:19-20`

**Interfaces:**
- Produces: `pub fn standing_line(s: Standing, weak: bool) -> String` in `src/web/decide.rs`: `standing_copy(s)` when not weak; when weak, the copy with its full stop replaced by `, from loose matches.`. CLI and MCP call it.
- Produces: `RailTemplate.weak: bool`; `rail_head` returns `Weighed the closest {evidence} of {read}` when weak.
- Produces: `progressText(read, of, bearing)` returns `Weighing the closest together` when `read === of && bearing === 0 && of > 0`.

- [ ] **Step 1: Write the failing tests.** `src/web/decide.rs`: a weak decision's fragment contains `Leaning, from loose matches.` and bars, and not the notice; `rail_head` is `Weighed the closest 3 of 8`; the rail contains `5 more were not weighed` and not `did not bear`; the verdict bar has the `Nothing here` button. `decide.test.mjs`: `progressText(8,8,0) === "Weighing the closest together"`, `progressText(0,0,0) === ""`. `src/cli/decide.rs`: rendered text has the standing line and `were not weighed`; `--json` exit is `0`. `src/mcp/mod.rs`: `format_decision` has the standing line. All three: no `%`, no `likely`.
- [ ] **Step 2: Run** `cargo test --lib web::decide cli::decide mcp::` and `node --test assets/js/decide.test.mjs` — FAIL.
- [ ] **Step 3: Implement.** `_decide_rail.html` chooses between `more did not bear on it` and `more were not weighed` on `weak`. `docs/api.md`: add `weak` to the answer shape ("present and `true` when no note reached the line and the closest were weighed; absent means false") and narrow the `not_held` sentence to "no passage was found". README: one clause.
- [ ] **Step 4: Run** the same commands, then `cargo test --test browser_decide` if `ENGRAM_CHROME` is set — PASS.
- [ ] **Step 5: Commit** `feat(ui): a weak decision reads "from loose matches" in the pane, the rail, the CLI and MCP`

### Task 6: The line moves on verdicts

**Files:**
- Modify: `src/jobs/tune.rs` (constants near `:733`, `tune_questions` near `:881`)
- Modify: `src/store/decisions.rs` (beside `settled_record`)
- Modify: `src/web/insights.rs:943-997`
- Modify: `src/core/decide/mod.rs` test `split_at_and_bears_min_are_never_moved`

**Interfaces:**
- Produces: `pub const BEARS_BOUNDS: (f64, f64) = (0.20, 0.80);`
- Produces: `#[derive(Default, Clone, Copy, PartialEq, Debug)] pub struct BearsEvidence { pub too_low: usize, pub too_high: usize, pub fine: usize }` in `src/store/decisions.rs`, with `fn n(&self) -> usize`.
- Produces: `impl Store { pub async fn bears_verdicts(&self, model: &str) -> Result<BearsEvidence> }` — decisions under `decider = model`, `BEARS_VERSION`, created after `moved_at(BEARS_ID, era)`: not weak + `nothing_here` → `too_low`; weak + `right` → `too_high`; not weak + `right`, weak + `nothing_here` → `fine`. `not_held` rows and `wrong` are not counted.
- Produces: `pub fn next_bears_at(current: f64, e: BearsEvidence, tune_min: usize, target: f64) -> Option<f64>`:

```rust
if e.n() < tune_min || e.n() == 0 { return None; }
let n = e.n() as f64;
if e.too_low as f64 / n > target && e.too_low > e.too_high && current < BEARS_BOUNDS.1 - 1e-9 {
    Some(two_places(current + STEP).min(BEARS_BOUNDS.1))
} else if e.too_high as f64 / n > target && e.too_high > e.too_low && current > BEARS_BOUNDS.0 + 1e-9 {
    Some(two_places(current - STEP).max(BEARS_BOUNDS.0))
} else { None }
```

- Behaviour: `tune_questions`, after the settled line, moves `BEARS_ID` with `move_thresholds`, reason `"{too_high} of {n} weak held a note that answered"` or `"{too_low} of {n} rested on notes that bore nothing"`.

- [ ] **Step 1: Write the failing tests.** `tune.rs`: table test of `next_bears_at` — `(0.50, {too_high:4, fine:27})` → `Some(0.48)`; `(0.50, {too_low:4, fine:27})` → `Some(0.52)`; `(0.50, {too_low:4, too_high:4, fine:23})` → `None`; `(0.50, {too_high:4, fine:20})` → `None` (under 30); `(0.20, {too_high:31})` → `None`. `core/decide/mod.rs`: thirty-one judged decisions, four weak and right, give `tune_questions == 1`, `bears_at() == 0.48`, and a move with `question_id == "decide_bears"`; thirty-one with four `wrong` move nothing. Rename the old test to `split_at_is_never_moved` and assert only that.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.** Insights: a line `decide_bears · bears {:.2} · moved …|as configured` beside the settled line, and a share `weak and right` in `decide_shares`; update the field doc.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib tune decide:: insights` — PASS.
- [ ] **Step 5: Commit** `feat(tune): Decide's bearing line moves on what weak and held decisions were judged`

### Task 7: Labels are trusted until shown otherwise

**Files:**
- Modify: `src/jobs/label.rs:152-196`
- Modify: `src/store/search_labels.rs`
- Modify: `src/web/insights.rs:34-76` (`model_lines`), `docs/evaluation.md` (the paragraph on labels)

**Interfaces:**
- Produces: `impl Store { pub async fn recent_selectivity(&self, relevant_at: f64, version: u32, model: &str, last: i64) -> Result<Option<f64>> }` — over the newest `last` searches that have labels at this version and model (`artifact_id <> ''`), the mean of relevant over labelled; `None` when there are none.
- Produces: `pub enum Stopped { ByThePerson, BySelectivity }` and `Figures.stopped: Option<Stopped>`; `Figures.counts == stopped.is_none()`.
- Changes: `Core::labels_count()` returns `false` only when (a) `agreement.judged >= label_overlap_min && (hit_rate < label_agree_min || selectivity > label_select_max)`, or (b) `recent_selectivity(.., 200)` is `Some(s)` with `s > label_select_max`. Otherwise `true`.
- Produces: `pub const SELECTIVITY_WINDOW: i64 = 200;` in `src/jobs/label.rs`.

- [ ] **Step 1: Write the failing tests** in `src/jobs/label.rs` (the helper `labelled_search` in `src/eval/sweep.rs:945` is the pattern):

```rust
#[tokio::test] async fn on_a_base_nobody_judged_the_labels_count() { assert!(core.labels_count().await.unwrap()); }
#[tokio::test] async fn a_model_that_calls_everything_relevant_is_stopped_without_a_verdict() {
    // arrange: 5 unjudged searches, every candidate labelled 0.95
    assert!(!core.labels_count().await.unwrap());
    assert_eq!(core.label_figures().await.unwrap().stopped, Some(Stopped::BySelectivity));
}
#[tokio::test] async fn thirty_judged_searches_the_model_missed_stop_the_labels() { /* hit_rate 0.5 */ }
```

Invert the existing assertions that expected `false` on too few judged searches (`src/eval/sweep.rs:1012-1058`, `src/jobs/label.rs` tests).

- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.** `model_lines` says `model labels count toward sweeps` or `model labels are stopped: {they disagree with your verdicts | the model calls too much relevant}`. Rewrite the `docs/evaluation.md` paragraph to the new rule.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib label sweep insights` — PASS.
- [ ] **Step 5: Commit** `feat(label): the model's labels count until a verdict or their own selectivity shows them bad`

---

## Part A: Trails

### Task 8: Trails as rows

**Files:**
- Create: `src/store/trails.rs`; register in `src/store/mod.rs`
- Modify: `src/store/schema.sql` (tables `trails`, `trail_sources` exactly as the spec's "Storage"; `search_candidates.trail_id TEXT`), `src/store/mod.rs` (`ADDITIVE` for `trail_id`)

**Interfaces (all produced):**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailSource { Verdict, Ask, Decide, Label, Deep }      // as_str: verdict|ask|decide|label|deep
impl TrailSource { pub fn by_a_person(self) -> bool }            // Verdict | Ask | Decide

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier { Person, Model }

pub struct NewTrail<'a> {
    pub artifact_id: &'a str, pub query: &'a str, pub query_vec: &'a [f32],
    pub embed_model: &'a str, pub source: TrailSource, pub source_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct Trail {
    pub id: String, pub artifact_id: String, pub query: String, pub vec: Vec<f32>,
    pub tier: Tier, pub matched: i64, pub missed: i64,
}

impl Store {
    /// Adds the pairing as a source of the live trail on this note within NEAR (0.95) of the
    /// query, else makes a trail; then enforces `per_note` and `max`. Returns the trail's id.
    pub async fn mint_trail(&self, t: &NewTrail<'_>, per_note: usize, max: usize) -> Result<String>;
    /// Removes the source row from every trail that has it; a trail left with none is retired.
    pub async fn withdraw_trail_source(&self, source: TrailSource, source_id: &str) -> Result<u64>;
    pub async fn live_trails(&self, embed_model: &str) -> Result<Vec<Trail>>;
    pub async fn stale_trails(&self, embed_model: &str, limit: i64) -> Result<Vec<(String, String)>>; // (id, query)
    pub async fn reembed_trail(&self, id: &str, vec: &[f32], embed_model: &str) -> Result<()>;
    pub async fn trail_confirmed(&self, id: &str) -> Result<()>;   // matched += 1, used_at = now
    /// missed += 1; retires at `missed >= 3 && missed > matched`. True when it retired.
    pub async fn trail_missed(&self, id: &str) -> Result<bool>;
    /// Model trails whose `used_at` is older than `before`. Deleted, not retired: typed words age.
    pub async fn expire_model_trails(&self, before: i64) -> Result<u64>;
    pub async fn forget_trails(&self) -> Result<u64>;
    pub async fn trail_counts(&self) -> Result<TrailCounts>;       // { live, notes, by_person, found_through }
}
pub const NEAR: f32 = 0.95;
```

Eviction order for both caps: model tier before person tier, then oldest `created_at`. Tier is computed from `trail_sources`, never stored.

- [ ] **Step 1: Write the failing tests** in `src/store/trails.rs`:

```rust
#[tokio::test] async fn a_near_pairing_joins_the_trail_it_resembles() {
    let a = store.mint_trail(&new("n1", "q one", &v(1.0, 0.0), Label, "e1"), 8, 20000).await.unwrap();
    let b = store.mint_trail(&new("n1", "q one!", &v(0.99, 0.05), Verdict, "e2"), 8, 20000).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(store.live_trails(M).await.unwrap()[0].tier, Tier::Person);
}
#[tokio::test] async fn a_trail_survives_one_undo_and_is_retired_by_both() {
    store.withdraw_trail_source(Verdict, "e2").await.unwrap();
    assert_eq!(store.live_trails(M).await.unwrap()[0].tier, Tier::Model);
    store.withdraw_trail_source(Label, "e1").await.unwrap();
    assert!(store.live_trails(M).await.unwrap().is_empty());
}
#[tokio::test] async fn the_ninth_trail_on_a_note_puts_out_a_model_trail_first() { /* per_note 8 */ }
#[tokio::test] async fn three_misses_over_no_match_retire_a_trail() {
    assert!(!store.trail_missed(&id).await.unwrap()); assert!(!store.trail_missed(&id).await.unwrap());
    assert!(store.trail_missed(&id).await.unwrap());
}
#[tokio::test] async fn a_model_trail_ages_and_a_person_trail_does_not() { /* expire_model_trails(now + 1) */ }
#[tokio::test] async fn a_trail_under_another_embed_model_is_not_live() { /* live_trails("other") is empty */ }
#[tokio::test] async fn deleting_the_note_deletes_its_trails() { /* FK cascade */ }
```

- [ ] **Step 2: Run** `cargo test --lib store::trails` — FAIL.
- [ ] **Step 3: Implement.** Vectors are stored as the other tables store `query_vec` (reuse the existing blob helpers in `src/store/`). The near-duplicate check loads the note's live trails and compares in Rust.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib store::` — PASS.
- [ ] **Step 5: Commit** `feat(store): trails, the queries a note was found for, with every pairing behind each`

### Task 9: `trail_weight`, a ranking setting

**Files:**
- Modify: `src/config.rs` (new `TrailsConfig` on `Config` as `trails`, defaults, validation, `apply_learn_mode`), `config.example.toml`
- Modify: `src/core/ranking.rs`, `src/store/generations.rs`, `src/eval/sweep.rs` (`candidates`, `moved`), `src/eval/rehearsed.rs` (`probes_tell_apart` is unchanged: trails are visible to a replay), `src/core/mod.rs` (`Core.trails_cfg`)

**Interfaces:**
- Produces: `pub struct TrailsConfig { pub weight: f32, pub match_at: f32, pub per_note: usize, pub max: usize }` — defaults `0.5, 0.80, 8, 20000`; valid `weight` in `[0,1]`, `match_at` in `(0,1]`, `per_note` 1..=64, `max` 0..=200000.
- Produces: `pub const TRAIL_WEIGHTS: [f32; 3] = [0.0, 0.5, 1.0];` and `RankingParams.trail_weight: f32`; `GenerationParams.trail_weight: f32` with `#[serde(default = "crate::config::default_trail_weight")]`.
- Produces: `Core.trails_cfg: TrailsConfig`.
- Behaviour: `apply_learn_mode` resolves `trails.weight` to `0.0` for `Off` and `Learning`. `candidates()` walks `TRAIL_WEIGHTS` outward like `PRIME_LIFTS`; `moved()` counts it.

- [ ] **Step 1: Write the failing tests.** `generations.rs`: `serde_json::from_str::<GenerationParams>(r#"{"recency_weight":0.1,"per_source_cap":3}"#)` has `trail_weight == 0.5` (Review Focus 4). `sweep.rs`: `candidates(current, &[], 20)` contains one candidate with `trail_weight == 0.0` and one with `1.0`, each differing from `current` in nothing else. `config.rs`: `mode = "learning"` gives `trails.weight == 0.0`; a file that sets `trails.weight = 1.0` keeps it.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --lib config ranking generations sweep tune` — PASS.
- [ ] **Step 5: Commit** `feat(ranking): how much a trail counts is a setting on a ladder, off outside full learning`

### Task 10: The trail step in search

**Files:**
- Create: `src/core/trails.rs`; register in `src/core/mod.rs`
- Modify: `src/core/search.rs` (`search_inner`: after `cap_per_corpus` `:1330`, around the reranker `:1441-1472`, before priming `:1504`, the candidate recording `:1571-1640`)
- Modify: `src/core/explain.rs` (`HitExplanation`)
- Modify: `src/store/feedback.rs` (`NewCandidate.trail_id: Option<String>`, written by `record_search`)

**Interfaces:**
- Consumes: `Store::live_trails`, `Trail`, `Tier` (Task 8); `RankingParams.trail_weight`, `Core.trails_cfg` (Task 9); `Core::labels_count` (Task 7).
- Produces, in `src/core/trails.rs`:

```rust
pub struct TrailIndex { /* live trails of one embed model */ }
pub struct Matched { pub trail_id: String, pub artifact_id: String, pub query: String, pub similarity: f32, pub tier: Tier }

impl TrailIndex {
    /// The best trail per note at or above `match_at`, best first. `leave_out_own` drops a trail
    /// within OWN (0.999) of `query_vec`; `with_model` false drops model trails.
    pub fn matches(&self, query_vec: &[f32], match_at: f32, leave_out_own: bool, with_model: bool) -> Vec<Matched>;
}
pub const OWN: f32 = 0.999;

/// New order of `n` rows: row `pos` scores `1/(pos+2)`, plus `tier * weight / (tpos+2)` for a
/// row at place `tpos` among the matched. Stable. Returns the permutation.
pub fn reorder(n: usize, matched: &[(usize /*pos*/, usize /*tpos*/, Tier)], weight: f32) -> Vec<usize>;

impl Core {
    pub async fn trail_index(&self) -> Result<Arc<TrailIndex>>;   // loaded once, held on Core
    pub fn drop_trail_index(&self);                               // after any mint, withdraw, retire, forget
    pub fn trail_match_at(&self) -> f32;                          // line_above(trails_cfg.match_at, measured_line)
    pub fn model_trails_count(&self) -> bool;                     // cached labels_count
    pub async fn refresh_model_trails_count(&self);               // called from the Decisions tick
}
```

- Produces: `HitExplanation.trail: Option<TrailEffect>` with `pub struct TrailEffect { pub query: String, pub similarity: f32, pub moved: StageEffect }`, `skip_serializing_if = "Option::is_none"`.
- Behaviour in `search_inner`, in the spec's six steps. Skipped when `params.trail_weight == 0.0`, the vector is empty, or the index is empty. Leave-one-out when `origin.door == Door::Judge`. A matched note is followed forward through `superseded_by` up to 8 hops. Notes not in the pool are loaded with `vectors.payloads_of` and kept only if they pass `filter` (tags AND, category, status not trashed, superseded and deprecated per the include flags); they join after the cap with `similarity: None` and the pool's lowest `score`. After the reranker returns, matched rows it cut are appended after the kept rows in the reranker's order. Then `reorder` is applied. A recorded candidate carries `trail_id`.

- [ ] **Step 1: Write the failing tests.** Pure, in `src/core/trails.rs`:

```rust
#[test] fn a_person_trail_lands_near_third_and_a_model_trail_lower() {
    // 10 rows; the matched row is last (pos 9), the only match (tpos 0), weight 0.5
    assert_eq!(place_of(9, &reorder(10, &[(9, 0, Tier::Person)], 0.5)), 2);
    assert_eq!(place_of(9, &reorder(10, &[(9, 0, Tier::Model)], 0.5)), 6);
}
#[test] fn a_first_row_without_a_trail_is_never_passed() {
    assert_eq!(reorder(10, &[(9, 0, Tier::Person)], 1.0)[0], 0);
}
#[test] fn weight_zero_changes_nothing() { assert_eq!(reorder(4, &[(3,0,Tier::Person)], 0.0), [0,1,2,3]); }
#[test] fn a_replay_leaves_out_the_trail_made_from_its_own_query() {
    assert!(idx.matches(&own_vec, 0.8, true, true).is_empty());
    assert_eq!(idx.matches(&own_vec, 0.8, false, true).len(), 1);
}
#[test] fn a_vector_of_another_width_matches_nothing() { /* cosine is 0 on a width mismatch */ }
```

In `src/core/search.rs` tests (the fake embedder maps text to vectors; mint trails directly through `Store::mint_trail` with a vector equal to the later query's):

```rust
#[tokio::test] async fn a_note_the_store_does_not_return_is_found_through_its_trail() {
    let (hits, _) = core.search_with(&q("how do I flumph"), None, Door::Ui).await.unwrap();
    let h = hits.iter().find(|h| h.artifact_id == far_note).unwrap();
    assert_eq!(h.explanation.as_ref().unwrap().trail.as_ref().unwrap().query, "how do I flumph");
    assert!(!h.weak);
}
#[tokio::test] async fn with_trail_weight_zero_it_is_not_found() { /* ranking.trail_weight = 0.0 */ }
#[tokio::test] async fn at_the_judge_door_its_own_trail_does_not_find_it() { /* Door::Judge, same vector */ }
#[tokio::test] async fn a_matched_note_the_reranker_cut_is_still_in_the_results() { /* Door::Ask, a reranker that scores it last, limit 3 */ }
#[tokio::test] async fn a_trashed_note_is_not_joined_by_its_trail() {}                 // Review Focus 3
#[tokio::test] async fn a_note_outside_the_tag_filter_is_not_joined_by_its_trail() {} // Review Focus 3
#[tokio::test] async fn a_keyword_fallback_search_runs_no_trail_step() {}             // Review Focus 2: failing embedder, results equal the keyword hits
#[tokio::test] async fn a_model_trail_counts_on_a_base_nobody_judged_and_not_while_labels_are_stopped() {}
#[tokio::test] async fn a_recorded_candidate_carries_the_trail_that_brought_it() { /* search_candidates.trail_id */ }
```

- [ ] **Step 2: Run** `cargo test --lib core::trails core::search` — FAIL.
- [ ] **Step 3: Implement.** `TrailIndex` holds a flat `Vec<f32>` matrix plus row metadata; `matches` is one pass of dot products over normalised rows. `Core` holds `Arc<RwLock<Option<Arc<TrailIndex>>>>` and an `AtomicBool` for the cached count (initially true).
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib core:: eval:: jobs::tune` and `cargo test --features contained --lib core::search` — PASS.
- [ ] **Step 5: Commit** `feat(search): a note is found through a query it answered before, after the reranker and before priming`

### Task 11: Where trails come from, and how they go

**Files:**
- Modify: `src/core/trails.rs` (minting), and the call sites: the search verdict route's core path (callers of `Store::judge_hit`, `unjudge`, `gap_event`, `open_event` in `src/web/` — route them through new `Core` methods below), `src/core/ingest.rs:1234` (`judge_ask`) and the ask undo, `src/core/decide/verdict.rs` (`judge_decision`, `unjudge_decision`), `src/jobs/label.rs` (`label`), `src/store/search_labels.rs` (`model_expected`, `model_judged_pairs`)
- Modify: `src/core/recommend.rs:276` (`forget_situations`), `src/jobs/retention.rs`, `src/jobs/decisions.rs` (refresh the cached count; re-embed stale trails while quiet)

**Interfaces:**
- Consumes: Task 8 and Task 10.
- Produces:

```rust
impl Core {
    /// No-op unless `learn.enabled`, the vector is under the live embed model and non-empty.
    pub async fn mint_trail(&self, artifact_id: &str, query: &str, query_vec: &[f32], embed_model: &str,
                            source: TrailSource, source_id: &str) -> Result<()>;
    pub async fn withdraw_trails(&self, source: TrailSource, source_id: &str) -> Result<()>;
    /// What a search's outcome says to the trails its shown candidates carry.
    pub async fn trails_after_open(&self, event_id: &str, artifact_id: &str) -> Result<()>;   // confirm that candidate's trail
    pub async fn trails_after_hit(&self, event_id: &str, artifact_id: &str) -> Result<()>;    // confirm + mint Verdict
    pub async fn trails_after_gap(&self, event_id: &str) -> Result<()>;                       // miss every shown trail
    pub async fn trails_after_labels(&self, event_id: &str, labels: &[(String, f64)]) -> Result<()>; // model trails only: yes < ask_at → miss
}
pub const STALE_PER_PASS: i64 = 50;
```

- Behaviour, per the spec's two tables: `verdict` on a search hit; `ask` on an ask judged Right, one per cited note the answer used; `decide` on a decision that is not weak, judged with the right answer known, one per bearing note that agrees; `label` when the labeller's top candidate is relevant, the search is unjudged, and that candidate carries no model trail's `trail_id`. `model_expected` and `model_judged_pairs` skip a candidate whose `trail_id` names a model trail. Undo of each verdict calls `withdraw_trails`. `forget_situations` calls `Store::forget_trails` and drops the index. Retention calls `expire_model_trails(now - feedback.retain_days)`. The `Decisions` tick calls `refresh_model_trails_count`, and while quiet re-embeds up to `STALE_PER_PASS` stale trails with `embedder.embed_query` on the `background_light` lane.

- [ ] **Step 1: Write the failing tests** (in the module that owns each call site):

```rust
#[tokio::test] async fn a_search_judged_a_hit_leaves_a_person_trail_and_undo_retires_it() {}
#[tokio::test] async fn an_ask_judged_right_leaves_a_trail_on_each_note_it_used() {}
#[tokio::test] async fn a_decision_judged_right_leaves_a_trail_on_the_notes_that_agree_and_a_weak_one_leaves_none() {}
#[tokio::test] async fn the_labellers_find_in_an_unjudged_search_is_a_model_trail() {}
#[tokio::test] async fn a_candidate_a_model_trail_brought_makes_no_trail_and_no_sweep_pair_when_labelled_relevant() {
    assert!(core.store.live_trails(M).await.unwrap().iter().all(|t| t.id == original));
    assert!(core.store.model_judged_pairs(&generation, at, v, model, 10).await.unwrap().is_empty());
}
#[tokio::test] async fn a_model_trail_labelled_unhelpful_three_times_is_retired_and_a_person_trail_is_not() {}
#[tokio::test] async fn opening_the_note_a_trail_brought_confirms_the_trail() { /* matched == 1, used_at moved */ }
#[tokio::test] async fn nothing_here_counts_against_every_trail_shown() {}
#[tokio::test] async fn deleting_captured_searches_deletes_every_trail() {}
#[tokio::test] async fn with_learning_off_no_trail_is_made() {}
#[tokio::test] async fn a_trail_under_the_old_embed_model_is_embedded_again_when_quiet() {}
```

- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.** Every mint, withdraw, retire and forget ends in `drop_trail_index`.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib` — PASS.
- [ ] **Step 5: Commit** `feat(trails): a verdict, a cited answer, a judged decision and the labeller's find each leave a trail; use confirms one and the labeller may withdraw the model's`

### Task 12: Trails on the page

**Files:**
- Modify: the rail's "why" template and its view model (find them with `grep -rn "recalled_via\|keyword" src/web/templates src/web/*.rs`; the trail line goes beside the keyword line)
- Modify: `src/web/insights.rs` (retrieval block), `docs/api.md:53` (the search hit's explanation)

**Interfaces:**
- Consumes: `HitExplanation.trail` (Task 10), `Store::trail_counts` (Task 8).
- Copy: the why line is `found before for «{query}»`. Insights: `trails: {live} on {notes} notes · {by_person} by you · {found_through} searches found a note through one`. The line is absent when `live == 0`.

- [ ] **Step 1: Write the failing tests:** the rendered why of a hit with a trail contains `found before for «how do I flumph»` and escapes HTML in the query; Insights with one person trail contains `trails: 1 on 1 notes · 1 by you`; the Android fixture check in `src/web/android_fixtures.rs` still passes.
- [ ] **Step 2: Run** `cargo test --lib web::` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --lib web::`, then delete `assets/app.css` and run `oc-check .` — PASS.
- [ ] **Step 5: Commit** `feat(ui): a hit found through a trail says which query found it before; Insights counts trails`

---

## Part B: The deep look

### Task 13: The stage and its rows

**Files:**
- Create: `src/store/deep_looks.rs`; register in `src/store/mod.rs`
- Modify: `src/store/schema.sql` (`deep_looks` as in the spec; `deep_look_failures(source_kind TEXT NOT NULL, source_id TEXT NOT NULL, question_version INTEGER NOT NULL, model TEXT NOT NULL, refused INTEGER NOT NULL, retry_at INTEGER NOT NULL, PRIMARY KEY (source_kind, source_id))`)
- Modify: `src/store/jobs.rs` (`Stage::Deepen`: `ALL` to 27, `as_str` "deepen", `parse`, `class` 1, `calls_a_generator` false, `reads_a_capture` false, the count tests), `src/jobs/mod.rs` (dispatcher arm calling `deepen::run`, stubbed `Ok(())` until Task 14), `src/core/ingest.rs:1636` (the refusing arm), `src/web/ui.rs:148` (`sweep_label`: "Looking deeper")
- Modify: `src/store/gaps.rs` (`trim_gap_coverage` also trims `deep_looks` and `deep_look_failures` whose source row is gone)

**Interfaces (produced):**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeepKind { Gap(GapKind), WeakRight, SecondReading, Labelled }   // as_str: the gap kind's, "weak_right", "second_reading", "labelled"
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeepSource { pub kind: DeepKind, pub id: String }
impl DeepSource { pub fn target(&self) -> String /* "kind:id" */; pub fn parse(target: &str) -> Option<Self> }

impl Store {
    pub async fn deep_looked(&self, s: &DeepSource, version: u32, model: &str) -> Result<bool>;   // a done row ('' artifact) in this era
    pub async fn deep_asked(&self, s: &DeepSource, version: u32, model: &str) -> Result<Vec<String>>; // artifact ids already asked
    /// One transaction: every answer, then the done row; clears the source's failure row.
    pub async fn put_deep_look(&self, s: &DeepSource, answers: &[(String, f64)], version: u32, model: &str) -> Result<()>;
    pub async fn deep_look_failed(&self, s: &DeepSource, version: u32, model: &str, refused: bool, retry_at: i64) -> Result<i64>;
    pub async fn deep_look_due(&self, s: &DeepSource, version: u32, model: &str) -> Result<bool>; // no failure row past LABEL_GIVE_UP or with retry_at in the future
    pub async fn deep_counts(&self, relevant_at: f64, version: u32, model: &str) -> Result<(i64 /*looks*/, i64 /*found*/)>;
}
```

- [ ] **Step 1: Write the failing tests** in `src/store/deep_looks.rs`: `put_deep_look` then `deep_looked == true` and `deep_asked` lists the answers; another era reads `false`; a failure row past `LABEL_GIVE_UP` makes `deep_look_due == false`; `DeepSource::parse(&s.target()) == Some(s)` for every kind; trimming removes rows of a deleted search. Update `Stage::ALL.len() == 27` and the position tests (`src/store/jobs.rs:1910-1936`, `src/jobs/read.rs:570`, `src/jobs/label.rs:697`); `every_stage_the_queue_can_run_has_a_word_for_it` must pass.
- [ ] **Step 2: Run** `cargo test --lib store::deep_looks store::jobs web::ui` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib` — PASS.
- [ ] **Step 5: Commit** `feat(jobs): a Deepen stage and the rows a deep look leaves`

### Task 14: One deep look

**Files:**
- Create: `src/jobs/deepen.rs`; register in `src/jobs/mod.rs` and wire the dispatcher arm

**Interfaces:**
- Consumes: Task 13; `label::answers(core, query, note) -> Result<Option<f64>>` and `label::relevant_at` (`src/jobs/label.rs:22-56`); `Core::mint_trail` (Task 11); `Store::cover_gap`, `reject_gap_note`, `open_gaps`; `eval::sweep` replay (`remember_query_vector` + `search_with_ranking` at `Door::Judge`).
- Produces:

```rust
pub async fn arm(core: &Core, s: &DeepSource) -> Result<()>;     // rearm_idle_seq(Stage::Deepen, "deep", &s.target(), 0)
pub async fn run(core: &Core, target: &str) -> Result<()>;
/// What a source is asked with: its text, its stored vector, and the notes to read first.
pub(crate) struct Brief { pub text: String, pub vec: Vec<f32>, pub first: Vec<String>, pub pool: bool }
pub(crate) async fn brief(core: &Core, s: &DeepSource) -> Result<Option<Brief>>;   // None: the source is gone or under another embed model
pub(crate) async fn pool(core: &Core, s: &DeepSource, b: &Brief) -> Result<Vec<String>>;
pub const REPLAY_LIMIT: usize = 50;
pub const LINKED_OF: usize = 5;
```

- Behaviour:
  - `brief`: `Gap(k)` → the gap's text and vector; for `Gap(Decide)` `first` is the decision's joint evidence. `WeakRight` → the decision's `q`, vector, `first` = joint evidence, `pool: true`. `SecondReading` → the same with `pool: false`. `Labelled` → the search's query and vector, `first` empty, `pool: true`.
  - `pool`: `first`, then (when `b.pool`) the replay's results at `REPLAY_LIMIT`, then the notes linked to the replay's first `LINKED_OF`; minus `deep_asked`, the source's `search_labels` artifacts, its `gap_rejections`; only `in_results()` notes; cut to `decide_cfg.deep_max`. A note `label::answers` returns `None` for (does not fit whole) is skipped and not counted.
  - `run`: `brief` is `None` → `Ok(())`, nothing stored (Review Focus 5). Otherwise ask in order, stop at the first `yes >= relevant_at`, then `put_deep_look`. For `SecondReading` every `first` note is asked and there is no early stop. On a call error: store nothing, `deep_look_failed`, and return as `label::run` does (`Ok` at the give-up count, else `Err`).
  - A find, by kind: `Gap(k)` → `mint_trail(.., TrailSource::Deep, &s.target())` and `cover_gap(k, id, corpus_of_note, note, yes as f32, CoveredBy::Model)`. `WeakRight`, `Labelled` → the trail only. `SecondReading` → nothing.
  - No find on a `Gap`: each answer also goes to `reject_gap_note`.

- [ ] **Step 1: Write the failing tests** in `src/jobs/deepen.rs` (scripted decider: `answers_query` yes 0.95 when the state contains a marker word, else 0.05; build the base so the marked note is outside the first ten of the replay):

```rust
#[tokio::test] async fn a_gap_whose_answer_search_did_not_show_is_covered_and_leaves_a_trail() {
    run(&core, &s.target()).await.unwrap();
    assert!(open_gap_ids(&core).await.is_empty());
    let t = core.store.live_trails(M).await.unwrap();
    assert_eq!((t.len(), t[0].tier, t[0].artifact_id.as_str()), (1, Tier::Model, marked.as_str()));
}
#[tokio::test] async fn a_second_run_asks_nothing() { /* decider.calls() unchanged */ }
#[tokio::test] async fn nothing_found_leaves_the_gap_and_rejections_so_coverage_does_not_ask_again() {}
#[tokio::test] async fn the_look_stops_at_its_first_find_and_asks_at_most_deep_max() {}
#[tokio::test] async fn a_weak_decision_judged_right_gets_a_trail_and_no_coverage() {}
#[tokio::test] async fn a_second_reading_reads_the_evidence_only_and_makes_nothing() {
    assert_eq!(decider.calls(), 3); assert!(core.store.live_trails(M).await.unwrap().is_empty());
}
#[tokio::test] async fn a_linked_note_outside_the_replay_is_in_the_pool() {}
#[tokio::test] async fn a_source_deleted_before_the_look_ends_quietly() {                 // Review Focus 5
    assert!(run(&core, "labelled:gone").await.is_ok()); assert_eq!(decider.calls(), 0);
}
#[tokio::test] async fn a_failed_call_stores_nothing_and_is_counted() {}
```

- [ ] **Step 2: Run** `cargo test --lib jobs::deepen` — FAIL.
- [ ] **Step 3: Implement.** Calls go through `label::answers`, which already takes the `background_light` lane.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib jobs::` — PASS.
- [ ] **Step 5: Commit** `feat(deepen): a deep look asks about notes search did not show, and a find covers the gap and leaves a trail`

### Task 15: What gets a deep look, and when

**Files:**
- Modify: `src/jobs/decisions.rs:145-150` and a new `deepen` function beside `label`
- Modify: `src/core/judging/mod.rs:211-238` (`judging_queued` counts a queued `Deepen` unit at `decide_cfg.deep_max`)
- Modify: `src/store/deep_looks.rs` (the source queries), `src/store/decisions.rs`, `src/store/search_labels.rs`

**Interfaces:**
- Produces:

```rust
impl Store {
    pub async fn weak_right_decisions(&self, embed_model: &str, limit: i64) -> Result<Vec<String>>;
    /// Unjudged, not weak, not `not_held`, under this decider and BEARS_VERSION, oldest first.
    pub async fn decisions_unread(&self, decider: &str, embed_model: &str, limit: i64) -> Result<Vec<String>>;
    /// Labelled in this era, no candidate at or above `relevant_at`, verdict not 'hit', live embed
    /// model; those with a gave_up observation first, then oldest.
    pub async fn searches_left_out(&self, relevant_at: f64, version: u32, model: &str, embed_model: &str, limit: i64) -> Result<Vec<String>>;
}
pub const DEEPEN_LOOKAHEAD: i64 = 16;
async fn deepen(core: &Core, share: usize) -> Result<u64>;   // in src/jobs/decisions.rs
```

- Behaviour: `run` ends with `r.deepened = deepen(core, left / 4).await?` after `label`. `deepen` returns 0 unless `learn.enabled`, and unless `scan`'s quiet test holds (no `activity_since`, no `captured_since` within `evolve.idle_secs`). It walks sources in the spec's order — open gaps (`open_gaps`), weak-right, second readings, left-out searches — skipping one that is `deep_looked`, not `deep_look_due`, or already queued; each armed look costs `decide_cfg.deep_max` (a second reading costs its evidence count); the first may overrun the share when nothing is in flight, as `label` does.

- [ ] **Step 1: Write the failing tests** in `src/jobs/decisions.rs`:

```rust
#[tokio::test] async fn an_open_gap_is_armed_for_a_deep_look_when_quiet() { assert_eq!(deepen(&core, 60).await.unwrap(), 1); }
#[tokio::test] async fn nothing_is_armed_while_someone_searched_inside_the_idle_window() { assert_eq!(deepen(&core, 60).await.unwrap(), 0); }
#[tokio::test] async fn gaps_go_before_second_readings_and_those_before_left_out_searches() {}
#[tokio::test] async fn a_source_already_looked_at_in_this_era_is_passed() {}
#[tokio::test] async fn a_queued_deep_look_is_counted_in_the_tick_budget() { /* judging_queued rises by deep_max */ }
#[tokio::test] async fn a_search_the_labeller_left_out_qualifies_and_one_with_a_relevant_candidate_does_not() {}
```

- [ ] **Step 2: Run** `cargo test --lib jobs::decisions` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib jobs:: core::judging` — PASS (the labeller's pacing tests `at_the_defaults_one_whole_search_is_labelled_per_tick` and `labelling_takes_at_most_a_quarter_of_the_tick` must be unchanged).
- [ ] **Step 5: Commit** `feat(deepen): open gaps, weak decisions and searches the labeller left out are looked at again when the base is quiet`

### Task 16: What a deep look teaches the sweep and the line

**Files:**
- Modify: `src/eval/sweep.rs:356-388` (`evidence_pairs`), `src/store/deep_looks.rs`
- Modify: `src/store/decisions.rs` (`bears_evidence`), `src/jobs/tune.rs` (use it)
- Modify: `src/web/insights.rs`

**Interfaces:**
- Produces: `impl Store { pub async fn deep_found_pairs(&self, generation_id: &str, relevant_at: f64, version: u32, model: &str, limit: usize) -> Result<Vec<_>> }` returning exactly the row type `model_judged_pairs` returns (`src/store/search_labels.rs:346`) — `Labelled` sources of unjudged searches of this generation with a found note; the served rank is `None`.
- Produces: `impl Store { pub async fn bears_evidence(&self, model: &str, relevant_at: f64, ask_at: f64, with_readings: bool) -> Result<BearsEvidence> }` — `bears_verdicts` (Task 6) plus, when `with_readings`, unjudged decisions since the last move that have a done deep-look row of kind `second_reading` or gap kind `decide`: not weak and no joint-evidence note with `yes >= ask_at` → `too_low`; weak and a joint-evidence note with `yes >= relevant_at` → `too_high`; otherwise `fine`. `tune_questions` calls it with `with_readings = core.labels_count().await?`.
- Behaviour: `evidence_pairs` appends `deep_found_pairs` after `model_judged_pairs`, inside the same `room`, under the same `labels_count()` condition.

- [ ] **Step 1: Write the failing tests:**

```rust
#[tokio::test] async fn a_deep_find_for_an_unjudged_search_is_a_pair_the_sweep_scores() {
    // pairs: the `Vec<Pair>` out of `evidence_pairs(&core, &generation)`
    assert!(pairs.iter().any(|p| p.satisfies.contains(&found) && p.served_rank.is_none()));
}
#[tokio::test] async fn that_pair_is_not_found_through_the_trail_its_own_query_left() { /* rank_of under trail_weight 1.0 is None */ }
#[tokio::test] async fn four_of_thirty_one_unjudged_weak_decisions_with_a_relevant_note_lower_the_line() {
    assert_eq!(tune_questions(&core).await.unwrap(), 1); assert_eq!(core.bears_at().await, 0.48);
}
#[tokio::test] async fn a_verdict_replaces_the_second_reading_of_its_decision() {}
#[tokio::test] async fn second_readings_count_for_nothing_while_the_labels_are_stopped() {}
```

- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.** Insights: `deep looks: {looks} · {found} found a note search had missed`, absent when `looks == 0`.
- [ ] **Step 4: Regenerate `.sqlx/`; run** `cargo test --lib` — PASS.
- [ ] **Step 5: Commit** `feat(tune): what a deep look finds is a pair for the sweep, and a second reading of unjudged decisions moves the bearing line`

---

### Task 17: Documents and the whole gate

**Files:**
- Modify: `docs/evaluation.md` (a section on trails and the deep look; the knob table gains "Trail weight *(ladder)* · `[trails] weight`"; the labels paragraph already changed in Task 7), `docs/api.md` (search hit explanation `trail`; decide `weak`), `config.example.toml` (`[trails]`, `decide.weak_take`, `decide.deep_max`, with the spec's comments), `CHANGELOG.md`, `README.md`
- Modify: `docs/superpowers/specs/2026-11-13-decide-1-verb-design.md` — one line under its anti-goal and its step 4 pointing at the 2026-11-16 spec that supersedes them

- [ ] **Step 1: Write the documents.** No behaviour is described that a test in Tasks 1–16 does not pin.
- [ ] **Step 2: Run every command in AGENTS.md:**

```bash
cargo test && cargo test --features contained --lib
cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings
cargo deny check
DATABASE_URL=sqlite://target/sqlx-prepare.db cargo sqlx prepare --check -- --all-targets --features contained
node --test assets/js/*.test.mjs
rm -f assets/app.css && oc-check .
cargo build && sh scripts/test-install.sh
```

Expected: all pass. With `ENGRAM_CHROME` set also `cargo test --lib -- --ignored` and `cargo test --test browser_ask -- --ignored`; if it is not set, say so in the hand-over.

- [ ] **Step 3: Walk `manifest/99-review-checklist.md`** from the dev manifest v2026.11.1 and the spec's "Done when" list, line by line; each "Done when" line names the test that proves it.
- [ ] **Step 4: Commit** `docs: trails, the deep look and the soft gate in the evaluation guide, the API page and the example config`

## Not covered by any test here

No decision model is reachable from the build environment. `weak_take`, `match_at` and `deep_max` are estimates, and whether trails and deep looks improve results on a real base is unmeasured. The hand-over must say so and name `--replay-decisions` and Insights' new lines as where the owner can look.
