# New Refinement: Pair Net, Scan, Relevance Labels, Tuning (spec 2, stages 6 to 9) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The base judges far more of itself: pairs down to 0.70 similarity and the pairs a person's verdict on a decision queued; readings for every existing note; model relevance labels on recorded searches; and thresholds that move with the person's undos and answers.

**Architecture:** Everything here is a producer or a consumer of the layer. The pair net widens what `relate` files and marks what may not act alone. The scan and the labeller are armed by the `Decisions` sweep inside the tick's budget. The tuner gains one rule per question, writing `decision_thresholds`.

**Tech Stack:** Rust, sqlx, parts 1 to 4.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md`, sections "A wider pair net", "What Decide's verdicts feed", "The scan", "Relevance labels", "Tuning" and stages 6 to 9, with "Corrections from the code". Part 5 of 5 on `feat/judge`, after part 4. Each task ships alone, in this order.

## Global Constraints

- Below `review_min`, nothing acts on its own: a `conflict` in the act band opens a contradiction as today; `duplicate`, a replacement and `vacuous` go to the ask band; `distinct` and everything under `ask_at` are recorded and left.
- A person's verdict always wins. A model label never writes `search_events.verdict`, and model labels steer a sweep only while they agree with the person: over at least `decide.label_overlap_min` (30) judged searches, hit rate at least `decide.label_agree_min` (0.85) and selectivity at most `decide.label_select_max` (0.30).
- Tuning: with at least `decide.tune_min` (30) acted decisions whose window has closed, an undone share above `decide.undo_target` (0.05) raises `act_at` one step of 0.02; under half the target with a non-empty ask band lowers it; bounds `[0.80, 0.99]`. Decide's `settled_at` moves by the same rule within `[0.70, 0.95]`. A new era starts from config.
- All new decision calls are armed inside `Core::judging_budget()` and take the light background turn.
- Regenerate `.sqlx/` after schema or query changes. Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs.

## Decisions this plan makes

- **A pair is "restricted" (may not act alone) when** its similarity is below the live `review_min`, **or** nothing but similarity says it is a duplicate, **or** a judged decision queued it. Today `relate` files a pair only when one text contains the other or both come from one document; a bare similarity files nothing. The wider net therefore also files uncorroborated pairs, down to `scan_min`, and every one of them is restricted whatever its score. The pair row says why in a new column, `net` (`null` | `wide` | `decide`).
- The rule that a replacement must not retire the note a person confirmed needs no extra code: every pair a decision queued is restricted, so its replacement is always an ask row. A test pins it.
- The scan runs only while the base is quiet (`evolve.idle_secs`), newest notes first.
- `time_bound` reaches the stale test in `Core::stale_candidates`, by filtering what the vector store returns against `artifact_readings`. The three vector stores are not changed. (If the replay dropped `time_bound`, Task 2 leaves the stale test and the detail page alone.)
- Relevance labels: one request per candidate, state the candidate alone. A search is labelled once; twenty candidates are one unit.
- Decide's reads add one figure, not two. A note becomes evidence only when `bears` said yes, so "the model also called it relevant" is true by construction for every person-confirmed or person-refuted note. What is informative is the split: of the bearing notes a person ruled on, how many held up. Insights shows `bears held up: N of M`.
- `ask_at` moves one step up when a row lapsed since the last move, and one step down when the person has answered every open row since the last move and decisions are waiting for a seat; bounds `[0.50, act_at - 0.02]`.
- `answers_query` replaces the similarity line where a gap is covered and where a pursuit counts as answered: the candidate is still found by vector search, then asked; yes at `act_at` covers.

## Review Focus

1. Two notes about the same port with different numbers, from different captures, similarity 0.74: they are filed, asked, and a sure `conflict` opens a disagreement; a sure `duplicate` is an Unsure row and nothing is merged. Task 1.
2. A base with 5,000 notes on upgrade: the scan reads a tick's worth at a time, newest first, stops when the person searches, and never starves the pair judge of its budget. Task 2.
3. A recorded search whose candidates were since retired: the missing ones are skipped and the search still counts as labelled. Task 3.
4. A model that says yes to everything: hit rate 1.0, selectivity 1.0, and its labels are display only. Task 3.
5. A question with 29 acted decisions: nothing moves. A change of model: the moved thresholds are set aside and config is in force. Task 4.

## File Structure

| File | Responsibility |
|---|---|
| `src/config.rs`, `config.example.toml` | `consolidate.scan_min`; `decide.tune_*`, `label_*` |
| `src/jobs/relate.rs`, `src/store/pairs.rs`, `src/store/schema.sql`, `src/jobs/dedupe.rs` | the wide net, `net`, restriction |
| `src/core/decide/verdict.rs` (spec 1) | queues with `net = decide` |
| `src/jobs/decisions.rs`, `src/store/readings.rs`, `src/core/search.rs`, `src/web/artifact.rs`, `templates/_artifact_detail.html` | the scan and its uses |
| `src/store/search_labels.rs` (create), `src/jobs/label.rs` (create), `src/eval/sweep.rs`, `src/jobs/gaps.rs`, `src/jobs/pursuit.rs`, `src/web/insights.rs`, `templates/insights.html` | relevance labels |
| `src/jobs/tune.rs`, `src/core/decide/mod.rs`, `src/web/insights.rs`, `templates/_evolve.html` | tuning |

---

### Task 1: The wider pair net

**Files:**
- Modify: `src/config.rs` (`scan_min: f32`, default 0.70; must be below `review_min`), `src/jobs/relate.rs` (`run` ~17, `classify_pair` ~90–195), `src/store/pairs.rs`, `src/store/schema.sql` + `src/store/mod.rs` (`net TEXT`: one `ADDITIVE` entry), `src/jobs/dedupe.rs` (`run`, before `settle`), `src/core/decide/verdict.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum Net { Wide, Decide }                                        // as_str "wide" | "decide"; ArtifactPair.net: Option<Net>
  pub async fn record_pair_in(&self, a: &str, b: &str, score: f32, net: Option<Net>, detail: Option<&str>) -> Result<bool>;
  /// Whether this pair may act without a person.
  pub(crate) fn restricted(pair: &ArtifactPair, live_review_min: f32) -> bool;   // net.is_some() || (pair.score > 0.0 && pair.score < live_review_min)
  ```
  `relate::run` takes neighbours down to `scan_min`. `classify_pair` is unchanged down to its last test; there: corroborated and at or above `review_min` → `record_pair` as today; anything else at or above `scan_min` (uncorroborated at any score, or corroborated below `review_min`) → `record_pair_in(.., Some(Net::Wide), None)`. Passages and same-window pairs are still not filed.

  `dedupe::run`, for a restricted pair, after `ask`: pick `conflict` or `distinct` → `settle` as usual; any other pick in the act band → `judging::demote_to_ask(core, d, "restricted")`.

  Spec 1's `verdict.rs` switches from `record_pair_with_detail(.., 0.0, PAIR_DETAIL)` to `record_pair_in(.., 0.0, Some(Net::Decide), Some(PAIR_DETAIL))`. A pair handed over from a link (`detail = "link"`, score 0) stays unrestricted, as today.

- [ ] **Step 1: Write the failing tests**

```rust
// relate.rs
#[tokio::test] async fn two_notes_alike_at_seventy_four_from_different_captures_are_filed_in_the_wide_net() {}
#[tokio::test] async fn a_pair_under_scan_min_is_not_filed() {}
#[tokio::test] async fn an_uncorroborated_pair_above_review_min_is_filed_wide_not_plain() {}
#[tokio::test] async fn a_corroborated_pair_above_review_min_is_filed_as_before() { /* net None */ }
#[tokio::test] async fn passages_and_same_window_pairs_are_still_not_filed() {}
// dedupe.rs
#[tokio::test] async fn a_sure_conflict_in_the_wide_net_opens_a_disagreement() {}
#[tokio::test] async fn a_sure_duplicate_in_the_wide_net_is_asked_and_nothing_is_merged() { /* open row, reason "restricted", no Merge job */ }
#[tokio::test] async fn a_sure_replacement_in_the_wide_net_is_asked_and_nothing_is_superseded() {}
#[tokio::test] async fn a_sure_vacuous_in_the_wide_net_is_asked_and_nothing_is_discarded() {}
#[tokio::test] async fn a_distinct_or_weak_answer_in_the_wide_net_is_recorded_and_left() {}
#[tokio::test] async fn a_person_may_still_merge_a_wide_net_pair_from_the_unsure_row() {}
#[tokio::test] async fn a_pair_a_judged_decision_queued_is_asked_below_scan_min() { /* score 0.0, net Decide: in pairs_to_judge */ }
#[tokio::test] async fn a_replacement_that_would_retire_the_note_a_person_confirmed_becomes_an_unsure_row() {
    // spec 1: decide, judge Right, the against-note is queued; rule: a_replaces_b 0.99 both ways retiring the confirmed note
    // -> open row; the confirmed note is still in results      (the spec's "Done when")
}
#[tokio::test] async fn a_pair_handed_over_from_a_link_still_acts_alone() {}
// config.rs
#[test] fn scan_min_must_sit_below_review_min() {}
```

- [ ] **Step 2–4: Run to fail, implement, regenerate `.sqlx/`, run to pass.**

Run: `cargo test --lib jobs::relate jobs::dedupe jobs::consolidate core::decide store::pairs`

- [ ] **Step 5: Commit**

```bash
git add -A src config.example.toml .sqlx
git commit -m "feat(dedupe): pairs down to 0.70 are judged, and nothing below the review line acts alone"
```

---

### Task 2: The scan and its uses

**Files:**
- Modify: `src/jobs/decisions.rs`, `src/store/readings.rs`, `src/core/search.rs` (`stale_candidates` ~1018), `src/web/artifact.rs` (`ArtifactDetail` ~23, `build_artifact_detail` ~407), `src/web/templates/_artifact_detail.html` (the notices, ~6–49)

**Interfaces:**
- Consumes: `Stage::Read` and `reading_questions` (part 4), `judging_budget`, `store.activity_since`, `TIME_BOUND_AT`.
- Produces:
  ```rust
  /// Live artifacts with no reading at the current version of some reading question, newest first.
  pub async fn artifacts_lacking_readings(&self, versions: &[(&str, u32)], limit: i64) -> Result<Vec<String>>;
  // decisions::Report gains `scanned: u64`
  // ArtifactDetail gains `may_be_out_of_date: bool`
  ```
  `decisions::run`, last step: if the base has been quiet for `evolve.idle_secs`, arm `Stage::Read` for up to half of `judging_budget()` artifacts from `artifacts_lacking_readings`, skipping any with a live unit.

  `Core::stale_candidates(limit)`: ask the vector store for `limit * 4`, keep a hit when it has no `time_bound` reading or its yes is at least 0.5, truncate to `limit`.

  Detail page: `may_be_out_of_date` when the note is active, `time_bound` is at least 0.5, and `last_verified_at` is older than `consolidate.stale_after_days`. The notice, in the existing notice markup: `May be out of date`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn notes_from_before_readings_existed_are_read_newest_first_while_the_base_is_quiet() {}
#[tokio::test] async fn the_scan_stops_when_the_person_is_active() { /* a search just now: scanned == 0 */ }
#[tokio::test] async fn the_scan_takes_at_most_half_of_what_the_tick_has_left() {}
#[tokio::test] async fn a_note_already_read_at_this_version_is_not_read_again_and_a_new_version_reads_all_again() {}
#[tokio::test] async fn an_old_unused_note_that_is_not_time_bound_is_no_longer_a_stale_candidate() {}
#[tokio::test] async fn an_old_unused_time_bound_note_is() {}
#[tokio::test] async fn an_old_unused_note_with_no_reading_is_as_today() {}
#[tokio::test] async fn the_detail_page_says_a_time_bound_note_may_be_out_of_date() { /* contains "May be out of date"; a fresh or not-time-bound one does not */ }
```

