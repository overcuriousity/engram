# Decide Core and REST (spec 1, stage 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `Core::decide` turns a typed question into a verdict with its evidence, records it where an ask would be recorded, lets a person's verdict on it improve the base, and serves it over `/api/v1/decide`, `/api/v1/decide/stream` and the verdict route.

**Architecture:** A new module `src/core/decide/` mirrors `src/core/ask/`: a stream of events ending in one `Decision`, with `Core::decide` collecting it. The pipeline is the spec's six steps: retrieve, prior, single reads (in a `JoinSet`, the client's semaphore bounds them), evidence, joint read (swapped), standing. Standing and state layout are pure functions. Storage is two new tables in `schema.sql`.

**Tech Stack:** Rust, `async-stream`, `tokio::task::JoinSet`, axum, sqlx (SQLite, compile-time queries), serde.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-1-verb-design.md`, sections "How a decision is made", "What a verdict does", "REST" and "What is stored". Part 2 of 4 on branch `feat/decide`; part 1 (`2026-11-13-decide-1-foundation.md`) is done before this starts.

## Global Constraints

- Defaults: `limit` 8 (1 to 16); `[decide] joint_tokens = 2500`, `bears_min = 0.5`, `settled_at = 0.80`, `split_at = 0.34`.
- The reserved key is `bears`; its instructions are exactly `Does this excerpt contain information that helps answer the question: «{q}»?`.
- A caller sends at most 8 questions. `situation` is at most 2000 tokens. With no situation the prior's state is exactly `No information.`.
- Nothing bearing means no verdict: standing `not_held`, `answers` null, no joint read, whatever the model would say.
- There is no Keep (owner's decision, 2026-11-13). Nothing here writes text into the base.
- A verdict never deletes, retires or rewrites a note. It stamps, queues a pair for the judge, or opens and closes a gap.
- Recorded only where an ask is: `learn.enabled` and door `Ui`, `Cli` or `App`.
- Decision calls hold `gate.interactive()` for the whole decision and never observe the cooldown.
- The role is required: no route answers 404 for a missing role.
- Every query is compile-time (`sqlx::query!`) unless assembled at run time, and then carries a `// runtime:` reason. After any schema or query change: `cargo run -- --dev-schema target/sqlx-prepare.db`, then `DATABASE_URL=sqlite://target/sqlx-prepare.db cargo sqlx prepare -- --all-targets --features contained`; commit `.sqlx/`.
- Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs with `-D warnings`.

## Decisions this plan makes

- Option order is meaning. The request's `questions` and each `criteria` are read by a serde visitor into `Vec`s in document order, never through `serde_json::Map`; answers are written with `serialize_map` in option order.
- Response shapes: `choice` → `{"type","choice","probabilities":{name: p},"confidence","swapped_agree"}`; `score` → `{"type","score","probabilities":[p…],"confidence"}`; `noul` → `{"type","noul"}`. `swapped_agree` appears only on joint choice answers.
- `evidence` lists only passages that bear, strongest first; `n` is that position. `read` is how many passages were read. Passages that did not bear are `read - evidence.len()`.
- Stream events are the spec's four. The "Weighing N together" line needs no event: each `read` frame says whether it bears.
- Retrieval asks search for `min(limit * 2, 50)`, drops retired hits, and keeps the first `limit`.
- One failed read fails the decision. A partial verdict would be a guess.
- A read's pick: `choice` its pick; `noul` yes when `yes >= 0.5`; `score` the level with the largest probability, the lower on a tie.
- Gaps get their own kind, `GapKind::Decide`: a decision that is `not_held`, or judged `nothing_here`, is an open gap until dismissed or covered. It does not pose as an ask.
- Verdicts reuse `AskVerdict` (`right`, `wrong`, `nothing_here`).
- A verdict's effects use the first question when a decision has several: the web UI has one, and an API caller with several gets the verdict stored and the first question's effects.
- A queued pair is `record_pair_with_detail(a, b, 0.0, "decide: read against a confirmed answer")`, as a pair found through a link is recorded.
- Every passage read is stored, bearing or not; the API's `evidence` still lists only those that bear.
- Recorded decisions expire and purge with recorded asks.
- Excerpt dates are `crate::fmt::fmt_day` of the artifact's `created_at`.

## Review Focus

1. A passage longer than the single-read budget: it is cut to fit, not refused. Task 3, `a_passage_longer_than_the_window_is_cut_not_refused`.
2. Option names that arrive in an order `serde_json::Map` would sort (`"switch"`, `"keep"`): forward order on the wire is the caller's. Task 6, `criteria_keep_the_callers_order`.
3. A caller question keyed `bears`, a ninth question, a 2001-token situation, an empty `q`: each a 400 naming the problem, and no model call. Task 3 and Task 6.
4. The reader disconnects mid-decision: the reads still in flight are aborted and the lease is released. Task 3, `a_dropped_stream_stops_reading_and_frees_the_lane`.

5. A second press, an undo, a verdict on a decision that was not held, a `correct` that names no option: each has one defined result. Task 5.

## File Structure

