# Dedupe, Links and Reap Move to the Decision Model (spec 2, stage 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The three background judges stop being chat calls. Each becomes: build the state, `ask`, apply the band. Judging and writing are split into separate units, and the chat judging prompts, schemas, parsers and completers are deleted.

**Architecture:** Each judge keeps its file and its guards and loses its model call: `dedupe::run`, `associate::judge` and `reap::run` call `judging::ask` + `judging::settle`, and an `Applier` per question carries out a pick, whether it came from the act band, a waiting decision, or a person's press on an Unsure row. Two writer units are new, `Stage::Merge` and `Stage::Rescue`; they are the only parts of these jobs that still call a chat model.

**Tech Stack:** Rust, sqlx, the layer from part 1, the catalogue and states from part 2.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md`, sections "`pair_relation`", "`link_kind`", "`reap_worth`", "Throughput", "Removed" and stage 3, with its "Corrections from the code". Part 3 of 5 on `feat/judge`. **Starts only after part 2's Task 4** (wording and thresholds fixed on the owner's base).

## Global Constraints

- No background judgement is made by a chat model. `DEDUPE_SYSTEM`, `LINK_SYSTEM`, `REAP_SYSTEM`, their schemas, builders and parsers, `Core::judge`, `Core::link_judge`, `Core::reaper`, the `(attempt N)` line and the judges' unreadable counters are deleted, not switched off.
- The guards stay exactly as they are: newest-wins on a replacement, the loss checks, the taken-back check before every action, `may_act`, the lifecycle lock around a burial.
- Every action that came from a judgement links to its decision row, and undoing it sets that row's outcome.
- A subject is asked once per era and again only when it changes (its members' `embed_rev`).
- Decision units are not in `GENERATORS`, take `gate.background_light()`, and are armed inside `Core::judging_budget()`. Writer units are in `GENERATORS` and take `gate.background()`.
- Raised defaults: `consolidate.max_dedupe_per_tick` 20, `associate.judge_per_sweep` 30, `reap.max_judged_per_run` 50. `reap.max_rescues_per_run` and the cooldown are untouched.
- No free text from the model reaches a reader or a stored row: `detail` and `reason` are dropped, not replaced.
- Regenerate `.sqlx/` after schema or query changes. Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs.

## Decisions this plan makes

- The existing "a person pressed Synthese" path is the writer path. `artifact_pairs.synthesis_asked` now also carries who asked: a new column `synthesis_by` (`model` | `operator`). `Stage::Merge` runs what `synthesize_asked_pair` does today; `Stage::Dedupe` only judges.
- A pair's decision has `subject_kind = "pair"`, `subject_a` = the pair id, and `rev_a` / `rev_b` = its members' `embed_rev`. A link's has `subject_kind = "link"`, `subject_a` / `subject_b` = the artifact ids. A reap decision has `subject_kind = "artifact"`.
- `a_replaces_b` and `b_replaces_a` pass the newest-wins guard only when the note named obsolete is the older; otherwise the pick is treated as `conflict`, as today.
- With no merge writer configured, a `duplicate` in the act band is forced to ask, and that row's buttons are `Keep A`, `Keep B`, `Conflict`, `Distinct`.
- A link gains two columns, `kind` and `first` (`a` | `b`). The reader's line is rendered from them at read time; `reason` stays for rows judged before and for the four fixed hand-over strings.
- The reader's lines: `continues` → `The next step after this` / `The step before this`; `example` → `An example of this` / `What this is an example of`; `background` → `Explains what this uses` / `Uses what this explains`; `alternative` → `Another way to do this`; `contradicts` → `States this differently`; `duplicate` → the hand-over strings as today.
- `link_direction` is asked in the same request as `link_kind`, always, and read only for a directional kind.
- `reap::run` still nominates and judges in the sweep; a `yes` (valuable) in the act band arms one `Stage::Rescue` unit, a `no` buries, in between is an Unsure row.
- The rescue writer's prompt opens with a fixed line where the judge's reason was: `What the live base lacks: something this note states and the live notes shown do not.` The stored cue is `reap rescue`.
- Burials are no longer explained by a reason: the graveyard's `meta_json` has no `reason`, the journal row no `detail`, and Insights shows `buried {ago} — out of search and out of the index, text kept`.
- Where the code asked "is there a judge" (`core.judge.is_some()` in `background.rs:74,88` and `api.rs:1666`), the answer is now always yes: the checks keep their other conditions and lose that one.

## Review Focus

1. A pair asked, left, and then one member edited: it is asked again; untouched, it is not, however many ticks pass. Task 1.
2. A `duplicate` in the act band whose merge the writer then fails to write faithfully: both notes stay, the pair is closed, the decision is not counted as kept. Task 2.
3. A burial that would exceed the daily cap: the decision waits, the note is not buried, and the next day's sweep buries it without asking again. Task 4.
4. A link judged before this change, with a free-text reason: it still shows that reason. Task 3.
5. A base with open pairs and learning links at upgrade: nothing is lost and nothing is judged twice by the old path; the old counters are ignored. Task 5.

## File Structure

| File | Responsibility |
|---|---|
| `src/jobs/dedupe.rs` (modify) | the decision unit, `PairApplier`, the guards |
| `src/jobs/merge_unit.rs` (create) | `Stage::Merge`: the writer |
| `src/jobs/associate.rs`, `src/store/links.rs` (modify) | the link decision unit, `LinkApplier`, `kind` / `first`, the rendered line |
| `src/jobs/reap.rs` (modify), `src/jobs/rescue_unit.rs` (create) | the reap decision in the sweep, `ReapApplier`, `Stage::Rescue` |
| `src/store/pairs.rs`, `src/store/schema.sql`, `src/store/mod.rs` (`ADDITIVE`) | `synthesis_by`, `kind`, `first`; the to-judge queries |
| `src/store/jobs.rs`, `src/jobs/mod.rs`, `src/core/background.rs`, `src/jobs/consolidate.rs` | the two new stages, `GENERATORS`, arming inside the budget |
| `src/core/mod.rs`, `src/infer/prompt.rs`, `src/infer/openai.rs`, `src/config.rs` | registration of the appliers; the deletions; the raised defaults |
| `src/web/insights.rs`, `src/web/api.rs`, `src/web/artifact.rs`, `src/core/search.rs` | what loses its reason text |

---

### Task 1: The pair decision

**Files:**
- Modify: `src/jobs/dedupe.rs`, `src/store/pairs.rs` (`pairs_to_judge` ~1020), `src/jobs/consolidate.rs` (`arm_dedupe` ~314), `src/core/mod.rs` (register the applier), `src/config.rs` (default 20), `src/store/jobs.rs` (`Dedupe` out of `GENERATORS` and `calls_a_generator`)

**Interfaces:**
- Consumes: `judging::{ask, settle, Applier, Subjects}`, `catalogue::pair_relation`, `state::{pair, room, Note}`, `roots_of`, `artifacts_by_ids`, `Core::supersede_with`, `discard_both`, `ask_pair_synthesis`.
- Produces:
  ```rust
  pub struct PairApplier;                       // question "pair_relation", label "Same thing?", job Some(Job::Dedupe)
  /// What a pick does to a pair, guards included. Returns the journal rows written.
  pub(crate) async fn carry_out(core: &Core, pair: &ArtifactPair, pick: &str, by: DecidedBy, decision_id: &str) -> Result<Vec<String>>;
  // store
  /// Pending pairs with no decision in this era on their members as they are now.
  pub async fn pairs_to_judge(&self, era: &str, limit: i64) -> Result<Vec<ArtifactPair>>;
  pub async fn ask_pair_synthesis_by(&self, id: i64, by: DecidedBy) -> Result<bool>;   // sets synthesis_asked and synthesis_by
  ```
  `dedupe::run(core, pair_id)`: the early-outs and structural refusals of today up to the context build (`:61-176`), unchanged, except that a pair with `synthesis_asked` is not this unit's business and returns. Then `state::pair` with members dated by `created_at` and sources oldest first; `ask` (a `None` state logs `too_long`, and the pair is left as it is, not closed); `settle`.

  `carry_out`, per pick, is today's `apply` (`:461-558`) with the letter replaced by the direction:
  - `distinct` → `NoConflict`. `conflict` → `Contradiction`. `vacuous` → `discard_both`.
  - `a_replaces_b` / `b_replaces_a` → the newest-wins guard; failing it, `Contradiction`. Passing: the checks and `supersede_with` of today.
  - `duplicate` → the taken-back check, then `ask_pair_synthesis_by(pair.id, by)` and arm `Stage::Merge` for the pair. No journal row here; the merge unit writes them and links them to `decision_id` (kept in the pair's journal evidence as `"decision"`).
  - `keep_a` / `keep_b` (buttons only) → supersede the other, as an operator.
  - Every `settle*` call passes `detail: None` unless it is one of the existing fixed refusal sentences.

  `PairApplier::unchanged`: both members in results and their `embed_rev` equal `rev_a` / `rev_b`. `reopen`: `reopen_pair` (made `pub(crate)`). `buttons`: `Duplicate`, `Replaced ←` (`b_replaces_a`), `Replaced →` (`a_replaces_b`), `Conflict`, `Distinct`; with no `pair_synthesizer`, `Keep A` and `Keep B` in place of `Duplicate`.

  Between `ask` and `settle`: band act, pick `duplicate`, and no `pair_synthesizer` → `judging::demote_to_ask(core, d, "no_writer")`.

  `arm_dedupe` arms `min(max_dedupe_per_tick, judging_budget())` pairs from `pairs_to_judge(era, 200)`; pairs awaiting synthesis are no longer in this queue.

- [ ] **Step 1: Write the failing tests** in `src/jobs/dedupe.rs`. The existing tests that script `core.judge` with JSON are rewritten onto a `RuleDecider` answering `pair_relation` (forward and reversed in agreement unless the test says otherwise); their names and assertions stay wherever the behaviour stays. A helper `fn deciding(core: &mut Core, pick: &str, top: f64) -> Arc<RuleDecider>`.

```rust
// kept, on the new judge (same assertions as today):
//   two_artifacts_about_different_subjects_are_never_merged, a_value_conflict_is_kept_as_a_disagreement_and_never_merged,
//   a_vacuous_verdict_retires_both_sides_without_waiting_for_an_operator, a_replacement_journals_the_loser_naming_the_winner,
//   a_plain_replacement_supersedes_rather_than_merging, a_replacement_naming_the_newer_artifact_is_not_trusted,
//   an_action_taken_back_is_not_taken_again_on_the_same_subject, a_spent_budget_leaves_the_pair_pending_without_spending_the_judge,
//   a_pair_whose_member_was_retired_is_dismissed_without_a_call, a_merge_and_one_of_its_own_sources_is_dismissed_without_a_call,
//   a_pair_naming_a_passage_is_left_as_it_is_before_the_call, a_unit_settles_only_its_own_pair
// new:
#[tokio::test] async fn a_sure_replacement_links_its_journal_row_to_the_decision() { /* decision_actions holds the supersede row */ }
#[tokio::test] async fn undoing_that_replacement_marks_the_decision_undone() { /* unsupersede through web/ops's path -> outcome Undone */ }
#[tokio::test] async fn a_torn_pair_becomes_an_unsure_row_and_stays_pending() { /* top 0.7: open_asks has it; no journal row; state Pending */ }
#[tokio::test] async fn a_sure_pick_whose_reversed_read_disagrees_asks_instead_of_acting() {}
#[tokio::test] async fn a_weak_answer_is_left_and_not_asked_again_until_a_member_changes() {
    // top 0.4: pairs_to_judge excludes it; after set embed_rev + 1 on a member it is back
}
#[tokio::test] async fn a_new_era_asks_everything_again() { /* the same pair under a different model name is back in the queue */ }
#[tokio::test] async fn members_that_do_not_fit_are_left_and_counted_without_a_call() { /* reason too_long; pair still pending; calls() == 0 */ }
#[tokio::test] async fn merge_sources_are_dropped_before_the_pair_is_refused() {}
#[tokio::test] async fn a_sure_duplicate_asks_for_the_merge_and_writes_nothing_itself() {
    // synthesis_asked, synthesis_by == Model, a Merge job is live, no artifact was inserted, no chat call
}
#[tokio::test] async fn a_duplicate_with_no_writer_becomes_an_unsure_row_with_keep_one_buttons() {
    // pair_synthesizer None: open row; buttons contain "Keep A" and "Keep B" and not "Duplicate"; no chat call  (the spec's "Done when")
}
#[tokio::test] async fn keep_a_from_a_person_supersedes_b_past_the_cap() {}
#[tokio::test] async fn a_person_answering_distinct_closes_the_pair_and_can_ask_again() { /* ask_again -> pending */ }
#[tokio::test] async fn the_dedupe_unit_no_longer_holds_for_generation_or_waits_out_the_cooldown() {}
#[tokio::test] async fn arming_stops_at_what_the_tick_has_left() { /* max_per_tick 2, five pairs -> two armed */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::dedupe jobs::consolidate`
Expected: compile errors, then failures.

