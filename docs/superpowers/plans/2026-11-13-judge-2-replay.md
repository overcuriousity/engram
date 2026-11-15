# The Questions and the Replay (spec 2, stage 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every built-in question exists as code with its wording, version and state builder, and `engram --replay-decisions` runs them over what the base already decided, so wording and thresholds can be fixed from evidence before anything acts.

**Architecture:** `src/core/judging/catalogue.rs` is the one place a question's id, version, wording and options live. State builders are pure functions beside it, sized against `infer::decide::budget`, dropping context before they ever cut a subject. The replay is a one-shot command like `--recompute-coverage`: it opens one tenant, gathers past verdicts from pairs, links, the journal and the graveyard, asks the decision model directly, and prints agreement and band shares. It logs nothing to `decisions`.

**Tech Stack:** Rust, clap, sqlx, the `Decider` from spec 1.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md`, sections "The questions", "Writer gates", "Relevance labels" and stage 2. Part 2 of 5 on `feat/judge`; part 1 is done first.

## Global Constraints

- Wording is behaviour: a change of instructions or options bumps the question's version. Versions start at 1.
- The subjects of a question are never cut. Context (merge sources, neighbours, cues) is dropped first, oldest first; if the subjects alone do not fit, there is no state.
- The replay writes nothing to the tenant's data: no decision, no action, no pair or link change. (Opening a store migrates it, as every data command does.)
- Option sets stay small; `category` has eight because the spec fixes them.
- Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs.

## The catalogue (first-draft wording, from the spec; version 1)

| id | type | swapped | Instructions | Options |
|---|---|---|---|---|
| `pair_relation` | choice | yes | `How do A and B relate?` | `duplicate`: they state the same thing; nothing in one is missing in the other's subject · `a_replaces_b`: A is newer or corrected and makes B outdated · `b_replaces_a`: B is newer or corrected and makes A outdated · `conflict`: they give different values or instructions for the same thing and neither is clearly newer · `distinct`: different subjects, or the same subject with different content worth keeping apart · `vacuous`: neither states anything a reader could use |
| `link_kind` | choice | yes | `These two notes keep being found by the same searches. How are they related?` | `continues`: one is the next step after the other · `example`: one is an instance of what the other describes · `alternative`: two ways to the same end · `background`: one explains what the other uses · `contradicts`: they say different things about the same subject · `duplicate`: they say the same thing · `unrelated`: no connection worth showing |
| `link_direction` | choice | yes | `Which note comes first: the earlier step, the general description, or the explanation?` | `a_first`: A · `b_first`: B |
| `reap_worth` | noul | – | `Does the retired note state anything a reader could use that the live notes shown do not?` | |
| `category` | choice | no | `What kind of note is this?` | the eight `CATEGORIES`, each with one line: `concept`: an idea and how to think about it · `procedure`: steps to do something · `reference`: facts to look up · `snippet`: a piece of code or a command · `configuration`: settings and their values · `definition`: what a term means · `example`: a worked case · `other`: none of these |
| `pinned` | noul | – | `Does this record a decision or a commitment its writer made?` | |
| `time_bound` | noul | – | `Does this state something that will go out of date: a version, a price, a schedule, a temporary workaround?` | |
| `self_contained` | noul | – | `Can this be understood without the document it came from?` | |
| `asks_reminder` | noul | – | `Does this ask to be reminded of something?` | |
| `has_date` | noul | – | `Does this mention a specific future date or deadline?` | |
| `answerable` | noul | – | `Do these excerpts contain the answer to these questions?` | |
| `complete` | noul | – | `Does the rewrite state everything the originals state?` | |
| `adds_nothing` | noul | – | `Does the rewrite state only what the originals state?` | |
| `answers_query` | noul | – | `Would this note help someone who searched for: «{q}»?` | |

## Decisions this plan makes

- `time_bound` is in the catalogue at version 1 as worded, although the probe says the wording fails. The replay prints its extremes; Task 4 rewords it or drops it.
- Readings and gates have no past verdict to agree with, so the replay prints their distribution and, where the base has a comparison, that: `category` against the stored category; `complete` and `adds_nothing` over past merges and condensations.
- The replay asks at most `--limit` cases per question (default 200, newest first) and says how many it left out.
- Past link verdicts map onto the new kinds coarsely: `related` agrees with `continues`, `example`, `alternative` or `background`; `unrelated` with `unrelated`; a link whose reason starts `same content` with `duplicate`.
- A past pair verdict: `merged_into` set → `duplicate`; a `supersede` journal row → the replacement that names its subject as obsolete; a `discard` row on both → `vacuous`; state `contradiction` → `conflict`; state `no_conflict` decided by the model with no journal row → `distinct`. Pairs decided by `evidence`, and pairs settled with a refusal detail (taken back, lost a value, did not fit), are left out.
- "The person" in the replay: an undone journal row means the person disagreed with the past verdict; a pair decided by `operator` is the person's own verdict.

## Review Focus

1. A pair whose member was buried: its text comes from the graveyard; a member with no text anywhere is skipped and counted. Task 3.
2. A model-written member with ten merge sources: sources are dropped oldest first until the state fits; the members are whole. Task 2.
3. A member that alone exceeds the window: no state, counted as `too long`, no call. Tasks 2 and 3.
4. The endpoint answers `429` while the server is also using it: the replay retries as the client does and finishes. Task 3.
5. `--question nope`: one sentence naming the known ids, exit 2, no call. Task 3.

## File Structure

| File | Responsibility |
|---|---|
| `src/core/judging/catalogue.rs` (create) | every `Asked`, `all()`, `by_id()` |
| `src/core/judging/state.rs` (create) | state builders |
| `src/jobs/replay.rs` (create) | gathering cases, asking, the report |
| `src/main.rs` (modify) | `--replay-decisions`, `--question`, `--limit` |
| `docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md` (modify, Task 4) | "Measured → Replay" |

---

### Task 1: The catalogue

**Files:**
- Create: `src/core/judging/catalogue.rs`

**Interfaces:**
- Consumes: `Asked` (part 1), `Question`, `Kind`, `infer::decide::validate`, `prompt::CATEGORIES`.
- Produces:
  ```rust
  pub fn pair_relation() -> Asked;  pub fn link_kind() -> Asked;  pub fn link_direction() -> Asked;
  pub fn reap_worth() -> Asked;     pub fn category() -> Asked;   pub fn pinned() -> Asked;
  pub fn time_bound() -> Asked;     pub fn self_contained() -> Asked;
  pub fn asks_reminder() -> Asked;  pub fn has_date() -> Asked;   pub fn answerable() -> Asked;
  pub fn complete() -> Asked;       pub fn adds_nothing() -> Asked;
  pub fn answers_query(q: &str) -> Asked;
  pub fn all() -> Vec<Asked>;                       // answers_query with an empty q
  pub fn by_id(id: &str) -> Option<Asked>;
  /// A question's key on the wire is its id.
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn every_question_is_valid_and_keyed_by_its_id() { /* for a in all(): validate(&[a.question]) is Ok; a.question.key == a.id; version == 1 */ }
#[test] fn ids_are_unique_and_fit_a_key() {}
#[test] fn pair_relation_offers_the_six_relations_in_the_specs_order() {
    // names == ["duplicate","a_replaces_b","b_replaces_a","conflict","distinct","vacuous"]; swapped; every description non-empty
}
#[test] fn link_kind_offers_seven_kinds_and_direction_two() {}
#[test] fn category_describes_each_of_the_eight_and_is_not_swapped() { /* names == CATEGORIES */ }
#[test] fn answers_query_quotes_the_search() { /* instructions == "Would this note help someone who searched for: «mount loop»?" */ }
#[test] fn by_id_finds_each_and_refuses_the_unknown() {}
#[test] fn a_swapped_question_with_its_twin_still_fits_one_request() { /* pair_relation + link_kind + link_direction swapped <= 16 */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::judging::catalogue`
Expected: compile error.