| File | Responsibility |
|---|---|
| `src/config.rs` (modify) | `DecideConfig` (`[decide]`) |
| `src/core/mod.rs` (modify) | `Core.decide_cfg`, `pub mod decide` |
| `src/core/decide/mod.rs` (create) | `DecideRequest`, `Decision`, `Evidence`, `Verdict`, `Core::decide`, `Core::decide_events`, recording |
| `src/core/decide/standing.rs` (create) | `Standing`, `standing()`, `top()`, `pick()` |
| `src/core/decide/state.rs` (create) | `BEARS_KEY`, `bears_question`, `prior_state`, `single_state`, `joint_state` |
| `src/core/decide/stream.rs` (create) | `DecideEvent`, `PassageRead` |
| `src/core/decide/wire.rs` (create) | the JSON in and out: `DecideBody`, `Serialize` for `Decision`, `Answer` JSON |
| `src/core/decide/verdict.rs` (create) | the right answer, who agrees, `Core::judge_decision`, `Core::unjudge_decision` |
| `src/infer/fake.rs` (modify) | `RuleDecider` |
| `src/store/schema.sql`, `src/store/decisions.rs` (create), `src/store/mod.rs` | the two tables and their functions |
| `src/store/artifacts.rs` (modify) | `created_at_among` |
| `src/store/gaps.rs`, `src/store/feedback.rs` (modify) | `GapKind::Decide`; expire and purge |
| `src/web/api.rs`, `src/web/client.rs` (modify) | routes |
| `docs/api.md`, `config.example.toml`, `CHANGELOG.md` (modify) | docs |

---

### Task 1: `[decide]` config and the pure parts

**Files:**
- Modify: `src/config.rs`, `config.example.toml`, `src/core/mod.rs` (field `pub decide_cfg: crate::config::DecideConfig` beside `learn`, filled in `from_config` and the test `build`)
- Create: `src/core/decide/mod.rs` (module root only, for now), `src/core/decide/standing.rs`, `src/core/decide/state.rs`

**Interfaces:**
- Consumes: `Question`, `Kind`, `Answer` (part 1); `TokenCounter::{count, cut}`.
- Produces:
  ```rust
  // config.rs
  #[derive(Debug, Deserialize, Clone)] #[serde(default)]
  pub struct DecideConfig { pub joint_tokens: usize, pub bears_min: f64, pub settled_at: f64, pub split_at: f64 }
  // Default: 2500, 0.5, 0.80, 0.34.  Config: #[serde(default)] pub decide: DecideConfig

  // standing.rs
  #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum Standing { Settled, Leaning, Split, NotHeld }
  impl Standing { pub fn as_str(self) -> &'static str; }
  /// The largest probability; for a noul the larger of yes and no.
  pub fn top(a: &Answer) -> f64;
  /// Which answer this is, as an index: option, level, or 0 for yes and 1 for no.
  pub fn pick(q: &Question, a: &Answer) -> usize;
  /// `reads`: each evidence passage's bearing and its own answer to this question.
  pub fn standing(q: &Question, joint: &Answer, swapped_agree: Option<bool>, reads: &[(f64, &Answer)], cfg: &DecideConfig) -> Standing;

  // state.rs
  pub const BEARS_KEY: &str = "bears";
  pub const NO_INFORMATION: &str = "No information.";
  pub fn bears_question(q: &str) -> Question;
  pub struct Excerpt<'a> { pub title: Option<&'a str>, pub day: &'a str, pub text: &'a str }
  pub fn prior_state(situation: Option<&str>) -> String;                       // "Situation:\n{s}" or NO_INFORMATION
  pub fn single_state(situation: Option<&str>, e: &Excerpt<'_>) -> String;     // situation block, blank line, "Excerpt: {title} (captured {day})\n{text}"
  /// Whole excerpts, strongest first, while they fit `tokens`; returns the state and how many went in.
  /// The first is cut to fit if it alone is too large.
  pub fn joint_state(situation: Option<&str>, excerpts: &[Excerpt<'_>], counter: &TokenCounter, tokens: usize) -> (String, usize);
  ```
  An untitled excerpt's heading is `Excerpt 1 (captured 2026-09-14)`.

- [ ] **Step 1: Write the failing tests**

```rust
// config.rs
#[test] fn decide_thresholds_default_to_the_spec() { /* MINIMAL: 2500, 0.5, 0.80, 0.34 */ }
#[test] fn a_threshold_outside_zero_to_one_is_refused() { /* [decide] settled_at = 1.2 -> Invalid naming decide.settled_at; same for bears_min, split_at; joint_tokens = 100 -> Invalid */ }

// standing.rs — cfg = DecideConfig::default(); c2 = choice [keep, switch]
#[test] fn top_and_pick_read_each_kind() {
    // choice [0.81, 0.19] pick keep -> top 0.81, pick 0;  noul yes 0.3 -> top 0.7, pick 1;  noul 0.5 -> pick 0
    // score probabilities [0.1, 0.45, 0.45] -> top 0.45, pick 1
}
#[test] fn eighty_is_settled_and_just_under_is_leaning() { /* joint keep 0.80, reads all keep -> Settled; 0.79 -> Leaning */ }
#[test] fn swapped_picks_that_disagree_split() { /* joint 0.95, swapped_agree Some(false) -> Split */ }
#[test] fn a_third_of_the_bearing_against_splits() {
    // reads: (0.9, keep), (0.9, keep), (0.93, switch): against 0.93 / 2.73 = 0.3407 >= 0.34 -> Split even at joint 0.95
    // reads: (0.9, keep), (0.9, keep), (0.9, switch): 0.3333 -> not split -> Settled
}
#[test] fn no_reads_is_judged_on_the_joint_answer_alone() { /* reads empty, 0.85 -> Settled */ }

// state.rs
#[test] fn the_bears_question_quotes_the_question() {
    let b = bears_question("Should I keep bge-m3?");
    assert_eq!(b.key, "bears"); assert_eq!(b.kind, Kind::Noul);
    assert_eq!(b.instructions, "Does this excerpt contain information that helps answer the question: «Should I keep bge-m3?»?");
}
#[test] fn the_prior_has_a_fixed_state_without_a_situation() { assert_eq!(prior_state(None), "No information."); assert_eq!(prior_state(Some("12 GB.")), "Situation:\n12 GB."); }
#[test] fn the_joint_state_is_laid_out_as_the_spec_draws_it() {
    // situation "The GPU has 12 GB.", excerpts ("Reindex notes", "2026-09-14", "a"), (None, "2026-10-01", "b"), tokens large ->
    // "Situation:\nThe GPU has 12 GB.\n\nExcerpt 1: Reindex notes (captured 2026-09-14)\na\n\nExcerpt 2 (captured 2026-10-01)\nb", 2
}
#[test] fn excerpts_that_do_not_fit_are_left_out_whole() { /* three of ~40 tokens, tokens = 100 -> packed 2, the third's text absent */ }
#[test] fn a_first_excerpt_too_large_alone_is_cut() { /* one of 5000 tokens, tokens 500 -> packed 1, counter.count(state) <= 500 */ }
#[test] fn without_a_situation_there_is_no_situation_block() { /* joint and single start with "Excerpt" */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::decide config::tests::decide_thresholds config::tests::a_threshold`
Expected: compile error.

