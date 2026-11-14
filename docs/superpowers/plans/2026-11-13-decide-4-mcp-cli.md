# Decide over MCP and the CLI (spec 1, stage 4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The same decision through the last two doors: an MCP tool `decide` that returns markdown, and `engram -d` that prints bars, a standing and numbered evidence; then the checks that close the branch.

**Architecture:** MCP calls `Core::decide` directly, as its `ask` tool calls `Core::ask`. The CLI is a client of `/api/v1/decide/stream` (text) and `/api/v1/decide` (`--json`), as `-a` is of ask's routes, and saves the evidence ids where `--show` finds them.

**Tech Stack:** `rmcp` (`#[tool]`, `#[tool_router]`), `schemars`, clap, reqwest SSE as `src/cli/ask.rs` reads it.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-1-verb-design.md`, sections "MCP", "CLI" and "Done when". Part 4 of 4 on branch `feat/decide`; parts 1 to 3 are done before this starts.

## Global Constraints

- MCP: `decide { q, options?: [{name, description?}], levels?: [string], situation? }`; neither `options` nor `levels` is yes/no. The tool is always registered.
- CLI: `engram -d QUESTION`, `--option NAME[=DESCRIPTION]` (repeatable), `--level TEXT` (repeatable), `--situation TEXT|-`, `--json`, `--tag`, `--category`. Exit `0` a verdict, `1` on `not_held`, `2` on error.
- Weights are whole numbers out of 100, no percent sign, in every door. The single question's key is `verdict`.
- Standing copy as in the web UI: `Settled.` `Leaning.` `Split.`; not held: `Nothing here: the base does not hold this.`
- Before every commit: `cargo test`, `cargo test --features contained --lib`, both clippy runs with `-D warnings`.

## Decisions this plan makes

- MCP decisions come through `Door::Mcp` and are not recorded, as MCP asks are not.
- `--option` and `--level` exclude each other; all three decide flags require `-d`. `-d -` reads the question from stdin; `--situation -` reads the situation from stdin; both at once is refused.
- CLI bars are 15 cells: `█` and `░`, or `#` and `.` where ask's citations fall back to ASCII.
- CLI text output writes progress (`reading 3 of 8`) to stderr through the `Note` ask uses, and the result to stdout.
- The cross-door check compares numbers, not layout: the same base and decider give the same weights, standing and evidence order through REST, MCP and the CLI.

## Review Focus

1. `--option "a=b=c"`: the name is `a`, the description `b=c`. `--option "=x"` and `--option ""`: refused, exit 2. Task 2.
2. `--option` with `--level`, or either without `-d`: refused by the parser, naming the flags. Task 2.
3. MCP called with both `options` and `levels`, or with one option: a one-sentence refusal as text, no model call. Task 1.
4. The server unreachable or answering 400: one sentence on stderr, exit 2, nothing on stdout. Task 3.
5. An evidence title with `|`, `*` or a newline in MCP markdown: the list stays one item per passage. Task 1.

## File Structure

| File | Responsibility |
|---|---|
| `src/mcp/mod.rs` (modify) | `DecideParams`, the `decide` tool, `format_decision` |
| `src/cli/args.rs` (modify) | `-d` and its flags, `Verb::Decide` |
| `src/cli/decide.rs` (create) | the client: request, stream, rendering, exit code |
| `src/cli/mod.rs`, `src/main.rs` (modify) | dispatch; the `about` line |
| `README.md`, `CHANGELOG.md` (modify) | docs |

---

### Task 1: The MCP tool

**Files:**
- Modify: `src/mcp/mod.rs` (params beside `AskParams` ~408; the tool beside `ask` ~626; the formatter beside `format_answer` ~657; tests ~891)