- [ ] **Step 3: Implement.** `interpret` and `Settlement` go; `build_prompt` goes. Leave `synthesize_asked_pair` and `write_merge` in place for Task 2.

- [ ] **Step 4: Regenerate `.sqlx/`, run, commit**

Run: `cargo test --lib jobs:: store::pairs core::judging`
```bash
git add -A src .sqlx
git commit -m "feat(dedupe): the pair judge is a decision; it judges and no longer writes"
```

---

### Task 2: The merge writer unit

**Files:**
- Create: `src/jobs/merge_unit.rs`
- Modify: `src/jobs/dedupe.rs` (move `synthesize_asked_pair`, `write_merge`, `synthesis_prompt` out), `src/store/jobs.rs` (`Stage::Merge`, `"merge"`, class 1, a generator), `src/jobs/mod.rs` (dispatch), `src/store/schema.sql` + `src/store/mod.rs` (`synthesis_by TEXT`, one `ADDITIVE` entry, the array length up by one), `src/web/ui.rs` (label `Writing merges`)

**Interfaces:**
- Consumes: `pair_synthesizer`, `SYNTHESIZE_SYSTEM`, `parse_synthesis`, `merge::{write, losses}`, `set_pair_merged`, `leave_both`, `link_decision_actions`.
- Produces:
  ```rust
  pub async fn run(core: &Core, pair_id: &str) -> Result<()>;   // Stage::Merge
  ```
  It is today's `synthesize_asked_pair` with three changes: the journal rows carry `asked_by: "operator"` only when `synthesis_by` is the operator; the rows are linked to the decision named in the pair's ask (`decisions` → `last_logged("pair_relation", pair id, None, era)`); a refusal (lost literal, did not fit, unreadable past the ceiling) sets that decision's outcome to `refused`.