- [ ] **Step 3: Implement** `DecideConfig` with validation in `Config::validate` (each threshold in `0.0..=1.0`, `joint_tokens >= 256`), the commented `[decide]` block in `config.example.toml` copied from the spec, the `Core` field, and the two modules.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib core::decide && cargo test --lib config::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs config.example.toml src/core
git commit -m "feat(decide): thresholds, standing and the state the model reads"
```

---

### Task 2: Storage

**Files:**
- Modify: `src/store/schema.sql` (after `ask_subjects`), `src/store/mod.rs` (`pub mod decisions;`), `src/store/feedback.rs` (~844, ~868), `src/store/artifacts.rs`
- Create: `src/store/decisions.rs`
- Regenerate: `.sqlx/`

**Interfaces:**
- Produces:
  ```sql
  CREATE TABLE IF NOT EXISTS decide_events (
    id TEXT PRIMARY KEY,
    question TEXT NOT NULL,
    questions TEXT NOT NULL,          -- System One's shape
    situation TEXT,
    scope TEXT,
    filters TEXT NOT NULL DEFAULT '{}',
    answers TEXT,                     -- NULL when not held
    prior TEXT NOT NULL,
    standing TEXT NOT NULL,           -- {key: standing}
    not_held INTEGER NOT NULL,
    model TEXT NOT NULL,
    read INTEGER NOT NULL,
    dropped INTEGER NOT NULL,
    query_vec BLOB NOT NULL,
    vec_dim INTEGER NOT NULL,
    embed_model TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    judged_at INTEGER,
    verdict TEXT,
    correct TEXT,                     -- {key: answer}, the right answer when known
    dismissed_at INTEGER
  );
  CREATE INDEX IF NOT EXISTS idx_decides_created ON decide_events(created_at);
  CREATE INDEX IF NOT EXISTS idx_decides_open ON decide_events(not_held, verdict, dismissed_at);
  CREATE TABLE IF NOT EXISTS decide_evidence (
    event_id TEXT NOT NULL REFERENCES decide_events(id) ON DELETE CASCADE,
    rank INTEGER NOT NULL,            -- retrieval order; every passage read has a row
    artifact_id TEXT NOT NULL,
    bears REAL NOT NULL,
    bearing INTEGER NOT NULL,
    joint INTEGER NOT NULL,
    n INTEGER,                        -- place in the evidence; NULL when it did not bear
    read TEXT NOT NULL,               -- {key: answer}
    PRIMARY KEY (event_id, rank)
  );
  ```
  ```rust
  pub struct NewEvidence { pub rank: usize, pub artifact_id: String, pub bears: f64, pub bearing: bool, pub joint: bool, pub n: Option<usize>, pub read: String }
  pub struct NewDecision {
      pub question: String, pub questions: String, pub situation: Option<String>, pub scope: Option<String>,
      pub filters: String, pub answers: Option<String>, pub prior: String, pub standing: String, pub not_held: bool,
      pub model: String, pub read: usize, pub dropped: usize, pub query_vec: Vec<f32>, pub embed_model: String,
      pub evidence: Vec<NewEvidence>,
  }
  pub struct StoredEvidence { pub rank: i64, pub artifact_id: String, pub bears: f64, pub bearing: bool, pub joint: bool, pub n: Option<i64>, pub read: String }
  pub struct StoredDecision {
      pub id: String, pub question: String, pub questions: String, pub answers: Option<String>, pub standing: String,
      pub not_held: bool, pub created_at: i64, pub verdict: Option<AskVerdict>, pub correct: Option<String>,
      pub evidence: Vec<StoredEvidence>,
  }
  impl Store {
      pub async fn record_decision(&self, d: NewDecision) -> Result<String>;           // one transaction
      pub async fn decision(&self, id: &str) -> Result<Option<StoredDecision>>;
      pub async fn judge_decision(&self, id: &str, verdict: AskVerdict, correct: Option<&str>) -> Result<()>; // NotFound for an unknown id
      pub async fn unjudge_decision(&self, id: &str) -> Result<()>;                    // clears verdict and correct
      pub async fn expire_decisions(&self, retain_days: i64) -> Result<u64>;           // judged rows exempt
      pub async fn purge_decisions(&self) -> Result<u64>;
      pub async fn created_at_among(&self, artifact_ids: &[String]) -> Result<std::collections::HashMap<String, i64>>;   // artifacts.rs, runtime IN list
  }
  ```
  `expire_decisions` and `purge_decisions` are added to the sums at `feedback.rs:844` and `:868`.

- [ ] **Step 1: Write the failing tests** in `decisions.rs`, on `Store::memory()`, with a `fn sample() -> NewDecision` (three reads, two bearing):

```rust
#[tokio::test] async fn a_decision_is_read_back_as_recorded() { /* every field; three reads by rank, two bearing with n = 1, 2, one with n None */ }
#[tokio::test] async fn a_not_held_decision_has_no_answers_and_keeps_its_reads() { /* answers None, not_held true, the reads stored with bearing false */ }
#[tokio::test] async fn a_verdict_is_set_and_undone() { /* judge Wrong with correct -> both stored; unjudge -> both None */ }
#[tokio::test] async fn judging_an_unknown_decision_is_not_found() { /* Err(Error::NotFound) */ }
#[tokio::test] async fn old_unjudged_decisions_expire_and_judged_ones_stay() {}
#[tokio::test] async fn purging_takes_the_evidence_with_it() { /* decide_evidence is empty after purge_decisions */ }
#[tokio::test] async fn created_at_among_dates_the_artifacts_it_knows() {}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::decisions`
Expected: compile error.