- [ ] **Step 2–4: Run to fail, implement, regenerate `.sqlx/`, run to pass** (`cargo test --lib jobs::decisions core::search web::artifact && cargo test --test copy`).

- [ ] **Step 5: Commit**

```bash
git add -A src .sqlx
git commit -m "feat(readings): a quiet base reads the notes it has not read; shelf life joins the stale test"
```

---

### Task 3: Relevance labels

**Files:**
- Create: `src/store/search_labels.rs`, `src/jobs/label.rs`
- Modify: `src/store/schema.sql`, `src/store/jobs.rs` (`Stage::Label`, `"label"`, class 1, not a generator), `src/jobs/mod.rs`, `src/jobs/decisions.rs`, `src/config.rs` (`label_overlap_min` 30, `label_agree_min` 0.85, `label_select_max` 0.30), `src/eval/sweep.rs` (`evidence_pairs` ~355), `src/jobs/gaps.rs` (`cover_answering` ~98), `src/jobs/pursuit.rs` (`answered_since` ~173), `src/web/insights.rs` (~705–740), `src/web/templates/insights.html` (~31–38), `src/web/judge.rs`, `docs/evaluation.md`

**Interfaces:**
- Consumes: `catalogue::answers_query`, `state::one`, `Core::thresholds`, `search_candidates`, `feedback_stats`, spec 1's `decide_events` / `decide_evidence`.
- Produces:
  ```sql
  CREATE TABLE IF NOT EXISTS search_labels (
    event_id TEXT NOT NULL REFERENCES search_events(id) ON DELETE CASCADE,
    artifact_id TEXT NOT NULL,
    yes REAL NOT NULL,
    question_version INTEGER NOT NULL,
    model TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (event_id, artifact_id)
  );
  ```
  ```rust
  pub struct ModelRetrieval { pub searches: usize, pub left_out: usize, pub recall_at_10: f64, pub mrr: f64 }
  pub struct Agreement { pub judged: usize, pub hit_rate: f64, pub selectivity: f64 }
  pub struct BearsRecord { pub held_up: usize, pub ruled_on: usize }
  impl Store {
      pub async fn unlabelled_searches(&self, version: u32, model: &str, limit: i64) -> Result<Vec<String>>;   // oldest first
      pub async fn put_search_labels(&self, event_id: &str, rows: &[(String, f64)], version: u32, model: &str) -> Result<()>;
      pub async fn model_retrieval(&self, relevant_at: f64, version: u32, model: &str) -> Result<ModelRetrieval>;
      pub async fn label_agreement(&self, relevant_at: f64, version: u32, model: &str) -> Result<Agreement>;
      pub async fn bears_record(&self, model: &str) -> Result<BearsRecord>;
      /// The candidate the model rates highest for a search the person did not judge, if it is relevant.
      pub async fn model_expected(&self, event_id: &str, relevant_at: f64) -> Result<Option<String>>;
  }
  pub async fn label::run(core: &Core, event_id: &str) -> Result<()>;   // Stage::Label
  impl Core { pub async fn labels_count(&self) -> Result<bool>; }       // the three thresholds hold
  ```
  Definitions, with `relevant` meaning `yes >= act_at` of `answers_query`:
  - model recall@10 of a search: the share of its relevant candidates ranked in the first ten; model MRR: the reciprocal rank of the first relevant one. A search with no relevant candidate is `left_out`.
  - hit rate: over searches the person judged a hit, the share whose `expect_id` is relevant. selectivity: the mean share of a search's candidates that are relevant.
  - `bears_record`: over held decisions of this model judged `right` or `nothing_here`: `ruled_on` = bearing notes of both; `held_up` = notes of the `right` ones whose reading agreed.

  `label::run`: each candidate still in results is asked alone; missing ones are skipped; the search is stored as labelled even when none was asked (an empty marker row is not needed: `unlabelled_searches` excludes searches whose candidates are all gone). `decisions::run` arms `Stage::Label` for `unlabelled_searches` while the budget lasts, counting a search as its candidate count, after the pair judge's share (labels take at most a quarter of the tick).

  `sweep.rs`: an unjudged search contributes `model_expected` as its expected artifact only when `labels_count()`.

  `cover_answering` and `answered_since`: where the similarity line is tested today, the found artifact is instead asked `answers_query(the gap's or pursuit's question)`; yes at `act_at` covers. A call that fails leaves the gap open.

  Insights: under the existing recall@10 and MRR, a second line `by the model: recall@10 0.71 · MRR 0.52 · 14 searches left out`, then `agreement: hit rate 0.88 · selectivity 0.21 over 34 judged searches`, then `bears held up: 41 of 47`, and `model labels count toward sweeps` or `model labels are shown only`.