- [ ] **Step 1: Write the failing tests** (the five operator-synthesis tests of `dedupe.rs` move here with their assertions; new ones:)

```rust
#[tokio::test] async fn a_merge_the_model_asked_for_is_written_and_linked_to_its_decision() {
    // one Kind::Merge row per source, none with asked_by; decision_actions holds them; pair merged_into set; decided_by Model
}
#[tokio::test] async fn undoing_the_merge_marks_the_decision_undone() {}
#[tokio::test] async fn a_draft_that_drops_a_literal_is_refused_and_the_decision_says_so() { /* both stay; outcome Refused; no artifact */ }
#[tokio::test] async fn a_merge_a_person_asked_for_is_not_counted_against_the_band() { /* asked_by operator, as today */ }
#[tokio::test] async fn the_merge_unit_holds_while_generation_is_shut() { /* Stage::Merge is in GENERATORS; Stage::Dedupe is not */ }
#[tokio::test] async fn with_no_writer_the_unit_leaves_the_ask_standing() {}
```

- [ ] **Step 2–4: Run to fail, implement, regenerate `.sqlx/`, run to pass.**

Run: `cargo test --lib jobs::merge_unit jobs::dedupe store::jobs`

- [ ] **Step 5: Commit**

```bash
git add -A src .sqlx
git commit -m "feat(dedupe): merges are written by their own unit, after a decision asked for them"
```