- [ ] **Step 3: Implement**, following `src/store/asks.rs` for transaction and query style and `retired_among` (`corpora.rs:567`) for the IN lists.

- [ ] **Step 4: Regenerate `.sqlx/`** with the two commands in Global Constraints.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib store:: && cargo test --test csp_static`
Expected: PASS (`every_runtime_query_says_why` included).

- [ ] **Step 6: Commit**

```bash
git add src/store .sqlx
git commit -m "feat(store): recorded decisions and their evidence"
```

---

### Task 3: The pipeline

**Files:**
- Modify: `src/core/decide/mod.rs`, `src/infer/fake.rs`
- Create: `src/core/decide/stream.rs`

**Interfaces:**
- Consumes: Task 1; `decide_swapped`, `swapped_questions`, `budget`, `validate` (part 1); `Core::search_with(&SearchQuery, None, Door::Ask)`; `store.created_at_among`; `core.gate.interactive()`; `core.counter`.
- Produces:
  ```rust
  // fake.rs — answers by rule, so parallel reads need no ordering
  pub struct RuleDecider { /* … */ }
  impl RuleDecider {
      pub fn new(rule: impl Fn(&str, &[Question]) -> Result<Answers> + Send + Sync + 'static) -> Self;
      pub fn calls(&self) -> usize;
      pub fn requests(&self) -> Vec<(String, Vec<Question>)>;
      pub fn set_context_tokens(&self, n: usize);   // default 8192
  }

  // core/decide/mod.rs
  pub const DEFAULT_LIMIT: usize = 8;
  pub const MAX_LIMIT: usize = 16;
  pub const MAX_CALLER_QUESTIONS: usize = 8;
  pub const MAX_SITUATION_TOKENS: usize = 2000;
  #[derive(Debug, Clone, Default)]
  pub struct DecideRequest {
      pub q: String,
      pub questions: Vec<Question>,     // empty: one noul keyed "verdict"; empty instructions become q
      pub situation: Option<String>,
      pub limit: Option<usize>,
      pub tags: Vec<String>,
      pub category: Option<String>,
      pub explain: bool,
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct Verdict { pub answer: Answer, pub swapped_agree: Option<bool> }
  #[derive(Debug, Clone, PartialEq)]
  pub struct Evidence {
      pub n: usize, pub artifact_id: String, pub title: Option<String>, pub text: String,
      pub bears: f64, pub joint: bool, pub read: Vec<(String, Answer)>,   // in question order
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct Decision {
      pub question: String,
      pub questions: Vec<Question>,                 // as asked, defaults filled
      pub answers: Option<Vec<(String, Verdict)>>,  // None: not held
      pub prior: Vec<(String, Answer)>,
      pub standing: Vec<(String, Standing)>,
      pub evidence: Vec<Evidence>,
      pub read: usize,
      pub dropped: usize,
      pub model: String,                            // as the server named itself on the last call
      pub event_id: Option<String>,
      pub state: Option<String>,                    // with explain
      pub reads: Vec<PassageRead>,                  // every passage read, by rank; stored, not in the JSON
  }
  impl Decision { pub fn not_held(&self) -> bool; }

  // stream.rs
  #[derive(Debug, Clone)]
  pub struct PassageRead {
      pub rank: usize, pub of: usize, pub artifact_id: String, pub title: Option<String>,
      pub bears: f64, pub bearing: bool, pub read: Vec<(String, Answer)>,
  }
  #[derive(Debug, Clone)]
  pub enum DecideEvent { Retrieved { passages: usize }, Read(Box<PassageRead>), Done(Box<Decision>) }

  impl Core {
      pub async fn decide(&self, req: &DecideRequest, origin: impl Into<Origin>) -> Result<Decision>;
      pub fn decide_events<O: Into<Origin>>(&self, req: &DecideRequest, origin: O)
          -> impl tokio_stream::Stream<Item = Result<DecideEvent>> + 'static + use<O>;
  }
  ```
  Recording is Task 4; here `event_id` stays `None`.

The stream, in order:
1. Normalise and refuse: empty `q`; more than 8 questions; a key `bears`; `validate`; a situation over 2000 tokens. All `Error::Validation`, before the lease and before any call.
2. Take `Arc::new(core.gate.interactive())`; every spawned read holds a clone.
3. Retrieve as "Decisions this plan makes" says; `mark` as ask's first round passes it. Yield `Retrieved { passages }`.
4. Prior: `decider.decide(&prior_state(situation), &questions)`.
5. Single reads in a `JoinSet` (dropping it aborts them): state `single_state`, the excerpt text first cut with `counter.cut` to `budget(counter, context_tokens, questions + bears)` less the situation and a 32-token heading allowance; questions are the caller's plus `bears_question(q)`. Yield one `Read` per result as it arrives, `bearing = bears >= bears_min`.
6. Evidence: bearing passages, by `bears` descending, then by rank. None: yield `Done` with `answers: None`, every standing `NotHeld`, `evidence` empty, `prior` present, `read` set.
7. Joint read: `joint_state` into `min(joint_tokens, budget(counter, context_tokens, &swapped_questions(&questions)))`; `decide_swapped`. `dropped` is the evidence left out; `joint` marks what went in. `state` is kept when `explain`.
8. Standing per question over all evidence reads. Yield `Done`.

- [ ] **Step 1: Write the failing tests** in `src/core/decide/mod.rs`. Helpers: `async fn base(notes: &[(&str, &str)]) -> Core` captures titled notes into `test_core()` and drains the queue (as `learning_base` in `src/web/client.rs:815` does); `fn deciding(core: &mut Core, rule) -> Arc<RuleDecider>` installs the decider. A rule answers `bears` 0.95 when the state contains a marker word and 0.02 otherwise, and other questions as the test says; it can tell prior (`No information.`/no `Excerpt`), single (`Excerpt:`) and joint (`Excerpt 1`) apart by the state.

```rust
#[tokio::test] async fn a_held_question_returns_a_verdict_and_what_it_rests_on() {
    // 3 notes, 2 with the marker. noul default question; joint yes 0.9.
    // answers == Some([("verdict", Verdict { answer: Noul { yes: 0.9 }, swapped_agree: None })])
    // standing == [("verdict", Settled)]; evidence.len() == 2, n == 1, 2, both joint; read == 3; dropped == 0
    // each evidence.read holds "verdict" and not "bears"; prior holds "verdict"
}
#[tokio::test] async fn nothing_bearing_means_no_verdict_whatever_the_model_would_say() {
    // rule: bears 0.02 everywhere, every other answer 0.99
    // answers None, standing [("verdict", NotHeld)], evidence empty, read == 3, prior present
    // no request's state contains "Excerpt 1": the joint read was never made
}
#[tokio::test] async fn an_empty_base_is_not_held_after_the_prior_alone() { /* calls() == 1 */ }
#[tokio::test] async fn each_passage_is_read_alone_with_the_bears_question() {
    // every "Excerpt:" request carries exactly the caller's keys plus "bears", and one excerpt
}
#[tokio::test] async fn the_situation_leads_every_state() { /* situation Some("12 GB."): all requests start "Situation:\n12 GB." */ }
#[tokio::test] async fn evidence_is_ordered_by_bearing() { /* rule gives 0.6, 0.9, 0.7 by marker -> evidence bears [0.9, 0.7, 0.6] */ }
#[tokio::test] async fn a_choice_is_weighed_with_its_options_swapped() {
    // choice [keep, switch]; the joint request has keys q0, q0r; answers.verdict.swapped_agree == Some(true)
}
#[tokio::test] async fn evidence_past_the_joint_budget_is_dropped_and_counted() {
    // decide_cfg.joint_tokens = 300, 4 bearing notes of ~150 tokens -> dropped == 2, two evidence rows joint == false
}
#[tokio::test] async fn a_passage_longer_than_the_window_is_cut_not_refused() {
    // set_context_tokens(1024), one bearing note of 3000 words -> Ok, its single read's state fits 1024
}
#[tokio::test] async fn bad_requests_are_refused_before_any_call() {
    // q ""; 9 questions; a question keyed "bears"; situation of 2001 tokens -> each Err(Validation), calls() == 0
}
#[tokio::test] async fn a_question_without_instructions_asks_q() { /* choice with instructions "" -> requests carry q there */ }
#[tokio::test] async fn the_limit_defaults_to_eight_and_stops_at_sixteen() { /* 20 bearing notes: read == 8; limit Some(40): read == 16 */ }
#[tokio::test] async fn retired_notes_are_not_read() { /* retire one as an existing search test does; its text is in no state */ }
#[tokio::test] async fn a_failed_read_fails_the_decision() { /* rule errs on one excerpt -> Err; no Done event */ }
#[tokio::test] async fn explain_carries_the_joint_state() { /* state.unwrap() starts "Excerpt 1"; without explain None */ }
#[tokio::test] async fn events_arrive_retrieved_then_reads_then_done() { /* collect: Retrieved{3}, 3 × Read (of == 3), Done */ }
#[tokio::test] async fn a_decision_holds_the_interactive_lane() { /* as a_question_holds_the_interactive_lane_for_its_whole_answer, ask/mod.rs:871: the rule probes gate.background() with a timeout and never gets it */ }
#[tokio::test] async fn a_dropped_stream_stops_reading_and_frees_the_lane() {
    // rule sleeps 30 s on excerpts (paused clock); take Retrieved, drop the stream;
    // gate.background() is then granted, and calls() does not grow after the drop
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::decide`
Expected: compile error.

- [ ] **Step 3: Implement** `RuleDecider` (runs `check` first, as the other fakes do), `stream.rs`, and the pipeline with `async_stream::try_stream!` as `ask_events` (`src/core/ask/mod.rs:81`) is built.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib core::decide && cargo test --lib infer::fake`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/decide src/infer/fake.rs
git commit -m "feat(core): decide reads each passage alone, then the bearing ones together"
```

---

### Task 4: Recording, the JSON shape, and gaps

**Files:**
- Create: `src/core/decide/wire.rs`
- Modify: `src/core/decide/mod.rs`, `src/store/gaps.rs` and every `match` on `GapKind` the compiler names

**Interfaces:**
- Consumes: Task 2's store functions; `Core::cached_query_vector`; `Origin`, `Door`.
- Produces:
  ```rust
  // wire.rs
  /// The request body, in the caller's order.
  #[derive(Debug, serde::Deserialize)]
  pub struct DecideBody { /* q, questions, situation, limit, tags, category, explain */ }
  impl DecideBody { pub fn into_request(self) -> Result<DecideRequest>; }   // Error::Validation for a bad shape
  pub fn questions_json(questions: &[Question]) -> String;   // System One's shape; part 1's wire::questions_json
  pub fn answer_json(q: &Question, a: &Answer, swapped_agree: Option<bool>) -> serde_json::Value;  // never used to order a choice
  impl serde::Serialize for Decision { /* the spec's response; probabilities written in option order */ }

  // gaps.rs
  GapKind::Decide   // "decide"
  ```
  A question's wire shape: `type` is `choice` (criteria an object, name to description), `score` (criteria an array), `noul` (no criteria); `instructions` optional. A `choice` with an array, a `score` with an object, or an unknown type is a `Validation` naming the key.

  Recording, at the end of the stream before `Done`: only when `learn.enabled` and the door is `Ui`, `Cli` or `App`. `filters` is `{"tags","category","limit"}` as ask stores it; `scope` is the origin's. Every read is stored (`Decision.reads`). On success `event_id` is set. A held decision also marks its joint evidence cited, exactly as `record_ask` does for used citations (`store/asks.rs:132`: the `Source::Cited` observation in the same transaction, and `mark_artifacts_cited` after).

  A decision is an open gap of kind `decide` when `((not_held = 1 AND COALESCE(verdict, '') <> 'wrong') OR (not_held = 0 AND verdict = 'nothing_here')) AND dismissed_at IS NULL AND embed_model = ? AND vec_dim > 0` and no `gap_coverage` row of kind `decide` covers it. `decide_gaps_from!` and `decide_gaps_sql!` sit beside `ask_gaps_from!` (`gaps.rs:109`) and are used wherever the ask pair is; dismissing a gap of this kind sets `decide_events.dismissed_at`.

- [ ] **Step 1: Write the failing tests**

```rust
// wire.rs
#[test] fn criteria_keep_the_callers_order() {
    // {"q":"x","questions":{"verdict":{"type":"choice","criteria":{"switch":"Move","keep":"Stay"}}}}
    // -> options [("switch","Move"), ("keep","Stay")]
}
#[test] fn questions_keep_the_callers_order() { /* keys "zeta" then "alpha" stay in that order */ }
#[test] fn an_absent_questions_field_is_the_default_noul() { /* into_request().questions is empty */ }
#[test] fn a_score_takes_an_array_and_a_noul_takes_nothing() {}
#[test] fn a_shape_that_does_not_match_its_type_names_the_key() { /* choice with array criteria -> Validation containing "verdict" */ }
#[test] fn the_response_is_the_specs_shape() {
    // a held Decision with one choice -> serde_json::to_string, then:
    //   the text contains "\"probabilities\":{\"keep\":0.81,\"switch\":0.19}" (order as asked, checked on the text)
    //   parsed: answers.verdict.type == "choice", .choice == "keep", .swapped_agree == true
    //   prior.verdict has no swapped_agree; standing.verdict == "settled"
    //   evidence[0] has n, artifact_id, title, text, bears, joint, read.verdict
    //   read, dropped, model present; event_id absent when None; state absent when None
}
#[test] fn a_not_held_response_has_null_answers_and_empty_evidence() { /* "answers":null, "evidence":[], standing.verdict == "not_held" */ }
#[test] fn noul_and_score_answers_have_their_own_shapes() { /* {"type":"noul","noul":0.9}; score: probabilities an array */ }

