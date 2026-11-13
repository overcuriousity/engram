# Judgement Layer (spec 2, stage 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One layer every background judgement will go through: a record of each decision with its band and outcome, the act / ask / leave rule, waiting and lapsing, outcomes written by undo, and the Unsure block on Insights. No stage produces decisions yet.

**Architecture:** `src/core/judging/` holds the pure rules (band, era, thresholds) and one function, `ask`, that puts a question to the `Decider`, bands the answer and logs it. A stage registers an `Applier` for its question; the layer calls it when a decision may act, when a person answers an Unsure row, and when a row is reopened. A new periodic sweep, `Stage::Decisions`, applies waiting decisions, seats waiting ask rows and lapses old ones. Outcomes are written where undo already happens, in `store/actions.rs`.

**Tech Stack:** Rust, sqlx (SQLite, compile-time queries), axum, askama, htmx.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md`, section "The decision layer" and stage 1; read its "Corrections from the code" section first. Part 1 of 5. **Depends on spec 1 being merged** (`Decider`, `decide_swapped`, `budget`, `ScriptedDecider`, `RuleDecider`, `[decide]`); branch `feat/judge` is cut from master after that merge.

## Global Constraints

- Bands, on the top probability: **act** at `>= act_at`, and for a `choice` only when the swapped read agreed (a disagreeing swap at `act_at` is ask); **ask** at `>= ask_at`; **leave** below. Defaults `act_at = 0.90`, `ask_at = 0.60`, per-question overrides under `[decide.bands.<id>]`.
- A band is what the model said. Whether it can be carried out now is separate: an act decision the gates refuse is `waiting`, is applied later without a second model call, and never becomes an ask row.
- At most `decide.ask_open_max` (20) ask rows are open; the rest wait, highest top first. A row unanswered for `decide.ask_lapse_days` (30) lapses.
- A person's pick is not subject to the daily cap and is attributed to the operator.
- `Core::may_act`, `[learn] mode` and each job's taken-back check gate actions exactly as today. Every action keeps its journal row and undo.
- A change of decision model or of a question's version is a new era: `era = "{model}/v{version}"`.
- Nothing irreversible rests on one call.
- The manifest, profile everyday, for the Unsure block: few words, one primary per view, keyboard reachable, 390 px. `tests/copy.rs`: no text run over 12 words.
- After any schema or query change regenerate `.sqlx/` as `AGENTS.md` says. Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs with `-D warnings`.

## Decisions this plan makes

- Names. The table is `decisions`, as the spec says. In Rust the store module is `store::decision_log` and its row is `Logged`, because spec 1 already owns `store::decisions` and `/api/v1/decisions/{id}` for what a person asked. The routes here are `/ui/ops/unsure/…` and `/api/v1/unsure…`.
- A "tick" for `decide.max_per_tick` (60) is a rolling window of `decide.tick_mins` (15): an arming sweep may arm at most `max_per_tick` less the decisions made in the last `tick_mins`.
- Threshold moves get their own table, `decision_thresholds`; the tuner has no record of its own to add them to. The row in force is the newest for a question and era, else config.
- A noul's pick is `yes` at `>= 0.5`, else `no`; its top is the larger of yes and no. One `act_at` covers both picks.
- `kept` is written by a sweep when a decision's window closes: 30 days after it acted with no undo.
- "Ask again" is offered on the Unsure block only, under a disclosure listing the last ten answers that left no journal row. There is no pair view to offer it on (see the spec's corrections).

## Review Focus

1. A subject changed between the decision and its turn to act (an edit, a retirement): it is not applied; it is closed and asked again. Task 3.
2. Two workers reach the same waiting decision: it is applied once. Task 3.
3. An Unsure row answered twice, or answered after it lapsed: the second press changes nothing and says so. Task 4.
4. An undo of one of several journal rows of a decision (a merge writes one per source): the decision is `undone` once. Task 2.
5. An applier that fails halfway: the decision stays as it was and is tried again. Task 3.

## File Structure

| File | Responsibility |
|---|---|
| `src/config.rs`, `config.example.toml` (modify) | `[decide.bands]`, caps, tick |
| `src/store/schema.sql`, `src/store/decision_log.rs` (create), `src/store/mod.rs` | `decisions`, `decision_actions`, `decision_thresholds` |
| `src/store/actions.rs` (modify) | undo writes the outcome |
| `src/core/judging/mod.rs` (create) | `Asked`, `Subjects`, `ask`, `Applier`, `Appliers`, the three flows |
| `src/core/judging/band.rs` (create) | `Band`, `Thresholds`, `band`, `era`, `pick_and_top` |
| `src/core/mod.rs` (modify) | `Core.appliers`; `judging_budget` |
| `src/store/jobs.rs`, `src/jobs/mod.rs`, `src/jobs/decisions.rs` (create), `src/core/background.rs`, `src/web/ui.rs` | `Stage::Decisions` and its sweep |
| `src/web/unsure.rs` (create), `templates/_unsure.html` (create), `templates/insights.html`, `src/web/insights.rs`, `src/web/judge.rs`, `src/web/api.rs`, `src/cli/status.rs` | the Unsure block, its routes, the count |
| `assets/css/76-insights.css`, `78-phone.css` (modify) | layout |

---

### Task 1: Config and the banding rule

**Files:**
- Modify: `src/config.rs` (`DecideConfig`), `config.example.toml`
- Create: `src/core/judging/mod.rs` (module root), `src/core/judging/band.rs`

**Interfaces:**
- Consumes: `Question`, `Kind`, `Answer` (`infer::decide`).
- Produces:
  ```rust
  // config.rs, on DecideConfig (all #[serde(default)])
  pub bands: Bands,            // { act_at: 0.90, ask_at: 0.60, per: HashMap<String, BandOverride { act_at: Option<f64>, ask_at: Option<f64> }> }
  pub ask_open_max: usize,     // 20
  pub ask_lapse_days: u32,     // 30
  pub max_per_tick: usize,     // 60
  pub tick_mins: u32,          // 15
  impl Bands { pub fn of(&self, question_id: &str) -> Thresholds; }

  // band.rs
  #[derive(Debug, Clone, Copy, PartialEq)] pub struct Thresholds { pub act_at: f64, pub ask_at: f64 }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Band { Act, Ask, Leave }   // as_str: "act" "ask" "leave"
  pub fn era(model: &str, version: u32) -> String;                                  // "{model}/v{version}"
  /// The answer's pick as a string (option name, level index, "yes"/"no") and its top probability.
  pub fn pick_and_top(q: &Question, a: &Answer) -> (String, f64);
  pub fn band(top: f64, swapped_agree: Option<bool>, t: Thresholds) -> Band;
  ```
  `[decide.bands.<id>]` is read as `per`; the flattening is `#[serde(flatten)]` over the map beside `act_at` and `ask_at`.

