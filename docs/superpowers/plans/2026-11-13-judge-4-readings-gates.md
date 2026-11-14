# Capture Readings and Writer Gates (spec 2, stages 4 and 5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The synthesizer stops judging: category, pinned, the reminder question and links are read by the decision model. Every background writer call is preceded by a decision that it is wanted and followed by one that the result is faithful.

**Architecture:** A new unit, `Stage::Read`, asks four questions of one artifact in one request and stores the answers in `artifact_readings`; readings are not decisions and have no bands. The capture window asks two questions of the raw text before synthesis to choose the schema. After each writer call a shared function, `judging::check`, asks `complete` and `adds_nothing` and refuses a result below a low line.

**Tech Stack:** Rust, sqlx, the layer (part 1), catalogue and states (part 2).

**Spec:** `docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md`, sections "Readings", "Window pre-read", "Writer gates" and stages 4 and 5, with "Corrections from the code". Part 4 of 5 on `feat/judge`, after part 3.

## Global Constraints

- Readings are stored, used at a fixed line, never banded and never put to a person: `category` takes its top pick whatever its probability; `pinned` is taken at 0.90; `time_bound` and `self_contained` at 0.5.
- The judged schema is asked for when `asks_reminder` or `has_date` is at 0.3 or above, when the window does not fit the decision context, or when the person forced an intent. A missed reminder is worse than a wasted schema.
- The decision model never writes: a date, an RRULE and the moment's text stay with the writer.
- After a writer call the result is refused when `complete` or `adds_nothing` is below `decide.refuse_below` (0.25). It is a refusal line, not a pass line. The literal checks stay and run first.
- A case that does not fit is not judged on cut text: the literal checks alone decide, as today.
- A reading never overwrites a category a person set.
- Regenerate `.sqlx/` after schema or query changes. Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs.

## Decisions this plan makes