// core/decide/mod.rs
#[tokio::test] async fn a_decision_from_the_ui_is_recorded_with_its_evidence() {
    // learn.enabled = true, Door::Ui.by("user-1") -> event_id Some; store.decision(id): question, standing,
    // one row per passage read: rank, bears, bearing, joint, n
}
#[tokio::test] async fn a_held_decision_marks_what_it_was_weighed_from_as_cited() {
    // the joint evidence is cited as an ask's used citations are (assert what the ask test beside record_ask asserts); a not-held decision cites nothing
}
#[tokio::test] async fn the_api_and_mcp_doors_leave_no_record() { /* Door::Api, Door::Mcp -> event_id None */ }
#[tokio::test] async fn nothing_is_recorded_while_learning_is_off() {}
#[tokio::test] async fn a_not_held_decision_is_recorded_and_is_an_open_gap() {
    // event_id Some; store.open_gap_refs(..) (as an ask-gap test calls it) holds one of kind Decide naming the question
}
#[tokio::test] async fn a_held_decision_judged_nothing_here_becomes_a_gap_and_dismissing_closes_it() {}
#[tokio::test] async fn a_not_held_decision_judged_wrong_is_no_gap_and_undoing_reopens_it() { /* store.judge_decision(id, Wrong, None); then unjudge */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::decide store::gaps`
Expected: compile error.

