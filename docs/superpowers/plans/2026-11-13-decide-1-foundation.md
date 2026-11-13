# Decide Foundation (spec 1, stages 0 and 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give engram a required `[infer.decide]` role with a `Decider` trait, an HTTP client for System One, fakes, swapped reads and a token budget, plus a probe script; no door uses it yet.

**Architecture:** A new module `src/infer/decide/` holds the types, validation, wire format, budget and swapped-read helper; `HttpDecider` reuses `openai::Endpoint` for auth, timeouts and error classification. `Core` gains `decider: Arc<dyn Decider>`, always present like `embedder`. The request body is written by hand, entry by entry, because option order is meaningful to the model and must not pass through a JSON object model.

**Tech Stack:** Rust (edition as in `Cargo.toml`), `async-trait`, `reqwest`, `serde_json`, `tokio`, `wiremock` (dev). POSIX sh, `curl`, `jq` for the probe. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-13-decide-1-verb-design.md`. Sections "The model engram has to work with", "The foundation", "Measured", and stages 0 and 1. Stages 2 to 4 follow on the same branch: `2026-11-13-decide-2-core-rest.md`, `2026-11-13-decide-3-web.md`, `2026-11-13-decide-4-mcp-cli.md`.

## Global Constraints

- Endpoint: `POST {base_url}/systemone`. Request `{"model", "state", "questions": {key: {"type", "instructions", "criteria"}}}`; types `choice`, `score`, `noul`.
- Validation: 1 to 16 questions; keys `[a-z0-9_]{1,30}`, unique; `choice` 2 to 8 options with unique names; `score` 2 to 10 levels; `instructions` never empty. No character caps: the token budget is the limit.
- A request that would exceed `context_tokens` is refused before it is sent. The margin is 256 tokens.
- A reply that does not parse is an error, never a guess. `429` is retried with backoff.
- The role is required (owner's decision, 2026-11-13). A config without `[infer.decide]` is refused at start with a message naming the block; there is no absent-role behaviour anywhere. An endpoint that is down warns at boot and errors when asked.
- Token counts use `infer::budget::TokenCounter`.
- No new crates. `unsafe` stays denied.
- Before every commit: `cargo test`, `cargo test --features contained --lib`, `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-targets --features contained -- -D warnings`. Task 9 also: `shellcheck scripts/*.sh`.
- Comments follow the codebase: one short line saying why, none saying what.
- All of spec 1 ships as one PR. Work on one branch, `feat/decide`, cut from `docs/decision-model-specs` (master plus these specs and plans, so they travel with the code); this plan is part 1 of 4 on it and opens no PR of its own.

## Decisions this plan makes (the spec left them open)

- The contained build writes its own config and has no in-process decision model: it gets a placeholder `[infer.decide]` at `http://127.0.0.1:9/v1`, as its embed and vector blocks have, so it starts and Decide fails there with an inference error until a device endpoint exists. Raised with the owner; not solved here.
- `[decide]` thresholds (`joint_tokens`, `bears_min`, `settled_at`, `split_at`) are **not** added here; stage 2 consumes them and adds them.
- `budget` takes the counter and the window explicitly: `budget(counter, context_tokens, questions)`. The spec's one-argument form has nowhere to get them from.
- The 256-token margin also covers the server's own template overhead (measured at about 140 tokens); there is no second constant.
- `parallel` is enforced inside `HttpDecider` by a semaphore, so the trait stays at the spec's three methods and callers may fire all their calls at once.
- The gate needs no change: an interactive caller takes `gate.interactive()`, a background one `gate.background_light()` (the turn, no cooldown, yields to a lease). Both exist. Callers take them, as everywhere else; the client does not.
- Local refusals (invalid questions, state too large) are `Error::Validation`. An unparsable reply is `Error::InferenceRejected { role: "decide", .. }`: identical requests return identical numbers, so retrying it is pointless.
- Retry budget for one call: 3 seconds (`DECIDE_RETRY_BUDGET`).
- Names, instructions and levels are trimmed on the wire and compared trimmed, as magicshell does; an empty option description sends the name.
- Swapped reads rename keys on the wire to `q0`, `q0r`, `q1`, … and map them back, so a twin key can never collide with or outgrow a caller's key.

## Review Focus

1. Option names the server sorts differently from the request (`"10"`, `"2"`, `"1"`): probabilities come back in request order. Task 3, fixture `response-choice-numeric-names`.
2. Text with quotes, newlines, `«»` or non-ASCII in instructions, names or state: the body is still valid JSON carrying the same strings. Task 3, `the_body_survives_awkward_text`.
3. A probability that is missing, not a number, or outside 0 to 1, or a pick that is not an option: an error, not an answer. Task 3, fixtures plus `a_probability_outside_zero_to_one_is_refused`.
4. `api_key = ""` (the example's commented default, uncommented): no `Authorization` header is sent. Task 5, `an_empty_key_sends_no_authorization`.
5. A swapped read over three or more options: the reversed twin's probabilities are mapped back by name, not by position. Task 7, `three_options_are_mapped_back_by_name`.

## File Structure

| File | Responsibility |
|---|---|
| `src/config.rs` (modify) | `DecideRole`, loading, the refusal without it, validation, redaction |
| `src/contained.rs`, `tests/notify.rs`, `docs/evaluation.md`, test configs in `src/config.rs` and `src/core/mod.rs` (modify) | every config that is loaded gains the block |
| `config.example.toml` (modify) | the `[infer.decide]` block, live |
| `src/infer/decide/mod.rs` (create) | `Question`, `Kind`, `Answer`, `Answers`, `Decider`, `validate`, `budget`, `check` |
| `src/infer/decide/wire.rs` (create) | `request_json`, `questions_json`, `parse_response` |
| `src/infer/decide/http.rs` (create) | `HttpDecider` |
| `src/infer/decide/swapped.rs` (create) | `decide_swapped`, `SwappedAnswers` |
| `src/infer/openai.rs` (modify) | `Endpoint::new` becomes `pub(crate)`; new `Endpoint::post_text` |
| `src/infer/fake.rs` (modify) | `FakeDecider`, `ScriptedDecider` |
| `src/infer/mod.rs` (modify) | `pub mod decide;` |
| `src/core/mod.rs` (modify) | `Core.decider` |
| `src/main.rs` (modify) | boot log line and probe |
| `tests/fixtures/systemone/` (create) | `request-*.json`, `response-*.json` and `README.md` copied from `../magicshell/fixtures/systemone/` |
| `scripts/probe-decide.sh` (create) | repeats "Measured" against an endpoint |
| `CHANGELOG.md` (modify) | one line under `[Unreleased]` → `### Added` |

---

### Task 1: The required `[infer.decide]` role in config

**Files:**
- Modify: `src/config.rs` (`InferConfig` ~776, `RawInferConfig` ~792, `TryFrom` ~1005, `validate` ~1858, `redacted` ~2039, `test_default` ~2156, `MINIMAL` ~2604 and the other test bodies)
- Modify: `config.example.toml` (after `[infer.ask]`, before the optional roles)
- Modify: every other config that is loaded: `src/contained.rs` (the generated file, ~117), `tests/notify.rs`, the test bodies in `src/core/mod.rs`, and the example in `docs/evaluation.md` if it is a whole config. `grep -rn "\[infer.embed\]" src tests docs config.example.toml deploy scripts install.sh` finds them.

**Interfaces:**
- Produces:
  ```rust
  /// Not tier-based: a decision endpoint is not a chat endpoint.
  #[derive(Debug, Deserialize, Clone)]
  pub struct DecideRole {
      pub base_url: String,
      pub model: String,
      #[serde(default)] pub api_key: Option<String>,
      pub context_tokens: usize,
      #[serde(default = "default_decide_timeout_secs")] pub timeout_secs: u64, // 30
      #[serde(default = "default_decide_parallel")] pub parallel: usize,      // 1
  }
  // InferConfig:    pub decide: DecideRole
  // RawInferConfig: #[serde(default)] decide: Option<DecideRole>, so the refusal is ours to word
  ```
  The block every test config gains (call it `DECIDE_BLOCK` where a constant helps):
  ```toml
  [infer.decide]
  base_url = "http://localhost:8080/v1"
  model = "clef-flash:9b-q8_0-8192"
  context_tokens = 8192
  ```

- [ ] **Step 1: Write the failing tests** in `src/config.rs` `mod tests`. `MINIMAL_WITHOUT_DECIDE` is today's `MINIMAL`; `MINIMAL` becomes it plus the block.

```rust
#[test]
fn a_config_without_a_decision_model_is_refused_by_name() {
    // load MINIMAL_WITHOUT_DECIDE -> Err(e); e.to_string() contains "[infer.decide]" and "config.example.toml"
}

#[test]
fn the_decide_role_loads_with_its_defaults() {
    let d = cfg.infer.decide; // MINIMAL
    assert_eq!(d.base_url, "http://localhost:8080/v1");
    assert_eq!(d.model, "clef-flash:9b-q8_0-8192");
    assert_eq!(d.context_tokens, 8192);
    assert_eq!(d.timeout_secs, 30);
    assert_eq!(d.parallel, 1);
    assert!(d.api_key.is_none());
}

#[test]
fn a_decide_role_without_a_window_is_refused() { /* block without context_tokens -> Err */ }