- [ ] **Step 1: Write the failing tests**

```rust
// store/search_labels.rs
#[tokio::test] async fn searches_are_labelled_oldest_first_and_once_per_version() {}
#[tokio::test] async fn model_recall_and_mrr_follow_the_definitions() {
    // 12 candidates, relevant at ranks 2, 5 and 11 -> recall@10 2/3, MRR 1/2
}
#[tokio::test] async fn a_search_with_nothing_relevant_is_left_out_and_counted() {}
#[tokio::test] async fn hit_rate_and_selectivity_follow_the_definitions() {}
#[tokio::test] async fn a_model_that_says_yes_to_everything_has_a_perfect_hit_rate_and_no_selectivity() { /* 1.0 and 1.0 */ }
#[tokio::test] async fn bears_is_scored_on_the_notes_a_person_ruled_on() { /* one right decision with 3 agreeing of 4 bearing; one nothing_here with 2 bearing -> 3 of 6 */ }
#[tokio::test] async fn deleting_a_search_takes_its_labels() {}
// jobs/label.rs
#[tokio::test] async fn each_candidate_is_asked_alone_about_the_search() { /* state holds one note; instructions quote the query */ }
#[tokio::test] async fn a_candidate_since_retired_is_skipped_and_the_search_still_counts() {}
#[tokio::test] async fn the_persons_verdict_is_never_touched() { /* search_events.verdict, expect_id, judged_at unchanged */ }
#[tokio::test] async fn labelling_takes_at_most_a_quarter_of_the_tick() {}
// eval/sweep.rs
#[tokio::test] async fn an_unjudged_search_counts_only_while_the_model_agrees_with_the_person() {
    // 30 judged searches at hit rate 0.9, selectivity 0.2 -> the unjudged search adds a pair; at hit rate 0.8 it does not; at 29 judged it does not
}
// gaps.rs / pursuit.rs
#[tokio::test] async fn a_gap_closes_when_a_new_note_answers_its_question() { /* answers_query 0.95 */ }
#[tokio::test] async fn a_similar_note_that_does_not_answer_leaves_the_gap_open() { /* similarity above the old line, yes 0.2 */ }
#[tokio::test] async fn a_decide_gap_closes_the_same_way() {}
#[tokio::test] async fn a_pursuit_counts_as_answered_only_by_a_note_that_answers_it() {}
// web/insights.rs
#[tokio::test] async fn insights_shows_retrieval_by_the_person_and_by_the_model() {}
```