- [ ] **Step 1: Write the failing tests**

```rust
// config.rs
#[test] fn bands_default_and_take_a_per_question_override() {
    // MINIMAL: decide.bands.of("x") == Thresholds { 0.90, 0.60 }
    // + "[decide.bands.reap_worth]\nact_at = 0.97": of("reap_worth") == { 0.97, 0.60 }; of("x") unchanged
}
#[test] fn a_band_out_of_order_is_refused() { /* ask_at 0.95 with act_at 0.90 -> Invalid naming decide.bands; act_at 1.2 -> Invalid */ }
#[test] fn the_caps_default_to_the_spec() { /* 20, 30, 60, 15 */ }

// band.rs
#[test] fn ninety_acts_sixty_asks_and_below_is_left() {
    let t = Thresholds { act_at: 0.90, ask_at: 0.60 };
    assert_eq!(band(0.90, None, t), Band::Act); assert_eq!(band(0.899, None, t), Band::Ask);
    assert_eq!(band(0.60, None, t), Band::Ask); assert_eq!(band(0.599, None, t), Band::Leave);
}
#[test] fn a_choice_acts_only_when_its_swapped_read_agreed() {
    // 0.99 with Some(false) -> Ask; with Some(true) -> Act; a swap disagreeing at 0.50 -> Leave
}
#[test] fn a_noul_picks_yes_or_no_and_its_top_is_the_larger() { /* yes 0.97 -> ("yes", 0.97); yes 0.02 -> ("no", 0.98); 0.5 -> ("yes", 0.5) */ }
#[test] fn a_choice_and_a_score_pick_their_largest() { /* choice -> option name; score -> "1" */ }
#[test] fn an_era_names_the_model_and_the_version() { assert_eq!(era("clef-flash", 2), "clef-flash/v2"); }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::judging config::tests::bands config::tests::a_band config::tests::the_caps`
Expected: compile error.