#[test]
fn a_decide_role_with_no_slot_or_no_room_is_refused() {
    // parallel = 0            -> Err(ConfigError::Invalid(m)), m contains "infer.decide.parallel"
    // context_tokens = 512    -> Err(ConfigError::Invalid(m)), m contains "infer.decide.context_tokens"
}

#[test]
fn redacted_hides_the_decide_key_and_shows_the_role() {
    // MINIMAL + api_key = "s3cret" in the block
    assert!(!dump.contains("s3cret"));
    assert!(dump.contains("clef-flash:9b-q8_0-8192"));
}

#[test]
fn the_example_config_carries_a_live_decide_block() {
    // config.example.toml parses (as the existing example test loads it) and cfg.infer.decide.context_tokens == 8192
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib config::tests`
Expected: compile error, `no field decide`.

- [ ] **Step 3: Implement.** `DecideRole`; the field on both structs; in `TryFrom`, a missing block is

  `[infer.decide] is missing. engram needs a decision model: Decide and background judgement run on it. Copy the block from config.example.toml and point base_url at a System One server.`

  `test_default` gets the role with `base_url = "http://127.0.0.1:9/v1"`, `model = "clef-flash"`, `context_tokens = 8192`, `timeout_secs = 30`, `parallel = 1`, no key. `redacted` hides the key. In `Config::validate`: `parallel == 0` and `context_tokens < 1024` are `ConfigError::Invalid`, each naming the key and the reason in one sentence (1024 because the margin and the questions must leave room for a state).

- [ ] **Step 4: `config.example.toml`.** The block from the spec's "Role" section, live (only `# api_key = ""` stays commented), placed after `[infer.ask]`.

- [ ] **Step 5: Give every loaded config the block**: `MINIMAL` and the other bodies in `src/config.rs`, `src/core/mod.rs`, `tests/notify.rs`, and the generated file in `src/contained.rs` with `base_url = "http://127.0.0.1:9/v1"`.

- [ ] **Step 6: Run to verify they pass**

Run: `cargo test && cargo test --features contained --lib`
Expected: PASS. A test that fails with "[infer.decide] is missing" names a config Step 5 missed.

- [ ] **Step 7: Commit**

```bash
git add -A src tests config.example.toml docs/evaluation.md
git commit -m "feat(config)!: [infer.decide] is a required role"
```

---

### Task 2: Types, the trait and validation

**Files:**
- Create: `src/infer/decide/mod.rs`
- Modify: `src/infer/mod.rs` (add `pub mod decide;` in alphabetical place)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub struct Question { pub key: String, pub kind: Kind, pub instructions: String }
  #[derive(Debug, Clone, PartialEq)]
  pub enum Kind {
      Choice { options: Vec<(String, String)> }, // (name, description); "" description sends the name
      Score { levels: Vec<String> },             // lowest first
      Noul,
  }
  #[derive(Debug, Clone, PartialEq)]
  pub enum Answer {
      Choice { pick: String, probabilities: Vec<f64>, confidence: f64 }, // in option order
      Score { score: f64, probabilities: Vec<f64>, confidence: f64 },    // in level order
      Noul { yes: f64 },
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct Answers {
      pub answers: std::collections::HashMap<String, Answer>,
      pub model: String,      // as the server names itself
      pub input_tokens: u64,
  }
  #[async_trait]
  pub trait Decider: Send + Sync {
      async fn decide(&self, state: &str, questions: &[Question]) -> Result<Answers>;
      fn context_tokens(&self) -> usize;
      fn model(&self) -> &str; // as configured
  }
  pub const MAX_QUESTIONS: usize = 16;
  pub fn validate(questions: &[Question]) -> Result<()>; // Error::Validation
  pub(crate) fn trim(s: &str) -> &str;                   // JavaScript's trim, so magicshell's fixtures hold
  impl Question { pub fn noul(key: &str, instructions: &str) -> Self; }
  ```

- [ ] **Step 1: Write the failing tests** in `mod tests` of `src/infer/decide/mod.rs`. A helper `fn choice(key, names: &[&str]) -> Question` (empty descriptions, instructions `"Which?"`) keeps them short.

```rust
#[test] fn one_noul_is_valid()                      { assert!(validate(&[Question::noul("verdict", "Is it?")]).is_ok()); }
#[test] fn no_question_is_refused()                 { assert!(matches!(validate(&[]), Err(Error::Validation(_)))); }
#[test] fn sixteen_pass_and_seventeen_do_not()      { /* keys q0..q15 ok; q0..q16 Err */ }
#[test] fn keys_are_lower_case_digits_and_underscore() {
    for bad in ["", "Verdict", "a-b", "a b", &"k".repeat(31)] { /* Err */ }
    for good in ["a", "a_1", &"k".repeat(30)] { /* Ok */ }
}
#[test] fn two_questions_cannot_share_a_key()        { /* Err */ }
#[test] fn instructions_of_only_whitespace_are_refused() { /* "  \n" Err */ }
#[test] fn a_choice_takes_two_to_eight_options()     { /* 1 Err, 2 Ok, 8 Ok, 9 Err */ }
#[test] fn option_names_are_unique_after_trimming()  { /* ["keep", " keep "] Err; ["keep", ""] Err */ }
#[test] fn a_score_takes_two_to_ten_levels()         { /* 1 Err, 2 Ok, 10 Ok, 11 Err; a blank level Err */ }
#[test] fn the_refusal_names_the_question()          { /* message contains the offending key */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib infer::decide::tests`
Expected: compile error, module or items not found.

- [ ] **Step 3: Implement** the types, the trait and `validate` in `src/infer/decide/mod.rs`. `trim` is ported from `../magicshell/src/systemone.rs` (`fn trim`, the `trim_matches` over ECMAScript whitespace). Messages start with `decide: ` and are one sentence.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib infer::decide::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer/mod.rs src/infer/decide/mod.rs
git commit -m "feat(infer): Decider, its questions and answers, and their validation"
```

---

### Task 3: The wire format

**Files:**
- Create: `src/infer/decide/wire.rs` (`pub mod wire;` in `decide/mod.rs`)
- Create: `tests/fixtures/systemone/` — copy `request-*.json`, `response-*.json` and `README.md` from `../magicshell/fixtures/systemone/` (not `invalid-*`: magicshell's caps differ). Append one line to the copied README saying where they came from and that engram reads only `run`, `model`, `expected_json`, `body`, `expected`.

**Interfaces:**
- Consumes: `Question`, `Kind`, `Answer`, `Answers`, `trim` from Task 2.
- Produces:
  ```rust
  /// The `questions` object alone, as it appears in the body; what `budget` counts.
  pub fn questions_json(questions: &[Question]) -> String;
  /// Compact; keys in the order model, state, questions; criteria entry by entry.
  pub fn request_json(model: &str, state: &str, questions: &[Question]) -> String;
  /// `body` is the response text. Any shape it does not expect is Error::InferenceRejected { role: "decide", .. }.
  pub fn parse_response(body: &str, questions: &[Question]) -> Result<Answers>;
  ```

- [ ] **Step 1: Write the failing tests.** A test-only `fn run_of(fixture: &serde_json::Value) -> (String, Vec<Question>)` maps magicshell's `run` shape to engram's: `state`; per question `key`, `type`, `text` → `instructions`, `options[].{name, description}`, `levels[].text`. Fixtures are read with `include_str!` or `std::fs` from `concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/systemone")`.

```rust
#[test]
fn every_request_fixture_is_reproduced_byte_for_byte() {
    // for each request-*.json: request_json(fixture["model"], state, &questions) == fixture["expected_json"]
    // and at least 7 fixtures were read
}

#[test]
fn every_response_fixture_reads_as_expected() {
    // for each response-*.json, with body = fixture["body"] serialised (a string body is passed as it is):
    //   expected.error present -> parse_response is Err(Error::InferenceRejected { role: "decide", .. })
    //   else -> Ok(a): a.model == expected.model, a.input_tokens == expected.inputTokens, and per key
    //     choice: pick == expected.choice, probabilities == expected.probabilities, confidence
    //     score:  score, probabilities, confidence
    //     noul:   yes == expected.noul
    // and at least 10 fixtures were read
}

#[test]
fn the_body_survives_awkward_text() {
    let q = Question { key: "v".into(), instructions: "Does «this» say \"yes\"?\nReally?".into(),
        kind: Kind::Choice { options: vec![("ja – klar".into(), "tab\there".into()), ("nein".into(), String::new())] } };
    let v: serde_json::Value = serde_json::from_str(&request_json("m", "Zustand: \"ß\"\n\u{2028}", &[q])).unwrap();
    assert_eq!(v["state"], "Zustand: \"ß\"\n\u{2028}");
    assert_eq!(v["questions"]["v"]["instructions"], "Does «this» say \"yes\"?\nReally?");
    assert_eq!(v["questions"]["v"]["criteria"]["ja – klar"], "tab\there");
    assert_eq!(v["questions"]["v"]["criteria"]["nein"], "nein");
}

#[test]
fn a_probability_outside_zero_to_one_is_refused() {
    // noul 1.2; noul -0.1; a choice probability 1.5; each Err(InferenceRejected)
}

#[test]
fn the_state_is_sent_untrimmed() { /* "  x \n" arrives as "  x \n" */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib infer::decide::wire`
Expected: compile error, functions not found.

- [ ] **Step 3: Implement**, porting `request_json` and `parse_response` from `../magicshell/src/systemone.rs` onto engram's types: strings are written with `serde_json::Value::String(..).to_string()`; choice probabilities are looked up by trimmed option name, score probabilities by `"0"`, `"1"`, …; the pick must be one of the names; every probability, `confidence` and `noul` must be a finite number, and probabilities and `noul` within `0.0..=1.0`. `model` and `usage.input_tokens` default to `""` and `0` when absent.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib infer::decide::wire`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer/decide tests/fixtures/systemone
git commit -m "feat(infer): System One's request and response, checked against magicshell's cases"
```

---

### Task 4: The budget and the refusal

**Files:**
- Modify: `src/infer/decide/mod.rs`

**Interfaces:**
- Consumes: `wire::questions_json`, `crate::infer::budget::TokenCounter` (`count(&str) -> usize`; `TokenCounter::default()` is the chars/3.5 estimator).
- Produces:
  ```rust
  pub const MARGIN_TOKENS: usize = 256;
  /// Tokens left for `state`: the window, less the questions, less the margin. Saturates at 0.
  pub fn budget(counter: &TokenCounter, context_tokens: usize, questions: &[Question]) -> usize;
  /// `validate`, then the fit. Every Decider calls it before anything else.
  pub fn check(counter: &TokenCounter, context_tokens: usize, state: &str, questions: &[Question]) -> Result<()>;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_budget_is_the_window_less_questions_and_margin() {
    let c = TokenCounter::default();
    let qs = [Question::noul("verdict", "Is it?")];
    assert_eq!(budget(&c, 8192, &qs), 8192 - c.count(&wire::questions_json(&qs)) - 256);
}
#[test] fn a_window_smaller_than_its_questions_leaves_nothing() { assert_eq!(budget(&c, 100, &qs), 0); }
#[test]
fn a_state_at_the_budget_fits_and_one_past_it_does_not() {
    // build states by repetition until count == budget and count > budget
    // check(..) is Ok for the first; Err(Error::Validation(m)) for the second,
    // m contains both numbers (the state's tokens and the budget)
}
#[test] fn check_refuses_invalid_questions_first() { /* empty questions, tiny state -> Err(Validation) */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib infer::decide::tests`
Expected: compile error, `budget` not found.

- [ ] **Step 3: Implement `budget` and `check`.** The refusal reads `decide: the state is {n} tokens and {b} fit`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib infer::decide::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer/decide/mod.rs
git commit -m "feat(infer): a decision request that cannot fit is refused before it is sent"
```

---

### Task 5: `HttpDecider`

**Files:**
- Create: `src/infer/decide/http.rs` (`pub mod http;` and `pub use http::HttpDecider;` in `decide/mod.rs`)
- Modify: `src/infer/openai.rs` — `Endpoint::new` (~111) becomes `pub(crate)`; add beside `post_json` (~192):
  ```rust
  /// A body already written out: the caller's key order is the request.
  pub(crate) async fn post_text(&self, path: &str, body: String) -> Result<String>
  ```
  (`Content-Type: application/json`, through `send`, so auth and status classification are the shared ones.)

**Interfaces:**
- Consumes: `DecideRole` (Task 1), `check` (Task 4), `wire::{request_json, parse_response}` (Task 3), `infer::retry::transiently`, `Endpoint::new(base_url, model, api_key: Option<&str>, timeout_secs, role)`.
- Produces:
  ```rust
  pub const DECIDE_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);
  pub struct HttpDecider { /* ep, model, context_tokens, counter: Arc<TokenCounter>, slots: tokio::sync::Semaphore */ }
  impl HttpDecider {
      pub fn new(cfg: &DecideRole) -> Self;                         // estimator counter until told otherwise
      pub fn with_counter(self, counter: Arc<TokenCounter>) -> Self;
  }
  impl Decider for HttpDecider { /* model() is cfg.model */ }
  ```

- [ ] **Step 1: Write the failing tests** in `mod tests` of `http.rs`, with `wiremock` as `openai.rs`'s tests use it. `fn role(server: &MockServer) -> DecideRole` gives `base_url = server.uri()`, `model = "m"`, `context_tokens = 8192`, `timeout_secs = 5`, `parallel = 1`, no key. `NOUL_OK` is the `body` of `response-noul.json` and `wet` its question.

```rust
#[tokio::test] async fn it_posts_the_request_to_systemone() {
    // Mock: POST /systemone, body_string(request_json("m", "", &[wet])) -> 200 NOUL_OK, expect(1)
    // decide("", &[wet]) == Ok; answers["wet"] == Answer::Noul { yes: 0.9784768909885947 }; input_tokens == 140
}
#[tokio::test] async fn a_trailing_slash_on_the_base_url_is_harmless() { /* base_url = uri + "/" ; same as above */ }
#[tokio::test] async fn a_request_that_cannot_fit_never_leaves() {
    // Mock: any request -> 200, expect(0). context_tokens = 1024, state = "word ".repeat(4000)
    // decide(..) is Err(Error::Validation(_))
}
#[tokio::test] async fn invalid_questions_never_leave() { /* expect(0); decide("", &[]) Err(Validation) */ }
#[tokio::test] async fn a_429_is_asked_again() {
    // Mock 429 up_to_n_times(1), then 200 NOUL_OK; decide is Ok; server received 2 requests
}
#[tokio::test] async fn a_500_is_an_inference_error() { /* Err(Error::Inference { role: "decide", .. }) */ }
#[tokio::test] async fn a_reply_that_does_not_parse_is_refused_once() {
    // 200 "not json", expect(1); Err(Error::InferenceRejected { role: "decide", .. })
}
#[tokio::test] async fn the_key_is_sent_as_a_bearer() { /* api_key = Some("k") ; header("authorization", "Bearer k") */ }
#[tokio::test] async fn an_empty_key_sends_no_authorization() {
    // api_key = Some(""); after the call, received_requests()[0].headers has no "authorization"
}
#[tokio::test] async fn one_slot_means_one_request_at_a_time() {
    // 200 NOUL_OK with set_delay(150 ms); two decides joined; elapsed >= 300 ms.
    // With parallel = 2 the same pair finishes in under 290 ms.
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib infer::decide::http`
Expected: compile error, `HttpDecider` not found.

- [ ] **Step 3: Implement.** `new` drops an empty or whitespace `api_key` before handing it to `Endpoint::new` with role `"decide"`. `decide`: `check` → take a slot → `transiently(DECIDE_RETRY_BUDGET, || ep.post_text("systemone", body.clone()))` → `parse_response`. The slot is held across the retries.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib infer::decide::http && cargo test --lib infer::openai`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer/decide src/infer/openai.rs
git commit -m "feat(infer): HttpDecider over POST /systemone"
```

---

### Task 6: `FakeDecider` and `ScriptedDecider`

**Files:**
- Modify: `src/infer/fake.rs`

**Interfaces:**
- Consumes: `Decider`, `Question`, `Kind`, `Answer`, `Answers`, `check`.
- Produces:
  ```rust
  /// The same answer to anything: `yes` to a noul, `yes` on the first option or level, the rest shared equally.
  pub struct FakeDecider { pub yes: f64 }          // Default: 0.9
  impl FakeDecider { pub fn saying(yes: f64) -> Self; }
  // Choice: pick = first name, confidence = yes. Score: score = the expected level index, confidence = yes.
  // model() "fake-decider", context_tokens() 8192, Answers.model "fake-decider", input_tokens 0.

  /// A queue of replies; records what it was asked. An exhausted script is Error::Inference { role: "decide", .. }.
  pub struct ScriptedDecider { /* … */ }
  impl ScriptedDecider {
      pub fn new(replies: Vec<Answers>) -> Self;
      pub fn rejecting(msg: &str) -> Self;                       // Error::InferenceRejected on every call
      pub fn calls(&self) -> usize;                              // calls that passed `check`
      pub fn requests(&self) -> Vec<(String, Vec<Question>)>;    // (state, questions), in arrival order
      pub fn set_context_tokens(&self, n: usize);                // default 8192
  }
  ```
  Both run `check` with `TokenCounter::default()` and their own window first, so a test of a caller meets the same refusals as production.

- [ ] **Step 1: Write the failing tests** in `fake.rs`'s test module (or a new `mod decide_tests` there):

```rust
#[tokio::test] async fn the_fake_answers_every_kind() {
    // noul -> Noul { yes: 0.9 }
    // choice [a, b, c] -> pick "a", probabilities [0.9, 0.05, 0.05] (within 1e-9), confidence 0.9
    // score of 3 levels -> probabilities [0.9, 0.05, 0.05], score 0.15 (within 1e-9)
    // every question's key is in the answers
}
#[tokio::test] async fn the_script_replies_in_order_and_remembers_what_it_was_asked() {
    // two Answers queued; two calls return them in order; calls() == 2; requests()[1].0 == second state
}
#[tokio::test] async fn an_exhausted_script_is_an_error_not_a_guess() { /* Err(Error::Inference { role: "decide", .. }) */ }
#[tokio::test] async fn a_rejecting_script_rejects() { /* Err(Error::InferenceRejected { .. }) */ }
#[tokio::test] async fn the_fakes_refuse_what_the_client_would() {
    // FakeDecider: decide("", &[]) Err(Validation)
    // ScriptedDecider: set_context_tokens(1024), a 4000-word state -> Err(Validation), calls() == 0, the queue untouched
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib infer::fake`
Expected: compile error, `FakeDecider` not found.

- [ ] **Step 3: Implement both**, following `FakeCompleter` and `ScriptedCompleter` in the same file for locking and counters.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib infer::fake`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer/fake.rs
git commit -m "test(infer): a fixed and a scripted Decider"
```

---

### Task 7: Swapped reads

**Files:**
- Create: `src/infer/decide/swapped.rs` (`pub mod swapped;` and `pub use swapped::{SwappedAnswers, decide_swapped};` in `decide/mod.rs`)

**Interfaces:**
- Consumes: `Decider`, `validate`, `MAX_QUESTIONS`, `ScriptedDecider` (tests).
- Produces:
  ```rust
  pub struct SwappedAnswers {
      pub answers: Answers,                                   // under the caller's keys
      pub agree: std::collections::HashMap<String, bool>,     // one entry per choice question
  }
  /// What a swapped read sends for these questions (`q0`, `q0r`, `q1`, …); a caller sizes its state against this.
  pub fn swapped_questions(questions: &[Question]) -> Vec<Question>;
  /// One request. Each choice is asked twice, options forward and reversed; the answer is the mean.
  pub async fn decide_swapped(decider: &dyn Decider, state: &str, questions: &[Question]) -> Result<SwappedAnswers>;
  ```
  Rules: the caller's questions are validated under their own keys first. On the wire question `i` is `q{i}` and a choice's twin `q{i}r`, same instructions, options reversed. More than 16 on the wire is `Error::Validation`. For a choice: `probabilities` = the per-option mean of both reads in the caller's option order (the twin mapped back by name); `pick` = the option with the largest mean, the earlier one on a tie; `confidence` = the mean of both; `agree` = both reads picked the same name. Score and noul pass through. `model` and `input_tokens` are the response's.

- [ ] **Step 1: Write the failing tests.** `fn scripted(wire_answers: &[(&str, Answer)]) -> ScriptedDecider` queues one `Answers`.

```rust
#[tokio::test] async fn the_request_carries_each_choice_twice_reversed() {
    // questions: verdict = choice [keep, switch], sure = noul
    // requests()[0].1 keys in order: q0, q0r, q1; q0 options [keep, switch]; q0r options [switch, keep]; q1 is Noul
    // instructions unchanged; descriptions travel with their names
}
#[tokio::test] async fn the_answer_is_the_mean_of_both_reads() {
    // q0:  pick keep, [0.50, 0.50];  q0r: pick keep, [0.24, 0.76] (in q0r's order: switch, keep)
    // -> verdict: pick "keep", probabilities [0.63, 0.37] (1e-9), agree["verdict"] == true
    // confidence 0.2 and 0.6 -> 0.4
}
#[tokio::test] async fn picks_that_differ_are_reported() {
    // q0 picks keep [0.55, 0.45]; q0r picks switch, in its order [0.70, 0.30]
    // -> probabilities [0.425, 0.575], pick "switch", agree false
}
#[tokio::test] async fn three_options_are_mapped_back_by_name() {
    // options [a, b, c]; q0 [0.6, 0.3, 0.1]; q0r in order (c, b, a) [0.2, 0.3, 0.5]
    // -> [0.55, 0.30, 0.15]
}
#[tokio::test] async fn a_tie_goes_to_the_earlier_option() { /* means [0.5, 0.5] -> pick first */ }
#[tokio::test] async fn noul_and_score_pass_through_unpaired() {
    // sure -> Answer::Noul { yes: 0.8 } under "sure"; agree has no "sure"
}
#[tokio::test] async fn nine_choices_do_not_fit_in_one_request() { /* 9 choices -> 18 on the wire -> Err(Validation), calls() == 0 */ }
#[tokio::test] async fn a_bad_caller_key_is_named_as_the_caller_wrote_it() { /* key "Verdict" -> Err(Validation(m)), m contains "Verdict" */ }
#[tokio::test] async fn a_missing_twin_answer_is_an_error() { /* script answers q0 only -> Err(Error::InferenceRejected { .. }) */ }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib infer::decide::swapped`
Expected: compile error, `decide_swapped` not found.