- [ ] **Step 2–4: Run to fail, implement, regenerate `.sqlx/`, run to pass.**

- [ ] **Step 5: `docs/evaluation.md`**: one paragraph on the model's labels, the two agreement figures and when a sweep may use them.

- [ ] **Step 6: Commit**

```bash
git add -A src docs/evaluation.md config.example.toml .sqlx
git commit -m "feat(eval): the decision model labels recorded searches; its labels count only while they agree with yours"
```

---

### Task 4: Tuning

**Files:**
- Modify: `src/config.rs` (`tune_min` 30, `undo_target` 0.05), `src/jobs/tune.rs` (`pass` ~115), `src/core/decide/mod.rs` (spec 1: `settled_at` read through a function), `src/store/decision_log.rs`, `src/web/insights.rs` (`evolve_view` ~958), `src/web/templates/_evolve.html`, `CHANGELOG.md`

**Interfaces:**
- Consumes: `decision_thresholds`, `Core::thresholds`, `close_outcome_windows`, spec 1's `decide_events`.
- Produces:
  ```rust
  pub const STEP: f64 = 0.02;
  pub struct ActRecord { pub acted: usize, pub undone: usize, pub asks_open: usize }
  pub struct AskRecord { pub lapsed_since_move: usize, pub answered_since_move: usize, pub open: usize, pub waiting: usize }
  pub async fn act_record(&self, question_id: &str, era: &str) -> Result<ActRecord>;       // store; acted = done act decisions with a closed window or an outcome
  pub async fn ask_record(&self, question_id: &str, era: &str) -> Result<AskRecord>;       // store
  pub async fn settled_record(&self, model: &str) -> Result<(usize, usize)>;               // store: held `settled` decisions judged right or wrong -> (judged, wrong)
  pub fn next_act_at(current: f64, r: ActRecord, tune_min: usize, undo_target: f64, lo: f64, hi: f64) -> Option<f64>;
  pub fn next_ask_at(current: f64, act_at: f64, r: AskRecord) -> Option<f64>;
  pub async fn tune_questions(core: &Core) -> Result<usize>;                               // moves made; called from tune::pass
  impl Core { pub async fn settled_at(&self) -> f64; }                                     // the tuned value for this model's era, else config
  ```
  `tune_questions`, gated by `evolve.autonomous.acts_on_corpus()`: for each catalogue question that has an applier, `next_act_at` within `[0.80, 0.99]` and `next_ask_at`; then Decide's `settled_at` as question `decide_settled`, era `"{model}/v{bears version}"`, with `(judged, wrong)` as `(acted, undone)` within `[0.70, 0.95]`. Every move is a `decision_thresholds` row whose reason is one sentence (`3 of 41 acted were undone`).

  Insights, in `_evolve.html`, a `<details>` `judgement lines ({{ n }})`: one line per question in force (`pair_relation · act 0.88 · ask 0.62 · moved 2026-12-02: 3 of 41 acted were undone`), and under it the three shares the spec names for Decide: `split and right`, `held and "nothing here"`, `not held and wrong`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn too_few_acted_decisions_move_nothing() { /* 29 */ }