**Interfaces:**
- Consumes: `Core::decide(&DecideRequest, Door::Mcp)`, `Decision`, `Question`, `Kind`, `pick`, `bars`-like rounding (`(p * 100).round()`), `web::decide::pick_label`.
- Produces:
  ```rust
  #[derive(serde::Deserialize, schemars::JsonSchema)]
  pub struct DecideOption {
      /// The option's name, as it will appear in the answer.
      pub name: String,
      /// What choosing it means. Optional.
      #[serde(default)] pub description: Option<String>,
  }
  #[derive(serde::Deserialize, schemars::JsonSchema)]
  pub struct DecideParams {
      /// The question to decide from the knowledge base.
      pub q: String,
      /// Two to eight options to choose between. Leave out, with `levels`, for yes or no.
      #[serde(default)] pub options: Option<Vec<DecideOption>>,
      /// Two to ten levels of a scale, lowest first.
      #[serde(default)] pub levels: Option<Vec<String>>,
      /// Facts about the present situation that the notes cannot know.
      #[serde(default)] pub situation: Option<String>,
  }
  pub fn format_decision(d: &Decision) -> String;
  ```
  Tool: `#[tool(name = "decide", description = "Decide a typed question from the notes that bear on it: yes or no, one of several options, or a level on a scale. Returns weights, how firmly the notes hold it, and each note's own reading. It answers what the notes support, not what is advisable.")]`. Errors come back as text: `Decide failed: {e}`. Both `options` and `levels` given: `Decide failed: give options or levels, not both`.

  `format_decision`, held:
  ```
  **Should I keep the bge-m3 embedder?**

  - keep: 81
  - switch to gemma: 19

  Settled. Without notes: keep 55, switch to gemma 45.

  ---

  **Evidence**

  1. Reindex notes 09/14 — keep
  2. GPU budget — switch to gemma (against)

  _2 more passages did not bear on it._
  ```
  Not held: the question line, then `Nothing here: the base does not hold this.` and `Without notes: …`. `dropped > 0` adds `_{n} bearing passage(s) did not fit the joint read._`. Titles have newlines turned to spaces and `|`, `*`, `_`, `` ` `` backslash-escaped; an untitled passage is its first 60 characters.

- [ ] **Step 1: Write the failing tests** in `src/mcp/mod.rs` tests. `fn decision() -> Decision` is the spec's example with two evidence rows shown and `read` 4.

```rust
#[test] fn the_mcp_decision_reads_as_weights_standing_and_evidence() { /* equals the block above */ }
#[test] fn a_decision_the_base_does_not_hold_says_so_and_lists_nothing() {
    // contains "Nothing here: the base does not hold this." ; no "**Evidence**"; no "- keep:"
}
#[test] fn no_percent_sign_and_no_likelihood() { /* !contains('%'), !contains("likely") */ }
#[test] fn an_awkward_title_stays_one_list_item() { /* title "a|b *c*\nd" -> one line starting "1. " */ }
#[test] fn yes_or_no_lists_yes_and_no() {}
#[tokio::test] async fn the_decide_tool_is_always_offered() {
    // PkdbTools::over(test_core().await).routes().has_route("decide")
}
#[tokio::test] async fn the_tool_decides_yes_or_no_by_default_and_from_options_when_given() {
    // RuleDecider base from part 2; call the tool fn with q only -> text holds "- yes:" ;
    // with two options -> holds both names in the order given; the recorded requests carry a choice
}
#[tokio::test] async fn options_and_levels_together_are_refused_without_a_call() {
    // text == "Decide failed: give options or levels, not both"; decider.calls() == 0
}
#[tokio::test] async fn one_option_is_refused_in_one_sentence() { /* starts "Decide failed:"; calls() == 0 */ }
#[tokio::test] async fn an_mcp_decision_is_not_recorded() { /* learn.enabled = true; no decide_events row */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib mcp::`
Expected: compile error.