- [ ] **Step 3: Implement.** Validation in `Config::validate`: every `act_at` and `ask_at` in `0.0..=1.0`, `ask_at < act_at` for the default and each override, `ask_open_max >= 1`, `tick_mins >= 1`. The commented `[decide.bands]` block from the spec goes into `config.example.toml` with the caps.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib core::judging && cargo test --lib config::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs config.example.toml src/core/judging src/core/mod.rs
git commit -m "feat(judging): act, ask, leave"
```

---

### Task 2: The record, and outcomes from undo

**Files:**
- Modify: `src/store/schema.sql`, `src/store/mod.rs`, `src/store/actions.rs` (`undo_action_on` ~293, `undo_actions_under` ~316)
- Create: `src/store/decision_log.rs`

**Interfaces:**
- Produces:
  ```sql
  CREATE TABLE IF NOT EXISTS decisions (
    id TEXT PRIMARY KEY,
    question_id TEXT NOT NULL,
    question_version INTEGER NOT NULL,
    model TEXT NOT NULL,
    era TEXT NOT NULL,
    subject_kind TEXT NOT NULL,      -- pair | link | artifact | pursuit | write | search
    subject_a TEXT NOT NULL,
    subject_b TEXT,
    rev_a INTEGER,                   -- what the subjects were when asked
    rev_b INTEGER,
    answer TEXT NOT NULL,            -- probabilities as returned
    top REAL NOT NULL,
    pick TEXT NOT NULL,
    swapped_agree INTEGER,
    band TEXT NOT NULL,              -- act | ask | leave
    reason TEXT,                     -- too_long, restricted, …
    state TEXT NOT NULL,             -- waiting | open | done | lapsed
    outcome TEXT,                    -- kept | undone | confirmed | corrected | refused
    outcome_at INTEGER,
    answered_pick TEXT,              -- what a person chose, on an ask row
    decided_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_decisions_subject ON decisions(question_id, subject_a, subject_b, era);
  CREATE INDEX IF NOT EXISTS idx_decisions_band ON decisions(band, state, top DESC);
  CREATE INDEX IF NOT EXISTS idx_decisions_at ON decisions(decided_at);
  CREATE TABLE IF NOT EXISTS decision_actions (
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    action_id TEXT NOT NULL,
    PRIMARY KEY (decision_id, action_id)
  );
  CREATE TABLE IF NOT EXISTS decision_thresholds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    question_id TEXT NOT NULL,
    era TEXT NOT NULL,
    act_at REAL NOT NULL,
    ask_at REAL NOT NULL,
    reason TEXT NOT NULL,
    moved_at INTEGER NOT NULL
  );
  ```
  `state`: an ask row with a seat is `open`; act and ask decisions without one are `waiting`; everything settled is `done`.
  ```rust
  pub enum State { Waiting, Open, Done, Lapsed }
  pub enum Outcome { Kept, Undone, Confirmed, Corrected, Refused }
  pub struct NewLogged { pub question_id: String, pub question_version: u32, pub model: String, pub subject_kind: &'static str,
      pub subject_a: String, pub subject_b: Option<String>, pub rev_a: Option<i64>, pub rev_b: Option<i64>,
      pub answer: String, pub top: f64, pub pick: String, pub swapped_agree: Option<bool>,
      pub band: Band, pub reason: Option<String>, pub state: State }
  pub struct Logged { pub id: String, /* every column, typed */ }
  impl Store {
      pub async fn log_decision(&self, d: &NewLogged) -> Result<String>;
      pub async fn logged(&self, id: &str) -> Result<Option<Logged>>;
      /// The newest decision on these subjects in this era, if any.
      pub async fn last_logged(&self, question_id: &str, subject_a: &str, subject_b: Option<&str>, era: &str) -> Result<Option<Logged>>;
      pub async fn set_decision_state(&self, id: &str, from: State, to: State) -> Result<bool>;   // false: it was no longer `from`
      pub async fn set_decision_outcome(&self, id: &str, outcome: Outcome, answered_pick: Option<&str>) -> Result<()>;
      pub async fn link_decision_actions(&self, id: &str, action_ids: &[String]) -> Result<()>;
      pub async fn waiting_acts(&self, limit: i64) -> Result<Vec<Logged>>;       // band act, state waiting, top DESC
      pub async fn waiting_asks(&self, limit: i64) -> Result<Vec<Logged>>;       // band ask, state waiting, top DESC
      pub async fn open_asks(&self, limit: i64) -> Result<Vec<Logged>>;          // state open, top DESC
      pub async fn open_ask_count(&self) -> Result<i64>;
      pub async fn answered_without_action(&self, limit: i64) -> Result<Vec<Logged>>;  // confirmed|corrected, no decision_actions row, newest first
      pub async fn lapse_asks(&self, older_than: i64) -> Result<u64>;            // open -> lapsed
      pub async fn close_outcome_windows(&self, older_than: i64) -> Result<u64>; // done act decisions with actions and no outcome -> kept
      pub async fn decided_since(&self, at: i64) -> Result<i64>;
      pub async fn waiting_counts(&self) -> Result<Vec<(String, i64)>>;          // per question, band act, state waiting
      pub async fn thresholds_in_force(&self, question_id: &str, era: &str) -> Result<Option<Thresholds>>;
      pub async fn move_thresholds(&self, question_id: &str, era: &str, t: Thresholds, reason: &str) -> Result<()>;
      pub async fn threshold_moves(&self, limit: i64) -> Result<Vec<ThresholdMove>>;
  }
  ```
  `undo_action_on` and `undo_actions_under` each end by setting `outcome = 'undone'`, `outcome_at = now` on every decision joined through `decision_actions` to a row they just stamped, where the outcome is null or `kept`.

- [ ] **Step 1: Write the failing tests** in `decision_log.rs` on `Store::memory()`; `fn sample(band, state, top) -> NewLogged`:

```rust
#[tokio::test] async fn a_decision_is_read_back_as_logged() {}
#[tokio::test] async fn the_last_decision_on_a_subject_is_found_per_era() { /* two eras: each finds its own; a third subject finds none */ }
#[tokio::test] async fn a_state_moves_only_from_the_state_it_was_in() { /* waiting->done true; again false */ }
#[tokio::test] async fn waiting_acts_come_highest_top_first() {}
#[tokio::test] async fn open_rows_older_than_the_lapse_are_lapsed_and_younger_ones_stay() {}
#[tokio::test] async fn undoing_any_action_of_a_decision_marks_it_undone_once() {
    // a decision linked to two corpus_actions rows (one merge per source); undo_action_on for one subject:
    // outcome == Undone; undoing the other changes nothing and outcome_at is unchanged
}
#[tokio::test] async fn undo_actions_under_marks_the_merges_decision() {}
#[tokio::test] async fn an_undo_of_an_action_no_decision_made_touches_nothing() {}
#[tokio::test] async fn a_window_that_closed_without_an_undo_is_kept() { /* only done act decisions that have an action row */ }
#[tokio::test] async fn an_undo_after_kept_still_counts() { /* kept -> undone */ }
#[tokio::test] async fn decisions_are_counted_since_a_time() {}
#[tokio::test] async fn the_newest_threshold_move_is_in_force_for_its_era_only() {}
#[tokio::test] async fn answers_that_left_no_journal_row_are_listed_for_asking_again() {}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib store::decision_log`
Expected: compile error.