#[test] fn more_than_five_in_a_hundred_undone_raises_the_line_one_step() { /* 0.90 -> 0.92; at 0.99 None */ }
#[test] fn under_half_the_target_with_rows_waiting_lowers_it_one_step() { /* 1 of 60 undone, asks_open 3: 0.90 -> 0.88; at 0.80 None */ }
#[test] fn under_half_the_target_with_no_ask_band_stays() {}
#[test] fn a_lapsed_row_raises_ask_at_and_a_cleared_table_with_rows_waiting_lowers_it() {}
#[test] fn ask_at_stays_under_act_at_and_above_a_half() {}
#[tokio::test] async fn an_undone_merge_counts_against_pair_relation_once_however_many_rows_it_wrote() {}
#[tokio::test] async fn a_decision_whose_window_is_still_open_does_not_count_as_kept() {}
#[tokio::test] async fn a_move_is_recorded_and_is_in_force_at_the_next_decision() {}
#[tokio::test] async fn a_new_model_starts_from_config_again() {}
#[tokio::test] async fn decides_settled_line_rises_when_settled_verdicts_are_judged_wrong() {
    // 30 held settled decisions, 3 judged wrong -> settled_at 0.80 -> 0.82; Core::decide then calls 0.81 "leaning"
}
#[tokio::test] async fn split_at_and_bears_min_are_never_moved() {}
#[tokio::test] async fn nothing_is_tuned_while_the_base_may_not_act() {}
#[tokio::test] async fn insights_lists_the_lines_in_force_and_why_they_moved() {}
```

- [ ] **Step 2–4: Run to fail, implement, regenerate `.sqlx/`, run to pass.**

- [ ] **Step 5: `CHANGELOG.md`**, under `### Added`:

```
- The base looks at more of itself: pairs down to 0.70 similarity are judged for contradictions and replacements, and nothing below the review line is acted on without you. Notes from before are read while the base is quiet. Recorded searches are labelled by the decision model, shown beside your own verdicts on Insights.
- The lines at which the base acts or asks move with what you undo and answer; Insights shows each line and why it moved.
```

- [ ] **Step 6: Every check in `AGENTS.md`, and the spec's "Done when"**

Run the full list from `AGENTS.md` (tests with and without `contained`, both clippy runs, `cargo deny check`, the sqlx prepare with a clean `.sqlx` diff, the JS tests, the ignored browser and layout tests, `shellcheck`, `oc-check .` after deleting `assets/app.css`).
Then tick each line of the spec's "Done when" against a named test: no chat judge (`no_background_judgement_is_made_by_a_chat_model`); action linked to decision and undo sets outcome (`a_sure_replacement_links_…`, `undoing_that_replacement_…`); the cap (`the_cap_makes_an_act_wait_…`, `a_waiting_act_never_becomes_an_ask_row`); the disagreeing swap (`a_ninety_nine_whose_reversed_read_disagrees_…`); no writer (`a_duplicate_with_no_writer_…`); refuse below the line (`a_rescue_that_fails_the_check_is_not_stored`); the decide-queued pair (`a_replacement_that_would_retire_the_note_a_person_confirmed_…`); the replay ran (part 2, Task 4).

- [ ] **Step 7: Commit**

```bash
git add -A src CHANGELOG.md config.example.toml .sqlx
git commit -m "feat(tune): the lines the base acts and asks at move with what is undone and answered"
```