- [ ] **Step 3: Implement `decide_swapped`** to the rules in Interfaces.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib infer::decide`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/infer/decide
git commit -m "feat(infer): swapped reads, so option order cannot carry a choice"
```

---

### Task 8: `Core` holds the decider; the boot says so

**Files:**
- Modify: `src/core/mod.rs` (field beside `embedder`; `from_config` literal ~400; the test `build` literal ~711; `src/core/test_support` if the test core is built there)
- Modify: `src/main.rs` (`startup_checks`, after the embed probe ~127)
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `HttpDecider::new(&DecideRole).with_counter(Arc<TokenCounter>)`, `Decider`, `FakeDecider`.
- Produces:
  ```rust
  // on Core
  /// Always there: `[infer.decide]` is required.
  pub decider: Arc<dyn Decider>,
  ```
  Test cores carry `Arc::new(FakeDecider::default())`; a test that scripts decisions replaces the field.

- [ ] **Step 1: Write the failing test** in `src/core/mod.rs` tests, built as the test beside `cfg.infer.rerank` (~1016) builds its core from a config:

```rust
#[tokio::test] async fn a_core_decides_with_the_configured_window_and_model() {
    // cfg = Config::test_default(); cfg.infer.decide.context_tokens = 4096; cfg.infer.decide.model = "m".into();
    // core.decider.context_tokens() == 4096; core.decider.model() == "m"
}
#[tokio::test] async fn the_test_core_decides_with_the_fake() {
    // test_core().await.decider.model() == "fake-decider"
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib core::tests::a_core_decides`
Expected: compile error, no field `decider`.