- [ ] **Step 3: Implement.** The ordered reads are a serde `Visitor` with `visit_map` pushing entries as they come. `Serialize for Decision` uses `serialize_map` throughout. For gaps, read `src/store/gaps.rs` whole first; give `Decide` a sibling for each thing `Ask` has.

- [ ] **Step 4: Regenerate `.sqlx/`.**

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib core::decide store::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/core/decide src/store .sqlx
git commit -m "feat(decide): decisions are recorded; one the base cannot hold is a gap"
```

---

### Task 5: What a verdict does

**Files:**
- Create: `src/core/decide/verdict.rs`

**Interfaces:**
- Consumes: `store.decision`, `store.judge_decision`, `store.unjudge_decision`, `StoredDecision`, `StoredEvidence`; `standing::pick`; `Core::verify` (through `Core::confirm_cited(&[String])`, `core/ingest.rs:1212`); `store.record_pair_with_detail`; `AskVerdict`.
- Produces:
  ```rust
  /// What the person corrected a wrong verdict to, as the client sends it.
  #[derive(Debug, Clone, PartialEq)]
  pub enum Correct { Option(String), Level(usize), Yes, No }
  /// The right answer's index for this question (as `pick` counts), when it can be known:
  /// Right -> the joint pick; Wrong -> the other of two, else `correct`; otherwise None.
  pub fn right_answer(q: &Question, joint: &Answer, verdict: AskVerdict, correct: Option<&Correct>) -> Result<Option<usize>>;
  pub struct Sides { pub agree: Vec<String>, pub against: Vec<String> }   // artifact ids, strongest bearing first
  pub fn sides(q: &Question, right: usize, reads: &[(String, f64, Answer)]) -> Sides;
  pub const MAX_QUEUED_PAIRS: usize = 3;
  pub const PAIR_DETAIL: &str = "decide: read against a confirmed answer";
  pub struct Judged { pub verdict: Option<AskVerdict>, pub correct: Option<String> }   // correct: the stored JSON
  impl Core {
      pub async fn judge_decision(&self, id: &str, verdict: AskVerdict, correct: Option<Correct>) -> Result<Judged>;
      pub async fn unjudge_decision(&self, id: &str) -> Result<Judged>;
  }
  ```
  `judge_decision`, on the first question and the bearing reads of the stored decision:
  1. `right_answer`. A `correct` that names no option, a level out of range, or the verdict's own pick is `Validation`. `correct` given with `right` or `nothing_here` is ignored.
  2. Store the verdict and, when known, the right answer as `{key: answer}` (option name, level index, or `"yes"` / `"no"`).
  3. Held, and `right` or `wrong` with a right answer: `confirm_cited(&sides.agree)`; then, if anyone agrees, for the first `MAX_QUEUED_PAIRS` of `sides.against`, `record_pair_with_detail(against, &sides.agree[0], 0.0, PAIR_DETAIL)`.
  4. `nothing_here`, a decision not held, and `wrong` with no right answer: step 2 only. The gap follows from the stored verdict (Task 4's predicate).

  Effects after the verdict is written are best-effort and logged, as in `judge_ask`. `unjudge_decision` clears the verdict and the right answer and undoes nothing else.

- [ ] **Step 1: Write the failing tests**

```rust
// pure
#[test] fn right_means_the_verdicts_pick() { /* choice [a,b,c], joint b, Right -> Some(1) */ }
#[test] fn wrong_between_two_means_the_other() { /* noul yes 0.9, Wrong -> Some(1) (no); choice [keep, switch] joint keep -> Some(1); score of two levels likewise */ }
#[test] fn wrong_among_three_needs_to_be_told() { /* None without correct; Correct::Option("c") -> Some(2); Correct::Level(0) on a score -> Some(0) */ }
#[test] fn a_correction_must_name_another_answer() { /* Option("zzz"), Level(9), and the joint pick itself -> Err(Validation) */ }
#[test] fn nothing_here_names_no_right_answer() { /* None */ }
#[test] fn sides_split_the_bearing_notes_by_their_own_pick_strongest_first() {}