- [ ] **Step 3: Implement**, in the style of `src/store/asks.rs`.

- [ ] **Step 4: Regenerate `.sqlx/`.**

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib store:: && cargo test --test csp_static`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/store .sqlx
git commit -m "feat(store): the record of every decision, and what undo says about it"
```

---

### Task 3: Asking, applying, waiting

**Files:**
- Modify: `src/core/judging/mod.rs`, `src/core/mod.rs`, `src/store/jobs.rs`, `src/jobs/mod.rs`, `src/core/background.rs`, `src/web/ui.rs` (stage label, `SWEEP_WORDS`)
- Create: `src/jobs/decisions.rs`

**Interfaces:**
- Consumes: Tasks 1 and 2; `core.decider`, `decide_swapped`, `infer::decide::budget`, `core.counter`, `core.gate.background_light()`, `Core::may_act`, `DecidedBy`.
- Produces:
  ```rust
  pub struct Asked { pub id: &'static str, pub version: u32, pub question: Question, pub swapped: bool }
  pub struct Subjects { pub kind: &'static str, pub a: String, pub b: Option<String>, pub rev_a: Option<i64>, pub rev_b: Option<i64> }

  #[async_trait]
  pub trait Applier: Send + Sync {
      fn question_id(&self) -> &'static str;
      /// Two or three words for the Unsure row: "Same thing?", "Worth keeping?".
      fn label(&self) -> &'static str;
      /// The job whose daily cap an act of this question spends, if any.
      fn job(&self) -> Option<Job>;
      /// The answers a person may give, as (pick, button label), for this decision.
      async fn buttons(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>>;
      /// The subjects as (id to open, title) for the row.
      async fn subjects(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>>;
      /// Whether the subjects are still what was asked about.
      async fn unchanged(&self, core: &Core, d: &Logged) -> Result<bool>;
      /// Carry out `pick`. Returns the journal rows written; empty when the answer journals nothing.
      async fn apply(&self, core: &Core, d: &Logged, pick: &str, by: DecidedBy) -> Result<Vec<String>>;
      /// Put the subject back to be asked again.
      async fn reopen(&self, core: &Core, d: &Logged) -> Result<()>;
  }
  #[derive(Default)] pub struct Appliers(/* by question id */);
  impl Appliers { pub fn register(&mut self, a: Arc<dyn Applier>); pub fn get(&self, question_id: &str) -> Option<Arc<dyn Applier>>; }
  // Core: pub appliers: Arc<Appliers>   (empty until a stage registers; tests build their own)

  impl Core {
      /// How many decision calls an arming sweep may still arm in this tick.
      pub async fn judging_budget(&self) -> Result<usize>;
      /// The thresholds in force for a question: the tuner's for this era, else config.
      pub async fn thresholds(&self, a: &Asked) -> Result<Thresholds>;
  }
  /// Put the question, band it, log it. `state` is the whole state; `None` means the subjects do not fit:
  /// logged as leave with reason "too_long" and no call.
  pub async fn ask(core: &Core, a: &Asked, s: Subjects, state: Option<&str>) -> Result<Logged>;
  /// An act decision a stage may not carry out alone becomes an ask, with the reason. Called before `settle`.
  pub async fn demote_to_ask(core: &Core, d: Logged, reason: &str) -> Result<Logged>;
  /// After `ask`: act now if the gates allow (else leave it waiting), seat an ask row if one is free. Returns the decision as it stands.
  pub async fn settle(core: &Core, d: Logged) -> Result<Logged>;
  /// A person's answer to an open row. `pick` of None is Leave.
  pub async fn answer(core: &Core, id: &str, pick: Option<&str>) -> Result<Logged>;
  pub async fn ask_again(core: &Core, id: &str) -> Result<()>;
  // jobs/decisions.rs
  pub struct Report { pub applied: u64, pub reasked: u64, pub seated: u64, pub lapsed: u64, pub kept: u64 }
  pub async fn run(core: &Core) -> Result<Report>;
  ```
  Rules:
  - `ask` takes `core.gate.background_light()` around the call, uses `decide_swapped` when `a.swapped`, bands with `Core::thresholds`, and logs: act and ask as `waiting`, leave as `done`.
  - `settle`, act: no applier registered → stays `waiting`. `applier.job()` is `Some(j)` and `!may_act(j)` → stays `waiting`. Otherwise move `waiting → done` first (losing that move means another worker has it: return), then `apply(.., DecidedBy::Model)` and `link_decision_actions`. If `apply` fails, move it back to `waiting` and return the error.
  - `settle`, ask: `open_ask_count() < ask_open_max` → `waiting → open`.
  - `answer`: the row must be `open`, else `Error::Validation("this was already answered")`. Leave → `done`, no outcome. A pick → `apply(.., DecidedBy::Operator)` with no cap check, link the rows, `done`, outcome `confirmed` when `pick == d.pick` else `corrected`, `answered_pick` set.
  - `ask_again`: `reopen`, for a decision `answered_without_action` would list.
  - `decisions::run`, in order: `lapse_asks`; `close_outcome_windows` (30 days); for `waiting_acts(200)`: `unchanged` false → `done` with reason `changed`, `reopen`, count `reasked`; else `settle`; then seat `waiting_asks` while seats are free.
  - `Stage::Decisions` (`"decisions"`, class 1, not a generator, not a capture reader), periodic every `decide.tick_mins`, always armed; UI label `Applying decisions`.