- [ ] **Step 3: Implement.** The field; `decider: Arc::new(HttpDecider::new(&cfg.infer.decide).with_counter(counter.clone()))` in `from_config`; `FakeDecider::default()` in every test literal the compiler names. The contained path (~330–345) swaps nothing in.

- [ ] **Step 4: The boot line** in `startup_checks`, beside embed's:

```rust
let d = &cfg.infer.decide;
engram::infer::openai::probe("decide", &d.base_url, d.api_key.as_deref()).await;
tracing::info!(model = %d.model, context_tokens = d.context_tokens, parallel = d.parallel, "decision model");
```

- [ ] **Step 5: `CHANGELOG.md`**, under `## [Unreleased]`, a new `### Added` section above `### Changed`, and one line at the top of `### Changed`:

```
### Added
- `scripts/probe-decide.sh` measures a decision endpoint.

### Changed
- **`[infer.decide]` is required.** engram needs a decision model (System One, `POST {base_url}/systemone`); a config without the block is refused at start. Copy it from `config.example.toml`.
```

(The later parts add the verb itself to `### Added`.)

- [ ] **Step 6: Run everything**

Run: `cargo test && cargo test --features contained --lib && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features contained -- -D warnings`
Expected: PASS, no warnings. Then `cargo run -- --config config.example.toml --print-config | grep -c DecideRole` prints `1`.