- [ ] **Step 3: Implement** from the table above, wording verbatim.

- [ ] **Step 4: Run to verify they pass, then commit**

Run: `cargo test --lib core::judging::catalogue`
```bash
git add src/core/judging
git commit -m "feat(judging): the built-in questions, as code"
```

---

### Task 2: The states

**Files:**
- Create: `src/core/judging/state.rs`

**Interfaces:**
- Consumes: `TokenCounter`, `infer::decide::budget`, `swapped_questions`, `crate::fmt::fmt_day`, `Chunk`.
- Produces:
  ```rust
  pub struct Note<'a> { pub title: Option<&'a str>, pub day: Option<String>, pub text: &'a str }   // day: capture date, when it matters
  /// `tokens` is what the request leaves for the state; each returns None when the subjects alone do not fit.
  pub fn pair(a: &Note, b: &Note, sources_a: &[Note], sources_b: &[Note], counter: &TokenCounter, tokens: usize) -> Option<String>;
  pub fn link(a: &Note, b: &Note, cues: &[String], counter: &TokenCounter, tokens: usize) -> Option<String>;
  pub fn reap(retired: &Note, successor: Option<&Note>, neighbours: &[Note], counter: &TokenCounter, tokens: usize) -> Option<String>;
  pub fn one(note: &Note, counter: &TokenCounter, tokens: usize) -> Option<String>;
  pub fn window(text: &str, counter: &TokenCounter, tokens: usize) -> Option<String>;
  pub fn answerable(questions: &[String], excerpts: &[Note], counter: &TokenCounter, tokens: usize) -> Option<String>;
  pub fn rewrite(originals: &[Note], rewrite: &Note, counter: &TokenCounter, tokens: usize) -> Option<String>;
  /// The state budget for these questions on this decider (twins counted when swapped).
  pub fn room(core: &Core, asked: &[&Asked]) -> usize;
  ```
  Layouts (headings are fixed text; `title` falls back to `untitled`; `(captured YYYY-MM-DD)` only when `day` is set):
  ```
  A: <title> (captured 2026-09-14)
  <text>

  B: <title> (captured 2026-10-02)
  <text>

  Earlier versions merged into A:
  - <title>
  <text>
  ```
  `link`: `A:` and `B:` as above, then `Found together by these searches: q1; q2`. `reap`: `Retired note: …`, `Its replacement: …`, `Live notes:` with `[1] <title>` blocks. `one`: `<title>\n<text>`. `answerable`: `Questions:` one per line, then `Excerpt 1: <title>` blocks. `rewrite`: `Original 1: <title>` … then `Rewrite: <title>`.

  Dropping: `pair` drops sources oldest first across both members (the slices are oldest first), then gives up; `link` drops cues from the end; `reap` drops neighbours from the end, then the successor is kept; `answerable` drops excerpts from the end and needs at least one.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_pair_is_laid_out_with_titles_and_capture_dates() { /* exact string for two short notes without sources */ }