- [ ] **Step 1: Write the failing tests** in `src/core/judging/mod.rs`. `struct TestApplier` records calls, journals one `Kind::Discard` row on `apply` unless told to write none, and can be told to fail, to report changed, and which `Job` it spends. `fn core_with(applier, rule) -> (Core, Arc<TestApplier>, Arc<RuleDecider>)`; `fn q_choice() -> Asked` (swapped), `fn q_noul() -> Asked`.

```rust
#[tokio::test] async fn a_sure_swapped_choice_acts_and_links_its_journal_rows() {
    // 0.95 both ways: band Act, state Done, applier.apply called once with DecidedBy::Model, decision_actions has the row
}
#[tokio::test] async fn a_ninety_nine_whose_reversed_read_disagrees_asks_and_does_nothing() {
    // band Ask, state Open, apply never called  (the spec's "Done when")
}
#[tokio::test] async fn a_torn_answer_opens_a_row_and_a_weak_one_is_left() { /* 0.7 -> Open; 0.4 -> Leave, Done */ }
#[tokio::test] async fn subjects_that_do_not_fit_are_left_without_a_call() { /* state None: band Leave, reason "too_long", decider.calls() == 0 */ }
#[tokio::test] async fn a_demoted_act_asks_with_its_reason_and_never_acts() { /* 0.99 agreeing, demote_to_ask(.., "restricted"), settle -> Open, reason "restricted", apply not called */ }
#[tokio::test] async fn demoting_an_ask_or_a_leave_changes_nothing() {}
#[tokio::test] async fn the_cap_makes_an_act_wait_and_a_later_sweep_applies_it_without_asking_again() {
    // evolve.max_actions_per_day = 0: Waiting, apply not called, open_ask_count == 0
    // cap raised, decisions::run: applied == 1, decider.calls() unchanged, state Done   (the spec's "Done when")
}
#[tokio::test] async fn a_waiting_act_never_becomes_an_ask_row() { /* many sweeps under a zero cap: open_asks is empty */ }
#[tokio::test] async fn a_changed_subject_is_closed_and_asked_again_instead_of_applied() { /* reasked == 1, reopen called, apply not */ }
#[tokio::test] async fn an_applier_that_fails_leaves_the_decision_waiting() { /* Err; state Waiting; the next sweep applies it */ }
#[tokio::test] async fn two_sweeps_at_once_apply_a_decision_once() { /* tokio::join! two runs: apply called once */ }
#[tokio::test] async fn the_twenty_first_ask_waits_for_a_seat_and_takes_the_highest_top_first() { /* ask_open_max = 2 */ }
#[tokio::test] async fn a_row_nobody_answers_lapses() {}
#[tokio::test] async fn a_person_who_picks_the_models_pick_confirms_it_past_the_cap() {
    // cap 0: answer(id, Some(model pick)) -> apply with DecidedBy::Operator; outcome Confirmed
}
#[tokio::test] async fn a_person_who_picks_otherwise_corrects_it() { /* outcome Corrected, answered_pick set */ }
#[tokio::test] async fn leave_closes_the_row_and_does_nothing() {}
#[tokio::test] async fn a_second_answer_is_refused_in_one_sentence() { /* also after a lapse */ }
#[tokio::test] async fn ask_again_reopens_only_an_answer_that_left_no_journal_row() {}
#[tokio::test] async fn the_tuners_thresholds_outrank_config_for_their_era_only() {}
#[tokio::test] async fn the_budget_is_what_is_left_of_the_tick() { /* max_per_tick 3, two decisions logged now -> 1; one logged 16 min ago does not count */ }
#[tokio::test] async fn a_decision_call_takes_the_light_turn_and_no_cooldown() {
    // gate with a 600 s cooldown just finished: ask returns at once; an interactive lease held: ask waits
}
```
and in `src/store/jobs.rs`: `Stage::ALL` holds 22 and `Decisions` round-trips through `as_str` / `parse`; it is not in `GENERATORS`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::judging jobs::decisions store::jobs`
Expected: compile error.

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test && cargo test --features contained --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core src/jobs src/store/jobs.rs src/web/ui.rs
git commit -m "feat(judging): ask, band, act when allowed, wait when not"
```