---

### Task 3: The link decision

**Files:**
- Modify: `src/jobs/associate.rs` (`judge` ~308, the arming loop ~91), `src/store/links.rs` (`links_to_judge` ~595, `set_link_state` ~711, `links_from` ~513, `Link`, `LinkedTo`), `src/store/schema.sql` + `src/store/mod.rs` (`kind TEXT`, `first TEXT`: two `ADDITIVE` entries), `src/config.rs` (default 30), `src/store/jobs.rs` (`LinkJudge` out of `GENERATORS`), `src/core/mod.rs`

**Interfaces:**
- Consumes: `catalogue::{link_kind, link_direction}`, `state::link`, the layer.
- Produces:
  ```rust
  pub enum LinkKind { Continues, Example, Alternative, Background, Contradicts, Duplicate, Unrelated }   // as_str / parse
  /// The line a reader sees beside a link, from the side of `from_a`; None for kinds that show none.
  pub fn link_line(kind: LinkKind, first_is_a: Option<bool>, from_a: bool) -> Option<&'static str>;
  pub struct LinkApplier;                       // question "link_kind", label "Related how?", job None
  // store
  pub async fn set_link_kind(&self, a: &str, b: &str, state: LinkState, kind: Option<LinkKind>, first_is_a: Option<bool>, judged_revs: Option<(i64, i64)>) -> Result<()>;
  pub async fn links_to_judge(&self, era: &str, min_weight: f64, min_queries: i64, half_life_days: f64, at: i64, limit: i64) -> Result<Vec<Link>>;
  // LinkedTo.reason: the rendered line when `kind` is set, else the stored reason
  ```
  One request carries `link_kind` and `link_direction`; the decision logged is `link_kind`'s, with `link_direction`'s pick kept in its `answer` JSON under `"direction"`. What a pick does:
  - `continues`, `example`, `alternative`, `background` → `Related`, with the kind and, for the three directional ones, `first` from `link_direction` (its swapped read disagreeing → `first` unknown, and the neutral line `Related to this`).
  - `contradicts` → hand the pair to consolidation (`record_pair_with_detail(.., 0.0, "link")`, not over passages, exactly as `duplicate` is handed over today) and `Related` with `States this differently`.
  - `duplicate` → today's hand-over and its four fixed strings, unchanged.
  - `unrelated` → `Unrelated`.
  No journal row is written for any of them. `reopen`: state `learning`, kind and first cleared.