#[test] fn merge_sources_are_dropped_oldest_first_and_members_never() {
    // tokens for members + one source: the newest source of the three survives; both members whole
}
#[test] fn members_that_do_not_fit_alone_give_no_state() { /* None */ }
#[test] fn an_untitled_note_is_called_untitled() {}
#[test] fn a_link_names_the_searches_that_found_both_and_drops_them_first() {}
#[test] fn a_retired_note_is_shown_with_its_replacement_and_numbered_live_notes() {}
#[test] fn neighbours_go_before_the_replacement_does() {}
#[test] fn a_rewrite_follows_its_originals() { /* "Original 1: …" before "Rewrite: …" */ }
#[test] fn excerpts_are_dropped_from_the_end_and_one_must_remain() {}
#[test] fn room_counts_a_swapped_questions_twin() { /* room(pair_relation) < room(reap_worth) on the same decider */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib core::judging::state`
Expected: compile error.

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run to verify they pass, then commit**

```bash
git add src/core/judging
git commit -m "feat(judging): what each question is shown; subjects whole, context first to go"
```

---

### Task 3: `--replay-decisions`

**Files:**
- Create: `src/jobs/replay.rs`
- Modify: `src/main.rs` (args ~15–71; `data_command` ~191; dispatch beside `recompute_coverage` ~419)