- [ ] **Step 7: Commit**

```bash
git add src/core src/main.rs CHANGELOG.md
git commit -m "feat(core): Core carries the decider"
```

---

### Task 9: The probe script (stage 0)

**Files:**
- Create: `scripts/probe-decide.sh` (executable, `#!/bin/sh`, `set -eu`)

**Interfaces:**
- Consumes: nothing from the crate. Environment: `ENGRAM_DECIDE_URL` (the role's `base_url`), `ENGRAM_DECIDE_MODEL`, optional `ENGRAM_DECIDE_KEY`. Needs `curl` and `jq`.
- Produces: a plain-text report on stdout whose sections match the spec's "Measured".

Behaviour, in this order; each section prints a heading and its rows:

1. **Usage.** A missing URL or model, or a missing `curl`/`jq`, prints one line to stderr and exits `2`. `-h` prints the header comment and exits `0`.
2. **Key handling** as `../magicshell/scripts/probe.sh`: the key goes to `curl` on stdin with `-H @-`; it appears in no argument list and no output. Temp files under `mktemp -d`, removed by a trap.
3. **Cost.** One `noul` question over a synthetic state of 0, 400, 2400, 4800 and 7500 words of a repeated sentence (the sizes are a `STATES` variable). Row: `input_tokens` from `usage`, seconds from `curl -w '%{time_total}'`. A last line gives ms per input token from the largest that answered `200`.
4. **Questions.** 1, 2 and 4 `noul` questions over the 2400-word state: seconds each.
5. **Parallel.** 16 requests of the 400-word state at 1, 4 and 8 at a time (`xargs -P`): wall seconds and the count of each HTTP status.
6. **Repeat.** The same request twice: `identical` or `differs`.
7. **Limits.** A score with 11 levels, a question without `instructions`, and a 12000-word state: the status of each, beside the status "Measured" recorded (`400`, `400`, `500`).
8. **Behaviour.** `bears` on a passage that answers a question and on an unrelated one (the wording from the spec: "Does this excerpt contain information that helps answer the question: «…»?"); a two-option `choice` forward and reversed, printing the pick and its probability both ways.

A request that fails prints its status in the row and the script carries on; the exit code is `0` if every request in sections 3, 4, 6 and 8 answered `200`, else `1`.

- [ ] **Step 1: Write the script** to the behaviour above.

- [ ] **Step 2: Check it statically**

Run: `shellcheck scripts/probe-decide.sh && sh -n scripts/probe-decide.sh`
Expected: no output.

- [ ] **Step 3: Check the refusals**

Run: `env -u ENGRAM_DECIDE_URL -u ENGRAM_DECIDE_MODEL sh scripts/probe-decide.sh; echo $?`
Expected: one line on stderr naming `ENGRAM_DECIDE_URL`, then `2`.

Run: `ENGRAM_DECIDE_URL=http://127.0.0.1:9/v1 ENGRAM_DECIDE_MODEL=m sh scripts/probe-decide.sh; echo $?`
Expected: every section is printed with failed rows, no shell error, then `1`.

- [ ] **Step 4: Run it against the owner's endpoint** (the owner's step if the executor cannot reach it; say so in the handoff rather than skip it silently)

Run: `ENGRAM_DECIDE_URL=… ENGRAM_DECIDE_MODEL=clef-flash:9b-q8_0-8192 sh scripts/probe-decide.sh`
Expected: about 1.1 ms per input token; limits `400`, `400`, `500`; `identical`; `bears` above 0.9 and below 0.1; exit `0`.

- [ ] **Step 5: Commit**

```bash
git add scripts/probe-decide.sh
git commit -m "chore(scripts): probe-decide.sh repeats the spec's measurements against an endpoint"
```

---

## Done when (this plan's share of the spec's list)

- A config without `[infer.decide]` is refused at start, and the refusal names the block.
- A request that cannot fit the context is refused locally; `a_request_that_cannot_fit_never_leaves` proves no HTTP call was made.
- `cargo test`, `cargo test --features contained --lib`, both clippy runs, `cargo deny check` and `shellcheck scripts/*.sh` pass.
- The probe script reproduces "Measured" on the owner's endpoint.