- [ ] **Step 1: Write the failing tests** (the judge tests of `associate.rs` rewritten onto a `RuleDecider`; those about unreadable replies and the attempt counter are deleted with what they test):

```rust
#[test] fn each_kind_renders_a_line_for_each_side() {
    // The line describes the OTHER note, seen from the one being read.
    // continues: reading the first note, the other is "The next step after this"; reading the second, "The step before this".
    // example (first = the general description): reading it, the other is "An example of this"; reading the instance, "What this is an example of".
    // background (first = the explanation): reading the note that uses it, the other "Explains what this uses"; reading the explanation, "Uses what this explains".
    // alternative, either side: "Another way to do this". unrelated: None.
}
#[tokio::test] async fn a_sure_kind_is_stored_and_shown_as_its_line() { /* state Related, kind Example, first set; links_from(..).reason == the line */ }
#[tokio::test] async fn a_directional_kind_whose_direction_is_torn_shows_the_neutral_line() {}
#[tokio::test] async fn an_unrelated_verdict_is_stored_so_it_is_not_asked_again() {}
#[tokio::test] async fn a_torn_link_is_an_unsure_row_and_stays_learning() {}
#[tokio::test] async fn a_person_picking_a_kind_sets_it_and_can_ask_again() {}
#[tokio::test] async fn a_contradiction_between_linked_notes_is_handed_to_consolidation() {}
#[tokio::test] async fn a_disguised_duplicate_is_handed_to_consolidation_and_still_shown() { /* as today */ }
#[tokio::test] async fn a_duplicate_verdict_over_passages_is_not_handed_to_consolidation() { /* as today */ }
#[tokio::test] async fn a_link_is_asked_once_per_era_and_again_when_a_text_changes() {}
#[tokio::test] async fn a_link_judged_before_this_change_keeps_its_written_reason() { /* reason set, kind NULL -> shown */ }
#[tokio::test] async fn the_searches_that_bound_them_are_in_the_state() { /* the request's state contains "Found together by these searches: q" */ }
#[tokio::test] async fn a_queued_unit_spends_no_call_once_search_recording_is_switched_off() { /* as today */ }
```