// core, on a recorded decision: choice [keep, switch], joint keep; notes A (0.95, keep), B (0.9, keep), C (0.8, switch)
#[tokio::test] async fn right_confirms_the_notes_that_agree() {
    // A and B have last_verified_at set to now; C's is unchanged
}
#[tokio::test] async fn right_queues_the_note_against_with_the_strongest_that_agrees() {
    // store.pair_between(C, A) is pending with detail PAIR_DETAIL and score 0.0; no pair names B
}
#[tokio::test] async fn wrong_between_two_confirms_the_other_side() {
    // Wrong -> Judged.correct holds {"verdict":"switch"}; C is stamped, A and B are not; pairs (A, C) and (B, C) are queued
}
#[tokio::test] async fn wrong_among_three_without_a_correction_only_stores_the_verdict() { /* no stamp, no pair, correct None */ }
#[tokio::test] async fn at_most_three_pairs_are_queued() { /* five notes against -> three pairs, the strongest three */ }
#[tokio::test] async fn a_unanimous_decision_queues_nothing() {}
#[tokio::test] async fn a_pair_that_exists_is_left_as_it_is() { /* a dismissed pair (C, A) stays dismissed */ }
#[tokio::test] async fn nothing_here_confirms_nothing_and_queues_nothing() {}
#[tokio::test] async fn a_verdict_on_a_decision_not_held_touches_no_note() { /* Right and Wrong both: no stamp, no pair */ }
#[tokio::test] async fn no_verdict_retires_or_rewrites_a_note() {
    // after Right, Wrong and Nothing here in turn: every note is still in_results() with its text unchanged
}
#[tokio::test] async fn undo_clears_the_verdict_and_keeps_the_stamps() {}
#[tokio::test] async fn judging_an_unknown_decision_is_not_found() {}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::decide::verdict`
Expected: compile error.