- `artifact_readings` holds the newest answer per artifact and question: primary key `(artifact_id, question_id)`.
- Who set a category is recorded: `artifacts.category_by` (`person` | `reading` | `writer`, null on older rows). A reading writes the category only where it is null and `category_by` is not `person`. The PATCH route writes `person`.
- **`pinned` is stored and has no effect.** Today the synthesizer's flag is parsed and dropped; the only pin that ranks is a tag a person sets. Giving the reading that tag would be a new ranking behaviour nobody chose. The reading is kept so the owner can decide with data.
- A capture artifact that fails `adds_nothing` against its window is dropped, as the spec says, and its verbatim passages remain. (The spec's "as one that fails the literal check is today" is wrong about today: that one is flagged and kept. The literal check keeps flagging.)
- `Stage::Read` is armed where `Relate` is armed after an embed (`jobs/embed.rs:248`), and also for passages, which `Relate` skips.
- Links the synthesizer proposed are replaced by `link_kind` between the anchor artifact and each neighbour it was shown, asked by the read unit's sibling `judging::link_shown`, banded like any link. Only an act creates the link; an ask row's applier creates it on a pick.
- The pre-read's two answers are kept in the corpus metadata as `pre_read: {asks_reminder, has_date}`.
- Checks are logged in `decisions` with `subject_kind = "write"`: band `act` when passed, `leave` when refused, and outcome `refused` on a refusal.

## Review Focus

1. A capture of two words: the reading still runs; a category is chosen; nothing is flagged. Task 1.
2. A person sets a category, then the scan or an edit re-reads the note: the person's category stands. Task 1.
3. A window with a reminder the pre-read scores at 0.31: the judged schema is asked for; at 0.29 with no forced intent, the plain one. Task 2.
4. A faithful rewrite that scores 0.55 on `complete`: it is stored. One that scores 0.03: it is not, and the decision row says `refused`. Task 4.
5. A pursuit whose sources do not answer its questions: no writer call is made and the pursuit closes unsatisfied; one in between waits and is tried again when it gains a source. Task 5.

## File Structure

| File | Responsibility |
|---|---|
| `src/store/schema.sql`, `src/store/readings.rs` (create), `src/store/artifacts.rs`, `src/store/mod.rs` | `artifact_readings`, `category_by` |
| `src/jobs/read.rs` (create), `src/store/jobs.rs`, `src/jobs/mod.rs`, `src/jobs/embed.rs` | `Stage::Read` |
| `src/jobs/window.rs`, `src/jobs/judgement.rs` | the pre-read; links by decision |
| `src/infer/prompt.rs`, `src/infer/mod.rs`, `src/infer/openai.rs`, `src/infer/fake.rs` | the schema cut in ten languages |
| `src/core/judging/check.rs` (create) | `check`, `Checked` |
| `src/jobs/merge_unit.rs`, `rescue_unit.rs`, `condense.rs`, `pursuit.rs`, `promote.rs`, `window.rs` | the gates |
| `src/config.rs`, `config.example.toml` | `decide.refuse_below` |
| `src/web/api.rs` (`patch_artifact`) | `category_by = person` |

---

### Task 1: Readings

**Files:**
- Create: `src/store/readings.rs`, `src/jobs/read.rs`
- Modify: `src/store/schema.sql`, `src/store/mod.rs` (`category_by`: one `ADDITIVE` entry), `src/store/artifacts.rs` (`update_artifact_category` ~898), `src/store/jobs.rs` (`Stage::Read`, `"read"`, class 1, not a generator), `src/jobs/mod.rs`, `src/jobs/embed.rs` (~248), `src/web/api.rs` (~1542), `src/web/ui.rs` (label `Reading notes`)

**Interfaces:**
- Consumes: `catalogue::{category, pinned, time_bound, self_contained}`, `state::{one, room}`, `core.decider`, `gate.background_light()`.
- Produces:
  ```sql
  CREATE TABLE IF NOT EXISTS artifact_readings (
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    question_id TEXT NOT NULL,
    question_version INTEGER NOT NULL,
    answer TEXT NOT NULL,
    model TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (artifact_id, question_id)
  );
  ```
  ```rust
  pub struct Reading { pub question_id: String, pub question_version: u32, pub answer: Answer, pub model: String, pub at: i64 }
  impl Store {
      pub async fn put_readings(&self, artifact_id: &str, rows: &[(&Asked, &Answer)], model: &str) -> Result<()>;
      pub async fn readings_of(&self, artifact_id: &str) -> Result<Vec<Reading>>;
      /// The yes of a noul reading, when the artifact has one at this version.
      pub async fn reading_yes(&self, artifact_id: &str, a: &Asked) -> Result<Option<f64>>;
      pub async fn update_artifact_category_by(&self, id: &str, category: Option<&str>, by: &str) -> Result<()>;
  }
  pub const PINNED_AT: f64 = 0.90;  pub const TIME_BOUND_AT: f64 = 0.5;  pub const SELF_CONTAINED_AT: f64 = 0.5;
  /// The four questions the catalogue still holds (time_bound may have been dropped by the replay).
  pub fn reading_questions() -> Vec<Asked>;
  pub async fn run(core: &Core, artifact_id: &str) -> Result<()>;   // Stage::Read
  ```
  `run`: the artifact must be in results, else done. One request with all reading questions over `state::one`; a note that does not fit is cut with `counter.cut` to the room (a reading describes, it does not act, so a cut text is acceptable here and only here). Store the answers. If `category` is null and `category_by` is not `person`: write the top pick with `by = "reading"` and update the vector payload as `patch_artifact` does.

- [ ] **Step 1: Write the failing tests**

```rust
// store/readings.rs
#[tokio::test] async fn readings_are_kept_newest_per_question() {}
#[tokio::test] async fn a_readings_yes_is_found_only_at_the_current_version() {}
#[tokio::test] async fn deleting_an_artifact_takes_its_readings() {}
// jobs/read.rs
#[tokio::test] async fn a_new_note_is_read_once_after_it_is_embedded() { /* capture, drain: one request with the reading questions; rows stored */ }
#[tokio::test] async fn a_passage_is_read_too_and_gets_a_category() { /* the embed-only floor: passages have categories after a drain */ }
#[tokio::test] async fn the_top_category_is_taken_whatever_its_probability() { /* a pick at 0.21 is written, category_by "reading" */ }
#[tokio::test] async fn a_reading_never_overwrites_a_category_a_person_set() {
    // PATCH category "procedure" -> category_by "person"; run again with a decider preferring "concept": still "procedure"
}
#[tokio::test] async fn a_category_a_person_cleared_stays_cleared() {}
#[tokio::test] async fn a_category_a_writer_gave_is_left() { /* a merged artifact with category set: unchanged */ }
#[tokio::test] async fn pinned_is_stored_and_changes_no_tag_and_no_ranking() { /* yes 0.99: tags unchanged, search order unchanged */ }
#[tokio::test] async fn a_note_too_long_for_one_read_is_read_from_its_opening() {}
#[tokio::test] async fn a_retired_note_is_not_read() {}
#[tokio::test] async fn the_read_unit_takes_the_light_turn_and_is_not_held_with_the_generators() {}
```

- [ ] **Step 2–4: Run to fail (`cargo test --lib store::readings jobs::read`), implement, regenerate `.sqlx/`, run to pass.**

- [ ] **Step 5: Commit**

```bash
git add -A src .sqlx
git commit -m "feat(readings): every note is read once by the decision model: kind, pin, shelf life, whether it stands alone"
```

---

### Task 2: The window pre-read

**Files:**
- Modify: `src/jobs/window.rs` (~53–68)

**Interfaces:**
- Consumes: `catalogue::{asks_reminder, has_date}`, `state::{window, room}`.
- Produces:
  ```rust
  pub const PRE_READ_LINE: f64 = 0.3;
  /// Whether this window is synthesized with the judged schema.
  pub(crate) async fn wants_judged(core: &Core, corpus_id: &str, text: &str, forced_intent: Option<&str>, single_window: bool) -> Result<bool>;
  ```
  `single_window` false → false, as today. A forced intent → true without a call. The text does not fit → true without a call. Otherwise one request with both questions; true when either is at or above the line. The answers go into the corpus metadata as `pre_read`. A failed call → true (when in doubt, the judged schema), logged.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_note_that_asks_to_be_reminded_is_synthesized_with_the_judged_schema() { /* asks_reminder 0.97 */ }
#[tokio::test] async fn a_note_with_a_deadline_is_too() { /* has_date 0.31, asks_reminder 0.0 */ }
#[tokio::test] async fn a_note_with_neither_gets_the_plain_schema() { /* both 0.29: segment_judged is called with judge None */ }
#[tokio::test] async fn a_forced_intent_needs_no_question() { /* decider.calls() == 0, judged */ }
#[tokio::test] async fn a_window_too_long_to_ask_about_is_judged() { /* calls() == 0, judged */ }
#[tokio::test] async fn a_pre_read_that_fails_falls_to_the_judged_schema() {}
#[tokio::test] async fn a_capture_of_several_windows_is_never_judged() { /* as today; calls() == 0 */ }
#[tokio::test] async fn the_two_answers_are_kept_with_the_capture() { /* corpus metadata pre_read */ }
#[tokio::test] async fn a_reminder_still_arms_from_the_writers_when() { /* an existing judgement.rs remind test, unchanged in outcome */ }
```
A `RecordingSynthesizer` variant records whether `input.judge` was `Some`.

- [ ] **Step 2–4: Run to fail (`cargo test --lib jobs::window jobs::judgement`), implement, run to pass.**

- [ ] **Step 5: Commit**

```bash
git add src/jobs src/infer/fake.rs
git commit -m "feat(capture): two questions before synthesis decide whether to ask for a moment"
```

---

### Task 3: The schema cut, and links by decision

**Files:**
- Modify: `src/infer/prompt.rs` (the two schemas ~1836–1899; `RawArtifact`, `RawLink`, `JudgedEnvelope`; `proposed_from`; `parse_judged_response`; `user_prompt` ~994; all ten `SYNTHESIZER_SYSTEM*` at the lines the "Map" in this task lists), `src/infer/mod.rs` (`ProposedArtifact`, `Judgement`, `ProposedLink`), `src/infer/fake.rs`, `src/jobs/window.rs` (`proposed_to_new`), `src/jobs/judgement.rs` (~99–117), `src/core/judging/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  // ProposedArtifact loses `category` and `pinned`. Judgement loses `links`. ProposedLink is deleted.
  // artifacts_schema(): required ["text","title","corpus_lines","caveats","tags"]
  // judged_artifacts_schema(): required ["artifacts","moment","events"]
  /// After a judged capture: ask link_kind between the anchor and each neighbour it was shown.
  pub async fn link_shown(core: &Core, anchor_id: &str, shown: &[String]) -> Result<usize>;   // links created
  // store
  pub async fn relate_by_kind(&self, a: &str, b: &str, kind: LinkKind, first_is_a: Option<bool>, weight: f64, half_life_days: f64) -> Result<()>;
  ```
  The JSON shape line in every language becomes `{"artifacts":[{"text":"...","title":"...","corpus_lines":[start,end],"caveats":["..."],"tags":["..."]}]}`. The lines describing `category`, `pinned` and `links`, and the words `link targets` in the NEIGHBORS heading, are removed in all ten. The neighbours stay in the prompt as context.

  Map of the lines to edit (from a read of `522f26aa`; verify): EN `:41, :44-46, :56-57, :59-60, :81-83, :87, :90`; DE `:130, :133, :147, :151-152, :173, :180, :183`; ES `:223, :226, :240, :244-245, :267, :273, :276`; FR `:316, :319, :333, :337-338, :360, :367, :370`; IT `:410, :413, :427, :430-431, :452, :459, :462`; NL `:502, :505, :519, :523-524, :545, :551, :554`; PL `:592, :595, :609, :613, :634, :640, :643`; PT `:682, :685, :699, :703-704, :726, :732, :735`; RU `:775, :778, :792, :796-797, :817, :823, :826`; TR `:863, :866, :878, :882-883, :904, :911, :914`.

  `link_shown`: for each shown neighbour still in results, one request (`link_kind` + `link_direction`, state `state::link` with no cues), `ask`, `settle`. The `LinkApplier` from part 3 creates the row with `relate_by_kind` at weight `associate.show_min * 2.0` when a pick is a related kind and no row exists.

- [ ] **Step 1: Write the failing tests** (those that pinned category, pinned or links from the synthesizer are rewritten or deleted; the list is in the exploration notes of the spec's corrections):

```rust
// prompt.rs
#[test] fn the_schemas_ask_for_no_category_no_pin_and_no_links() { /* neither schema's JSON text contains "category", "pinned" or "links" */ }
#[test] fn the_translations_carry_the_same_contract() { /* CONTRACT updated: the new shape line; no "pinned"; no artifact_id/reason pair */ }
#[test] fn no_language_still_tells_the_writer_to_categorise_pin_or_link() {
    // for each Lang::ALL: synthesizer_system(lang) contains none of "\"category\"", "\"pinned\"", "\"links\""
}
#[test] fn a_reply_that_still_carries_the_old_fields_parses() { /* an older model's reply with category/pinned/links is accepted and they are ignored */ }
#[test] fn a_judged_reply_parses_moment_events_and_tags() {}
// window.rs / judgement.rs
#[tokio::test] async fn a_written_artifact_has_no_category_until_it_is_read() { /* None after the window unit; set after the drain */ }
#[tokio::test] async fn a_neighbour_shown_at_capture_is_linked_when_the_decision_model_is_sure() {
    // judged capture with one neighbour; rule: link_kind example 0.95 both ways -> a Related link with kind Example
}
#[tokio::test] async fn a_neighbour_it_calls_unrelated_gets_no_link() {}
#[tokio::test] async fn a_torn_neighbour_is_an_unsure_row_and_a_pick_creates_the_link() {}
#[tokio::test] async fn events_still_land_from_the_writer() { /* events_land_and_links_only_to_what_was_shown, without the links half */ }
// web/ui.rs
#[tokio::test] async fn the_search_page_offers_a_chip_for_what_the_collection_contains() { /* the chip is the category the reading gave */ }
```

- [ ] **Step 2–4: Run to fail, implement, run to pass.**

Run: `cargo test && cargo test --features contained --lib`

- [ ] **Step 5: Commit**

```bash
git add -A src
git commit -m "refactor(capture): the writer no longer categorises, pins or links; ten prompts say so"
```

---

### Task 4: The after-write check

**Files:**
- Create: `src/core/judging/check.rs`
- Modify: `src/config.rs` (`refuse_below: f64`, default 0.25, in `0.0..=1.0`), `config.example.toml`, `src/jobs/merge_unit.rs`, `src/jobs/rescue_unit.rs`, `src/jobs/condense.rs` (~102–120), `src/jobs/pursuit.rs` (~112–156), `src/jobs/window.rs` (after `write_segment_artifacts` ~206)

**Interfaces:**
- Consumes: `catalogue::{complete, adds_nothing}`, `state::{rewrite, room, Note}`, `log_decision`, `set_decision_outcome`.
- Produces:
  ```rust
  pub enum Checked { Passed, Refused { question: &'static str, yes: f64 }, TooLong }
  pub enum Which { Both, AddsNothingOnly }
  /// One request. Logs a decision of kind "write" on `subject`; a refusal sets its outcome.
  pub async fn check(core: &Core, subject: &str, originals: &[Note<'_>], rewrite: &Note<'_>, which: Which) -> Result<Checked>;
  ```
  Where it runs, always after the literal checks pass and before anything is stored:
  - merge (`Both`, originals = the members): refused → `leave_both` with `a check found the draft unfaithful; both stay as they are`.
  - rescue (`Both`, originals = the source excerpts): refused → stand the note down.
  - condense (`Both`, original = the artifact): refused → return, as a lost literal does today.
  - generate (`AddsNothingOnly`, originals = the packed sources): refused → the pursuit closes unsatisfied.
  - capture synthesis (`AddsNothingOnly`, original = the window), per proposed artifact, before `write_segment_artifacts`: refused → that artifact is dropped from the batch.
  `TooLong` everywhere means: proceed on the literal checks alone.

- [ ] **Step 1: Write the failing tests**

```rust
// check.rs
#[tokio::test] async fn a_faithful_rewrite_passes_at_fifty_five() { /* complete 0.55, adds_nothing 0.87 -> Passed; a decision row, no outcome */ }
#[tokio::test] async fn a_rewrite_that_dropped_a_sentence_is_refused() { /* complete 0.03 -> Refused { "complete", 0.03 }; outcome Refused */ }
#[tokio::test] async fn a_rewrite_that_added_a_fact_is_refused() { /* adds_nothing 0.04 */ }
#[tokio::test] async fn exactly_the_line_passes() { /* 0.25 */ }
#[tokio::test] async fn adds_nothing_only_asks_one_question() {}
#[tokio::test] async fn a_pair_that_does_not_fit_is_not_judged() { /* TooLong, calls() == 0, no decision row */ }
// per writer
#[tokio::test] async fn a_merge_that_fails_the_check_is_not_written_and_both_stay() {}
#[tokio::test] async fn a_rescue_that_fails_the_check_is_not_stored() {}   // the spec's "Done when"
#[tokio::test] async fn a_condensation_that_fails_the_check_changes_nothing() {}
#[tokio::test] async fn a_generated_artifact_that_adds_a_fact_is_not_stored_and_the_pursuit_closes() {}
#[tokio::test] async fn a_synthesized_artifact_that_adds_a_fact_is_dropped_and_the_passages_remain() {
    // two proposed artifacts, the rule refuses the second: one written; the window's passages are still in results
}
#[tokio::test] async fn a_lost_literal_is_still_caught_before_the_model_is_asked() { /* decider.calls() == 0 for the check */ }
#[tokio::test] async fn too_long_falls_back_to_the_literal_checks_alone() {}
```

- [ ] **Step 2–4: Run to fail, implement, run to pass.**

Run: `cargo test --lib core::judging::check jobs::`

- [ ] **Step 5: Commit**

```bash
git add -A src config.example.toml
git commit -m "feat(writers): every background rewrite is checked against its originals before it is stored"
```

---

### Task 5: The before-write gates

**Files:**
- Modify: `src/jobs/pursuit.rs` (`generate` ~13–165), `src/jobs/promote.rs` (`maybe_promote` ~16–94), `CHANGELOG.md`

**Interfaces:**
- Consumes: `catalogue::{answerable, self_contained}`, `state::answerable`, `Core::thresholds`, `reading_yes`, `SELF_CONTAINED_AT`.
- Produces: no new public items. Behaviour:
  - **Generate:** after packing and before the writer call, one `answerable` request over the questions and the packed sources. Yes at or above `act_at` → write. Below `ask_at` → the pursuit closes unsatisfied, no writer call. In between → return; the pursuit stays open and the unit is tried again when the pursuit gains a source (the existing arming). Not fitting → as today, write. The answer is logged as a decision of kind `pursuit`; it opens no Unsure row (`force` it to `leave` in the log when in between).
  - **Promote:** a passage is promoted only when it has earned its activation, as today, **and** its `self_contained` reading is below 0.5 or missing.
  - Merge and rescue are already gated by part 3's decisions.

- [ ] **Step 1: Write the failing tests**

```rust
// pursuit.rs
#[tokio::test] async fn sources_that_hold_the_answer_are_written_from() {}
#[tokio::test] async fn sources_that_do_not_close_the_pursuit_without_a_writer_call() { /* generator.calls() == 0; unsatisfied */ }
#[tokio::test] async fn in_between_the_pursuit_waits_and_is_tried_again_with_another_source() {}
#[tokio::test] async fn a_pursuit_too_long_to_ask_about_is_written_as_before() {}
#[tokio::test] async fn the_gate_opens_no_unsure_row() {}
// promote.rs
#[tokio::test] async fn a_used_passage_that_stands_alone_is_not_rewritten() { /* self_contained 0.8: no SegmentWindow job, no Promote action */ }
#[tokio::test] async fn a_used_fragment_is_promoted() { /* 0.23 */ }
#[tokio::test] async fn a_passage_not_read_yet_is_promoted_as_before() {}
```

- [ ] **Step 2–4: Run to fail, implement, run to pass.**

- [ ] **Step 5: `CHANGELOG.md`**, under `### Changed`:

```
- Every note is read once by the decision model: its kind, whether it records a decision, whether it will go out of date, whether it stands alone. Verbatim passages get a category this way. A category you set is never overwritten.
- The chat model no longer categorises, pins or links at capture; it writes. Reminders are still read by it, when two quick questions say there may be one.
- Background rewrites (merge, rescue, condense, generate, capture synthesis) are checked against their originals and refused when they drop or add something.
- A pursuit is written only when its sources hold the answer; a used passage that already stands alone is not rewritten.
```

- [ ] **Step 6: Run everything**

Run: `cargo test && cargo test --features contained --lib && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A src CHANGELOG.md
git commit -m "feat(writers): a pursuit is written only when its sources answer it; a passage that stands alone is left"
```