- [ ] **Step 3: Implement** the params, the tool and `format_decision`. The `tools/list` test at ~1630 gains `decide` if it lists names.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib mcp::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/mod.rs
git commit -m "feat(mcp): a decide tool"
```

---

### Task 2: `-d` and its flags

**Files:**
- Modify: `src/cli/args.rs`, `src/main.rs` (the `about` string ~13)

**Interfaces:**
- Produces:
  ```rust
  // CliArgs
  /// Decide one question from the base: yes or no, or from --option or --level.
  #[arg(short = 'd', value_name = "QUESTION", num_args = 1.., conflicts_with_all = ["capture", "search", "ask", "remind", "show", "status"])]
  pub decide: Vec<String>,
  /// An option to choose between, as NAME or NAME=DESCRIPTION. Repeatable.
  #[arg(long = "option", value_name = "NAME[=DESCRIPTION]", requires = "decide", conflicts_with = "levels")]
  pub options: Vec<String>,
  /// A level of a scale, lowest first. Repeatable.
  #[arg(long = "level", value_name = "TEXT", requires = "decide")]
  pub levels: Vec<String>,
  /// What the notes cannot know about now; `-` reads it from stdin.
  #[arg(long, value_name = "TEXT|-", requires = "decide")]
  pub situation: Option<String>,

  // Verb
  Decide { question: String, kind: DecideKind, situation: Option<String> }
  pub enum DecideKind { Noul, Choice(Vec<(String, String)>), Score(Vec<String>) }
  pub fn option(raw: &str) -> Result<(String, String)>;   // split at the first '='; an empty name is Validation
  ```
  The other verbs' `conflicts_with_all` lists gain `"decide"`; `--tag`, `--category` and `--json` do not conflict with it. `verb()` counts `decide` among the named verbs, and both error strings that list the verbs gain `` `-d` `` after `` `-a` ``. The `about` line names `-d` among the client flags.

- [ ] **Step 1: Write the failing tests** in `src/cli/args.rs` tests, with the `OnlyTheClientHalf` parser the existing tests use:

```rust
#[test] fn d_alone_is_yes_or_no() { /* ["-d", "Keep", "it?"] -> Verb::Decide { question: "Keep it?", kind: Noul, situation: None } */ }
#[test] fn options_split_at_the_first_equals() {
    // --option keep --option "switch=Move to Gemma" --option "a=b=c" -> [("keep",""), ("switch","Move to Gemma"), ("a","b=c")]
}
#[test] fn an_option_without_a_name_is_refused() { /* "=x" and "" -> Err(Validation) */ }
#[test] fn levels_make_a_scale_in_the_order_given() {}
#[test] fn options_and_levels_cannot_be_mixed() { /* try_parse_from is Err */ }
#[test] fn the_decide_flags_need_d() { /* ["--option", "a", "-s", "x"] is Err; ["--situation", "x"] alone is Err */ }
#[test] fn d_takes_the_filters_and_json() { /* -d x --tag t --category c --json parses */ }
#[test] fn d_and_another_verb_are_refused() { /* -d x -a y is Err */ }
#[test] fn a_dash_reads_the_question_or_the_situation_from_stdin_but_not_both() {
    // -d - with stdin "Q?" -> question "Q?";  -d Q --situation - with stdin "12 GB" -> situation Some("12 GB")
    // -d - --situation - -> Err(Validation)
}
#[test] fn the_one_verb_message_names_d() { /* the "one verb at a time" text contains "`-d`" */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib cli::args`
Expected: compile error.

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib cli::args && cargo test --lib main`
Expected: PASS, `status_refuses_every_flag_it_does_not_honour` included (give `--status` the new flags in its conflicts if that test demands it).

- [ ] **Step 5: Commit**

```bash
git add src/cli/args.rs src/main.rs
git commit -m "feat(cli): -d and the flags that type its question"
```

---

### Task 3: The CLI client

**Files:**
- Create: `src/cli/decide.rs`
- Modify: `src/cli/mod.rs` (`mod decide;`; the dispatch arm beside ask's ~125)

**Interfaces:**
- Consumes: `Verb::Decide`, `DecideKind` (Task 2); `Endpoint`, `client()`, `frames`, `decode`, the `Note` and `face` helpers `src/cli/ask.rs` uses (make them `pub(crate)` where they are private); `last::save`, `last::worth_remembering`; `stand_in_title`.
- Produces:
  ```rust
  pub fn body(question: &str, kind: &DecideKind, situation: Option<&str>, cli: &CliArgs) -> String;  // the request text, criteria in the order given
  pub fn bar(weight: u8, unicode: bool) -> String;                                                   // 15 cells
  pub fn render(decision: &serde_json::Value, unicode: bool) -> String;
  pub fn evidence_ids(decision: &serde_json::Value) -> Vec<String>;
  pub async fn run(e: &Endpoint, question: &str, kind: &DecideKind, situation: Option<&str>, cli: &CliArgs) -> Result<i32>;
  ```
  `run`: with `--json`, POST `{api}/decide?door=cli`, print the body as it came, exit `1` if `answers` is null else `0`. Otherwise POST `{api}/decide/stream?door=cli`; `retrieved` and `read` update the stderr note (`reading 3 of 8`); `done` prints `render`; `error` prints its message to stderr and returns `2`. A non-success status is `Validation("the question was refused: {status}: {error}")`. The evidence ids are saved for `--show` under the same condition ask's citations are.

  `render`, held (label column padded to the longest label):
  ```
  keep             ████████████░░░  81
  switch to gemma  ███░░░░░░░░░░░░  19
  Settled.

  from:
  ├─ [1] Reindex notes 09/14  keep  <artifact id>
  └─ [2] GPU budget  switch to gemma (against)  <artifact id>
  2 more did not bear on it
  ```
  Not held: `Nothing here: the base does not hold this.` and no bars. Filled cells are `(weight * 15 + 50) / 100`.

- [ ] **Step 1: Write the failing tests** in `src/cli/decide.rs`:

```rust
#[test] fn a_bar_is_fifteen_cells() {
    assert_eq!(bar(81, true), "████████████░░░");
    assert_eq!(bar(19, true), "███░░░░░░░░░░░░");
    assert_eq!(bar(0, true).chars().filter(|c| *c == '█').count(), 0);
    assert_eq!(bar(100, false), "###############");
}
#[test] fn the_body_keeps_the_options_in_the_order_given() {
    // Choice [("switch","Move"), ("keep","")] -> the text contains "\"criteria\":{\"switch\":\"Move\",\"keep\":\"\"}"
    // tags and category from cli are included; Noul sends no questions
}
#[test] fn a_decision_prints_bars_a_standing_and_its_evidence() { /* the block above from the matching JSON */ }
#[test] fn a_decision_the_base_does_not_hold_prints_the_notice_alone() {}
#[test] fn the_evidence_ids_keep_their_ranks() { /* a row with no id keeps an empty string, as citation_ids does */ }
#[tokio::test] async fn a_decision_streams_and_the_stream_terminates() {
    // serve_test_app(); run(..) inside a 30 s timeout -> code 0 or 1
}
#[tokio::test] async fn nothing_held_exits_one() { /* an empty base -> 1 */ }
#[tokio::test] async fn json_prints_the_servers_body() { /* captured stdout parses and has "standing" */ }
#[tokio::test] async fn a_refused_question_exits_two_through_run() {
    // crate::cli::run(Verb::Decide { question: "", .. }, ..) -> 2
}
#[tokio::test] async fn an_unreachable_server_exits_two() { /* Endpoint at 127.0.0.1:9 -> crate::cli::run == 2 */ }
#[tokio::test] async fn the_client_claims_the_cli_door() { /* as search.rs:500: with learn on, the decision is recorded */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib cli::decide`
Expected: compile error.