**Interfaces:**
- Consumes: Tasks 1 and 2; `core.decider`, `decide_swapped`, `band`, `pick_and_top`, `Bands::of`; `pairs_by_state`, `artifacts_by_ids`, `roots_of`, `graveyard_row`, `recent_actions` / `open_actions`, `links_touching`-style link reads, `artifact_versions`, `judged_pairs` and `search_candidates`.
- Produces:
  ```rust
  // main.rs
  /// Run the decision questions over what this base already decided, print how they agree, change nothing.
  #[arg(long, requires = "user")] replay_decisions: bool,
  /// With --replay-decisions: only this question.
  #[arg(long, value_name = "ID", requires = "replay_decisions")] question: Option<String>,
  /// With --replay-decisions: cases per question.
  #[arg(long, value_name = "N", default_value_t = 200, requires = "replay_decisions")] limit: usize,
  /// With --replay-decisions: print JSON.   (reuse the existing --json only if it is free for server commands; else `--replay-json`)

  // replay.rs
  pub struct Case { pub subjects: (String, Option<String>), pub state: Option<String>, pub past: Option<String>,   // the past verdict as one of the question's picks
      pub person: Option<PersonSaid> }           // Agreed | Disagreed with the past verdict
  pub struct Row { pub pick: String, pub top: f64, pub swapped_agree: Option<bool>, pub band: Band, pub past: Option<String>, pub person: Option<PersonSaid> }
  pub struct QuestionReport {
      pub id: String, pub version: u32, pub asked: usize, pub skipped_too_long: usize, pub skipped_no_text: usize, pub left_out: usize,
      pub agrees_with_past: Option<(usize, usize)>, pub agrees_with_person: Option<(usize, usize)>,
      pub bands: [usize; 3],                     // act, ask, leave at the thresholds in config
      pub by_top: Vec<(f64, usize, usize)>,      // bin floor (0.50, 0.55, … 0.95), cases, of which agreeing with the past
      pub swap_disagreed: usize,
      pub extremes: Vec<(f64, String)>,          // for questions with no past verdict: the 10 highest and 10 lowest, with a title
      pub suggested: Option<Thresholds>,
  }
  pub async fn cases(core: &Core, a: &Asked, limit: usize) -> Result<(Vec<Case>, usize)>;
  pub async fn run(core: &Core, question: Option<&str>, limit: usize) -> Result<Vec<QuestionReport>>;
  pub fn render(reports: &[QuestionReport]) -> String;
  pub fn suggest(by_top: &[(f64, usize, usize)]) -> Option<Thresholds>;
  ```
  `suggest`: `act_at` is the lowest bin floor at or above 0.80 such that the cases at or above it number at least 30 and at least 95 % agree with the past, rounded up to a 0.02 step and capped at 0.99; `ask_at` is the lowest bin floor from which at least 70 % agree, floored at 0.50 and kept at least 0.10 under `act_at`. No such `act_at` → `None`.

  The command opens the tenant as `recompute_coverage` does, but with the tenant's real vector store (`reap_worth` and `answers_query` need neighbours and candidates; `merge_orphans` shows the Qdrant connect), prints an estimate first (`cases × 0.6 s`), then the report. Exit 0; an unknown `--question` or `--user` exits 2 with one sentence.

  Text report, per question:
  ```
  pair_relation v1 · 183 asked · 9 too long · 4 without text · 0 left out
    agrees with the past verdict   151 of 183
    agrees with the person          11 of 14
    swapped reads disagreed         12
    would act 61 · ask 88 · leave 34      (act_at 0.90, ask_at 0.60)
    top     cases  agree
    0.95       40     40
    0.90       21     20
    …
    suggested   act_at 0.86   ask_at 0.62
  ```

- [ ] **Step 1: Write the failing tests** in `src/jobs/replay.rs`, on `test_core()` with a `RuleDecider`:

```rust
#[test] fn the_suggestion_is_the_lowest_line_that_holds_ninety_five_of_a_hundred_over_thirty_cases() {
    // bins built so 0.86 holds and 0.84 does not -> act_at 0.86
}
#[test] fn too_few_cases_suggest_nothing() {}
#[test] fn ask_at_stays_a_tenth_under_act_at() {}
#[tokio::test] async fn a_merged_pair_is_a_duplicate_case_and_a_superseded_one_names_its_direction() {
    // seed via the dedupe tests' helpers: one merge, one supersede of member A, one contradiction, one model-distinct
    // cases(pair_relation): past == ["duplicate", "b_replaces_a", "conflict", "distinct"] for those pairs
}
#[tokio::test] async fn an_undone_action_is_the_person_disagreeing() {}
#[tokio::test] async fn a_pair_closed_by_a_refusal_is_not_a_case() { /* detail TAKEN_BACK, LOSS_REFUSED, did not fit */ }
#[tokio::test] async fn a_buried_member_is_read_from_the_graveyard() {}
#[tokio::test] async fn a_member_with_no_text_anywhere_is_skipped_and_counted() {}
#[tokio::test] async fn a_burial_is_a_no_case_and_a_rescue_a_yes_case_for_reap_worth() {}
#[tokio::test] async fn judged_links_map_onto_the_new_kinds() {}
#[tokio::test] async fn category_is_compared_with_the_stored_one() {}
#[tokio::test] async fn questions_with_no_past_verdict_report_their_extremes() { /* self_contained: agrees_with_past None, extremes.len() <= 20 */ }
#[tokio::test] async fn the_report_counts_agreement_bands_and_swaps() { /* a rule that answers known tops: exact QuestionReport */ }
#[tokio::test] async fn the_limit_takes_the_newest_and_counts_the_rest() {}
#[tokio::test] async fn the_replay_changes_nothing() {
    // row counts of artifacts, artifact_pairs, artifact_links, corpus_actions, decisions, graveyard are equal before and after
}
#[tokio::test] async fn an_unknown_question_is_refused_before_any_call() {}
#[test] fn the_text_report_reads_as_the_plan_draws_it() { /* render of a fixed report equals the block above */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::replay`
Expected: compile error.

- [ ] **Step 3: Implement.** Calls go straight to `core.decider` (no gate: this is a separate process) and are not logged.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test && cargo test --features contained --lib && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/replay.rs src/jobs/mod.rs src/main.rs
git commit -m "feat: --replay-decisions measures the new questions against what the base already decided"
```

---

### Task 4: Run it, and fix wording and thresholds (the owner's gate)

This task needs the owner's base and the owner's decision endpoint. **Parts 3 to 5 do not start until it is done.** An agent without that access stops here and says so.

- [ ] **Step 1: Run the replay on the owner's base**

Run: `engram --config <the server's config> --replay-decisions --user <subject> > replay-1.txt`
Expected: a report per question; note the wall time.

- [ ] **Step 2: Read it with the owner, question by question.** For each: is agreement with the past and with the person good enough to act on (the spec's bar: undone share under 5 %)? Where is it wrong: one option absorbing another, a pick that flips under the swap? Reword only what the report shows to be wrong.

- [ ] **Step 3: Reword and bump.** Each reworded question gets its new text in `catalogue.rs` and `version: 2`; its catalogue test is updated. `time_bound`: reword until its 10 highest are things that go out of date and its 10 lowest are not, or delete it from the catalogue; if deleted, part 4's Task 2 and part 5's Task 4 leave it out, and the spec says so.

- [ ] **Step 4: Run again** until no question changed in the last run.

- [ ] **Step 5: Set the thresholds.** For each question with a suggestion, write it as `[decide.bands.<id>]` in `config.example.toml` and as that question's default in `Bands` (a `const DEFAULTS: &[(&str, f64, f64)]` beside `Bands::of`, used when the file has no override). A question with no suggestion keeps `act_at = 0.99`: it asks, it does not act.

- [ ] **Step 6: Write the result into the spec** under "Measured → Replay": the date, the model, the final report per question, the final wording where it changed, the thresholds chosen and why.

- [ ] **Step 7: Commit**

```bash
git add src/core/judging src/config.rs config.example.toml docs/superpowers/specs/2026-11-13-decide-2-judgement-design.md
git commit -m "feat(judging): wording and thresholds fixed from the replay on a real base"
```