- [ ] **Step 3: Implement `verdict.rs`.**

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib core::decide`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/decide
git commit -m "feat(decide): a verdict confirms the notes that agree and sends the ones against to the pair judge"
```

---

### Task 6: REST

**Files:**
- Modify: `src/web/api.rs` (routes ~1862; handlers beside `ask` ~1185 and `ask_stream` ~1722; the unauthenticated list ~3654), `src/web/client.rs` (routes ~15; handlers beside `ask_verdict` ~105)
- Modify: `docs/api.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: `DecideBody::into_request`, `Core::decide`, `Core::decide_events`, `Serialize for Decision`, `answer_json`, `Core::judge_decision`, `Core::unjudge_decision`, `Correct`, `ask_origin`, `error_frame`, `AskParams`.
- Produces:
  - `POST /api/v1/decide?door=` → the `Decision` as JSON. Default door `Api`.
  - `POST /api/v1/decide/stream?door=` → SSE: `retrieved` `{"passages": n}`; `read` `{"rank","of","artifact_id","title","bears","bearing","read":{key: answer}}`; `done` the same JSON as `/decide`; `error` `{"error": …}` and the stream ends. Default door `Api`.
  - `POST /api/v1/decisions/{id}/verdict` body `{"verdict": "right"|"wrong"|"nothing_here"|"none", "correct": {key: answer}}` → `{"verdict": label|null, "correct": {key: answer}|null}`, labels from `web::workspace::verdict_label`. `correct` is optional; its value is an option name, a level index or `"yes"`/`"no"`, read for the first question.
  - `fn decide_sse_event(ev: DecideEvent) -> Result<SseEvent>` (api.rs), used again by nothing else; the web UI has its own frames.

- [ ] **Step 1: Write the failing tests** in `src/web/api.rs` tests (and `client.rs` tests for the follow-ups), with a core from Task 3's helpers and `app_with_token`:

```rust
#[tokio::test] async fn api_decide_answers_a_bare_question_with_yes_or_no() {
    // POST /api/v1/decide {"q": "…"} -> 200; answers.verdict.type == "noul"; standing.verdict set; read == 3; no event_id
}
#[tokio::test] async fn api_decide_takes_system_ones_questions_in_the_callers_order() {
    // the spec's request body with criteria {"switch":…,"keep":…} -> the body text contains "\"probabilities\":{\"switch\":"
}
#[tokio::test] async fn api_decide_says_not_held_with_200() { /* answers null, evidence [], standing.verdict "not_held" */ }
#[tokio::test] async fn api_decide_refuses_a_bad_request_in_one_sentence() {
    // {"q": ""}; a question keyed "bears"; nine questions; type "box" -> 400 and {"error": …} each
}
#[tokio::test] async fn api_decide_explain_adds_the_state() { /* "explain": true -> state is a string; else absent */ }
#[tokio::test] async fn api_decide_stream_sends_retrieved_reads_and_done() {
    // frames in order: event: retrieved; 3 × event: read (each with bearing); event: done whose data equals /decide's shape
}
#[tokio::test] async fn api_decide_stream_ends_on_an_error_frame() { /* a rule that errs -> last frame event: error */ }
#[tokio::test] async fn a_decision_from_the_app_is_recorded_and_judged() {
    // learning base; POST /api/v1/decide?door=app -> event_id
    // POST /decisions/{id}/verdict {"verdict":"right"} -> {"verdict":"right","correct":{"verdict":"keep"}}; "none" -> both null; "maybe" -> 400
    // {"verdict":"wrong","correct":{"verdict":"c"}} on three options -> correct echoed; a correct naming no option -> 400
    // an unknown id -> 404
}
// and add to every_api_route_rejects_an_unauthenticated_request:
// ("POST", "/api/v1/decide"), ("POST", "/api/v1/decide/stream"),
// ("POST", "/api/v1/decisions/x/verdict")
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::api::tests::api_decide web::client`
Expected: 404s and compile errors.

- [ ] **Step 3: Implement** the three handlers and routes, each as its ask sibling is written, without the role check.

- [ ] **Step 4: `docs/api.md`.** In "Read routes", a row `` `POST /decide`, `POST /decide/stream` `` in the table's style: what `q`, `questions`, `situation`, `limit`, `tags`, `category`, `explain` are; the response fields; the four standings; that `not_held` is a 200 with `answers: null`; the stream's four frames. A second row for `?door=app` recording, as ask's row at :78. In "Judging and undoing", a row for the verdict route that says what each verdict does to the base, in the table's two columns. One example request and response, copied from the spec.

- [ ] **Step 5: `CHANGELOG.md`**, under `### Added`:

```
- Decide: a typed question answered from the notes that bear on it, with each note's own reading. Judging a decision confirms the notes that agree and sends the ones against to the pair judge. `POST /api/v1/decide`, `/decide/stream`, `/decisions/{id}/verdict`; thresholds under `[decide]`.
```

- [ ] **Step 6: Run everything**

Run: `cargo test && cargo test --features contained --lib && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/web docs/api.md CHANGELOG.md
git commit -m "feat(api): /api/v1/decide and its verdict route"
```