- [ ] **Step 3: Implement.** The test app's core decides with `FakeDecider`, so a seeded base gives a verdict.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib cli::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli
git commit -m "feat(cli): engram -d prints a decision and what it rests on"
```

---

### Task 4: One structure through every door, the docs, and the branch's checks

**Files:**
- Modify: `src/cli/decide.rs` tests (the cross-door test lives where a served app and a core are both at hand), `README.md`, `CHANGELOG.md`

- [ ] **Step 1: Write the cross-door test**

```rust
#[tokio::test] async fn the_same_question_is_the_same_decision_through_every_door() {
    // serve_test_app() with a seeded base and a RuleDecider (choice keep 0.81 / switch 0.19 on the joint read)
    // REST: POST /api/v1/decide -> weights [81, 19], standing "settled", evidence ids E
    // CLI:  run with --json -> the same weights, standing and E
    // MCP:  PkdbTools::over(core).decide(..) -> text holds "- keep: 81", "- switch: 19", "Settled." and E's titles in E's order
    // Web:  decision_fragment of Core::decide's result -> width="81", width="19", "Settled."
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib cli::decide::tests::the_same_question`
Expected: PASS. A failure is a door that rounds or orders differently; fix the door.

- [ ] **Step 3: `README.md`.** In "What it does", after Ask:

```
- **Decide** — a typed question (yes or no, options, a scale) answered from the
  notes that bear on it, with each note's own reading beside the verdict.
```

In the CLI block, after the `-a` line:

```
engram -d "keep bge-m3?"            # decide: yes or no, or --option / --level
```

and the exit line becomes ``Exit `1` means nothing was found or held.`` The setup section that lists the model roles names the decision model as required, in one line.

- [ ] **Step 4: `CHANGELOG.md`**: the Decide line under `### Added` ends `Also the MCP tool decide and engram -d.`

- [ ] **Step 5: Every check in `AGENTS.md`**

Run, in order:
```bash
cargo test && cargo test --features contained --lib
cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings
cargo deny check
cargo run -- --dev-schema target/sqlx-prepare.db && DATABASE_URL=sqlite://target/sqlx-prepare.db cargo sqlx prepare -- --all-targets --features contained && git diff --exit-code .sqlx
node --test assets/js/*.test.mjs
ENGRAM_CHROME=… cargo test --lib -- --ignored && ENGRAM_CHROME=… cargo test --test browser_ask --test browser_decide -- --ignored
cargo build && sh scripts/test-install.sh && shellcheck install.sh build-lowmem.sh extension/pack.sh scripts/*.sh
rm -f assets/app.css && oc-check .
```
Expected: all pass; `git diff --exit-code .sqlx` is clean. `scripts/test-install.sh` writes or reads a config: if it fails on `[infer.decide] is missing`, the install path needs the block too (`install.sh`, `deploy/`); add it there.

- [ ] **Step 6: The spec's "Done when", one by one.** Tick each against a named test or command: refusal without the block (`a_config_without_a_decision_model_is_refused_by_name`); one structure through the doors (Step 1); `not_held` against a decider that says 0.99 (`nothing_bearing_means_no_verdict_whatever_the_model_would_say`); no HTTP call for an oversized request (`a_request_that_cannot_fit_never_leaves`); `AGENTS.md`'s checks (Step 5) with 390 px (`nothing_runs_off_a_phone`); the probe on the owner's endpoint (part 1, Task 9, Step 4: the owner's, if not yet run).

- [ ] **Step 7: Commit**

```bash
git add -A README.md CHANGELOG.md src
git commit -m "docs: Decide in the README; one decision through every door"
```

Then the branch is ready for superpowers:finishing-a-development-branch: one PR for all four parts.