---

### Task 4: The Unsure block

**Files:**
- Create: `src/web/unsure.rs`, `src/web/templates/_unsure.html`
- Modify: `src/web/templates/insights.html` (above `What the base did`, ~62), `src/web/insights.rs` (`InsightsTemplate` ~103), `src/web/mod.rs`, `src/web/judge.rs`, `src/web/api.rs` (`StatusResponse` ~129, `status` ~1621), `src/cli/status.rs`, `assets/css/76-insights.css`, `assets/css/78-phone.css`, `docs/api.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: `open_asks`, `answered_without_action`, `waiting_counts`, `Appliers`, `judging::{answer, ask_again}`.
- Produces:
  ```rust
  pub struct UnsureRow { pub id: String, pub label: &'static str, pub subjects: Vec<(String, String)>,   // (href, title)
      pub weights: Vec<(String, u8)>,        // (answer, whole number out of 100), largest first
      pub buttons: Vec<(String, String)> }   // (pick, label); Leave is added by the template
  pub struct Answered { pub id: String, pub label: &'static str, pub subjects: Vec<(String, String)>, pub said: String }
  pub struct Unsure { pub rows: Vec<UnsureRow>, pub waiting_rows: i64, pub answered: Vec<Answered>, pub waiting_acts: Vec<(String, i64)> }
  pub(crate) async fn unsure(tenant: &Tenant) -> Result<Unsure>;
  ```
  | Method | Path | Does |
  |---|---|---|
  | POST | `/ui/ops/unsure/{id}/answer` | form `pick` (an answer, or `leave`); redirects to `/ui/insights` as the other `/ui/ops` posts do |
  | POST | `/ui/ops/unsure/{id}/again` | `ask_again` |
  | GET | `/api/v1/unsure` | `{"items": [{id, label, subjects: [{id, title}], weights: {answer: n}, answers: [pick]}], "waiting": n, "next": null}` |
  | POST | `/api/v1/unsure/{id}/answer` | body `{"pick": "…" \| null}`; 204 |
  | POST | `/api/v1/unsure/{id}/again` | 204 |

  `_unsure.html`, shown only when there is something in it: `<h2 class="oc-text-section">Unsure · {{ n }}</h2>`; one `<li>` per row in `<ul class="oc-list unsure" role="list">`: the label in `.muted`, the subjects as links joined by `↔`, a `.mono` line of weights `duplicate 48 · distinct 41`, then a `.oc-row` of one `<form method="post">` per button plus `Leave`. Under the list: `{{ waiting_rows }} more waiting` when any; a `<details>` `answered ({{ n }})` with `Ask again` per entry; and per question with waiting acts, `{{ n }} waiting on the daily pace`. No percent sign. `StatusResponse` gains `unsure: i64`; the CLI status prints `unsure     {n}` when non-zero.

- [ ] **Step 1: Write the failing tests** in `src/web/unsure.rs`, with `app_with_cookie` and Task 3's `TestApplier` registered:

```rust
#[tokio::test] async fn insights_shows_nothing_unsure_when_nothing_is() { /* no "Unsure" in the page */ }
#[tokio::test] async fn a_torn_decision_is_a_row_with_its_weights_and_a_button_per_answer() {
    // "Unsure · 1", the applier's label, both subject titles as links, "duplicate 48 · distinct 41", the buttons and "Leave"; no '%'
}
#[tokio::test] async fn pressing_an_answer_does_it_and_the_row_is_gone() { /* POST pick -> redirect; apply called by the operator; the page has no row */ }
#[tokio::test] async fn pressing_leave_closes_the_row_and_does_nothing() {}
#[tokio::test] async fn an_answer_that_journals_nothing_can_be_asked_again() { /* under "answered (1)"; POST again -> reopen called */ }
#[tokio::test] async fn a_row_already_answered_says_so_in_one_sentence() {}
#[tokio::test] async fn the_block_counts_what_waits_for_a_seat_and_for_the_daily_pace() {}
#[tokio::test] async fn the_json_door_lists_and_answers_the_same_rows() { /* GET /api/v1/unsure; POST answer -> 204; unauthenticated -> 401 on all three */ }
#[tokio::test] async fn status_counts_the_open_rows() { /* GET /api/v1/status .unsure == 1 */ }
```
Add the three API routes to `every_api_route_rejects_an_unauthenticated_request`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::unsure`
Expected: compile error.

- [ ] **Step 3: Implement.** Phone, below 48rem: each row's buttons wrap and are at least 44 px high.

- [ ] **Step 4: Docs.** `docs/api.md`: the three routes under "Judging and undoing" and `unsure` in the `/status` row. `CHANGELOG.md` under `### Added`: `Insights shows what the base is unsure about, with a button per answer. Nothing produces these rows yet.`

- [ ] **Step 5: Run everything**

Run: `cargo test && cargo test --features contained --lib && cargo test --test copy --test csp_static --test rendered && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings`
Then with `ENGRAM_CHROME` set: `cargo test --lib -- --ignored no_page_runs_off` (seed one row with five long-labelled buttons in `every_page()`'s Insights fixture first).
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A src assets docs/api.md CHANGELOG.md
git commit -m "feat(ui): what the base is unsure about, on Insights"
```