- [ ] **Step 2–4: Run to fail, implement, regenerate `.sqlx/`, run to pass.**

Run: `cargo test --lib jobs::associate store::links web::artifact core::search mcp::`

- [ ] **Step 5: Commit**

```bash
git add -A src .sqlx
git commit -m "feat(links): a link is judged by kind, and its line is rendered, not written"
```

---

### Task 4: The reap decision and the rescue unit

**Files:**
- Modify: `src/jobs/reap.rs`, `src/store/jobs.rs` (`Reap` out of `GENERATORS`; `Stage::Rescue`, `"rescue"`, a generator), `src/jobs/mod.rs`, `src/core/background.rs:88`, `src/web/api.rs:1666`, `src/web/insights.rs` (~441–466), `src/config.rs` (default 50), `src/core/mod.rs`
- Create: `src/jobs/rescue_unit.rs`

**Interfaces:**
- Consumes: `catalogue::reap_worth`, `state::reap`, `live_neighbours` (made `pub(crate)`), the layer, `reap_one`, `rescue_one`.
- Produces:
  ```rust
  pub struct ReapApplier;                       // question "reap_worth", label "Worth keeping?", job Some(Job::Reap)
  pub async fn rescue_unit::run(core: &Core, artifact_id: &str) -> Result<()>;   // Stage::Rescue
  ```
  `reap::run` per nominee: the taken-back stand-down as today; `state::reap`; `ask`; `settle`. Picks: `no` → `reap_one` (its journal row is the decision's action; no `detail`, no `reason` in `meta_json`). `yes` → arm `Stage::Rescue` for the artifact, inside `max_rescues_per_run` per run. Buttons: `Valuable`, `Worthless`. `unchanged`: still `REAPABLE` (a `reap_candidates`-style check for the one id). `reopen`: `restamp_retired`.

  `rescue_unit::run` is `rescue_one` with the fixed opening line and cue from "Decisions this plan makes"; an inference outage is retried by the queue, any other failure stands the note down as today.

  `Report` keeps `judged`, `reaped`, `rescued`, `stamped`; `rescued` now counts units armed.

- [ ] **Step 1: Write the failing tests** (reap's tests rewritten onto a `RuleDecider`; `the_judge_sees_the_successor_and_answers_from_the_script` becomes a state test):

```rust
#[tokio::test] async fn a_sure_no_buries_and_the_burial_is_the_decisions_action() {
    // graveyard holds the text, meta_json has no "reason", the journal row has no detail, decision_actions links it
}
#[tokio::test] async fn restoring_that_note_marks_the_decision_undone() {}
#[tokio::test] async fn a_sure_yes_arms_a_rescue_and_buries_nothing() {}
#[tokio::test] async fn in_between_is_an_unsure_row_and_the_note_stays() { /* buttons Valuable, Worthless */ }
#[tokio::test] async fn the_burial_line_is_the_higher_one() { /* [decide.bands.reap_worth] act_at 0.97: a no at 0.95 asks */ }
#[tokio::test] async fn a_burial_past_the_daily_cap_waits_and_happens_later_without_a_second_call() {}
#[tokio::test] async fn a_burial_taken_back_before_is_not_bought_again() { /* as today: calls() == 0 */ }
#[tokio::test] async fn the_retired_note_its_replacement_and_live_notes_are_what_is_shown() {}
#[tokio::test] async fn a_rescue_is_written_from_the_fixed_line_and_supersedes_the_retired_note() { /* the writer's prompt starts with the fixed line */ }
#[tokio::test] async fn the_rescue_cap_holds() { /* as today */ }
#[tokio::test] async fn a_person_pressing_worthless_buries_past_the_cap() {}
#[tokio::test] async fn reap_runs_without_a_synthesize_role_and_only_rescue_needs_one() {
    // test_core_without_synthesizer: a sure no still buries; a sure yes arms nothing and logs why
}
#[tokio::test] async fn insights_shows_a_burial_without_a_reason() { /* "buried … — out of search and out of the index, text kept" */ }
```

- [ ] **Step 2–4: Run to fail, implement, run to pass.**

Run: `cargo test --lib jobs::reap jobs::rescue_unit web::insights web::api core::background`

- [ ] **Step 5: Commit**

```bash
git add -A src .sqlx
git commit -m "feat(reap): burial is decided by the decision model; the rescue is its own unit"
```

---

### Task 5: Delete the chat judges

**Files:**
- Modify: `src/infer/prompt.rs`, `src/infer/openai.rs`, `src/core/mod.rs`, `src/store/pairs.rs`, `src/store/links.rs`, `src/jobs/associate.rs`, `CHANGELOG.md`, `config.example.toml`

- [ ] **Step 1: Write the failing test** in `tests/csp_static.rs` (it already scans the source tree):

```rust
#[test] fn no_background_judgement_is_made_by_a_chat_model() {
    // src/ contains none of: "DEDUPE_SYSTEM", "LINK_SYSTEM", "REAP_SYSTEM", "dedupe_schema", "link_schema", "reap_schema",
    // "parse_dedupe", "parse_link", "parse_reap", "for_judging", "for_link_judging", "for_reaping", "(attempt "
    // and the non-test part of jobs/dedupe.rs, jobs/associate.rs and jobs/reap.rs contains no ".complete("
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test csp_static no_background_judgement`
Expected: FAIL, listing what is left.

- [ ] **Step 3: Delete.** The three prompts with their schemas, builders, parsers and prompt tests; `Relation`, `Dedupe`, `DedupeMember`, `Reap`, `ReapCase`; `Core::{judge, link_judge, reaper}` and their construction; `HttpCompleter::{for_judging, for_link_judging, for_reaping}`; `record_judge_attempt` where only a judge called it; `record_link_judge_attempt` and `MAX_UNREADABLE_LINK_JUDGEMENTS`. `judge_unreadable` and `record_unreadable_judgement` stay for the merge writer, whose reply is still parsed. The columns `judge_attempts` (pairs and links) stay in the schema and are no longer read.

- [ ] **Step 4: Upgrade check.** Add to `src/store/pairs.rs` tests:

```rust
#[tokio::test] async fn pairs_and_links_left_mid_judgement_by_an_older_version_are_simply_asked() {
    // a pending pair with judge_attempts 3 and judge_unreadable 4, a learning link with judge_attempts 2:
    // both are returned by the to-judge queries
}
```

- [ ] **Step 5: `CHANGELOG.md`**, under `### Changed`:

```
- Duplicates, links and burials are judged by the decision model, not a chat model. A judgement it is sure of is carried out, one it is torn on is asked on Insights under "Unsure", the rest are left. Merges and rescues are still written by the chat model, each as its own unit after a decision asked for it.
- A link shows its kind ("An example of this", "Another way to do this") instead of a written sentence. Burials are no longer explained by a reason.
- Defaults raised: `consolidate.max_dedupe_per_tick` 20, `associate.judge_per_sweep` 30, `reap.max_judged_per_run` 50.
```

- [ ] **Step 6: Run everything**

Run: `cargo test && cargo test --features contained --lib && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings && cargo deny check`
Then regenerate `.sqlx/` and check `git diff --exit-code .sqlx` is clean after committing it.
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A src tests CHANGELOG.md config.example.toml .sqlx
git commit -m "refactor: the chat judging prompts, schemas, parsers and completers are gone"
```
