# Decide: a third verb, and the decision model behind it

Date: 2026-11-13. Spec 1 of 3. Spec 2 (background judgement) and spec 3
(standing questions) build on the role and client defined here.

File and line references come from a read of `522f26aa`; verify before relying.

## Intent

Search returns passages. Ask returns prose. Neither returns a *verdict*: which
of these options do my notes support, and how firmly. Decision models answer
exactly that: they take a situation and typed questions and return a probability
for each allowed answer, in tens of milliseconds, with no text.

engram's part is the situation. It has the retrieval; the decision model has
the judgement. Decide puts the two together: the person (or an agent, or
magicshell) asks a typed question, engram finds the passages that bear on it,
and the model answers from them. What comes back shows the verdict, how firmly
it is held, and which notes it rests on.

## The model engram has to work with

- **Clef-flash only** (Cloudflare, Apache 2.0, 9B), served by llama.cpp at
  `POST {base_url}/systemone`. No Clef, no Jev.
- **8192 tokens of context as served.** The limit is the server's, not the
  model's, so it is a configured number and every budget derives from it.
- **Request:** `{"model", "state", "questions": {key: {"type",
  "instructions", "criteria"}}}`. Types: `choice` (`criteria` is an object,
  option name to description), `score` (`criteria` is an ordered array of
  levels), `noul` (yes or no, no criteria).
- **Response:** `answers` keyed the same way. `choice`: `choice`,
  `probabilities` per option, `confidence`. `score`: `score` (expected level),
  `probabilities` per level index, `confidence`. `noul`: one number, the
  probability of yes. Plus `model` and `usage.input_tokens`.
- All questions of one request are answered in one forward pass.
- `../magicshell/src/systemone.rs` and `../magicshell/fixtures/systemone/` are a
  working request builder, response parser and shared cases. Port from them.

Known and unknown:

- Probabilities are calibrated at best in aggregate. `confidence` measures how
  concentrated the distribution is, not whether it is right.
- Clef-flash is weak at many-class classification (vendor benchmark:
  CLINC150 macro-F1 67 against 97 for Clef). Keep option sets small.
- Not documented anywhere read: limits on questions or options per request,
  behaviour on German text, option-order bias, parallel slots in llama.cpp.
  Stage 0 measures them.

## Goals

- An optional `[infer.decide]` role and a `Decider` trait that specs 2 and 3
  reuse unchanged.
- Decide in the web UI beside Search and Ask: one question, answered yes/no,
  from named options, or on a scale.
- The same over `/api/v1/decide`, the MCP tool `decide` and the CLI `-d`.
- Every verdict shows its evidence: which passages it rests on and how each one
  reads on its own.
- A verdict the base cannot support says so and records a gap.

## Non-goals

- Images as state, and Jev's `box` type.
- More than one question in the web UI. The API takes up to eight.
- Saved or recurring questions (spec 3). Background use (spec 2).
- Any change to magicshell. Its use of this API is its own spec; the contract
  here is written so that it can.
- A fallback that answers Decide with a chat model.

## Anti-goals

- **A probability shown as the truth.** The page never says "81 % likely". It
  shows weights and one of four standings, and the evidence is always next to
  the verdict.
- **A verdict from the model's own prior.** If no passage bears on the
  question there is no verdict, whatever the model would say unprompted.
- **Self-confirmation.** A decision kept as a note must never be evidence for a
  later decision.
- **Advice.** Decide answers what the notes support. Copy and docs do not
  suggest it for questions the base cannot hold ("should I take the job").

## Constraints

- The manifest (`AGENTS.md`), profile everyday: few words on the page, no
  decorative motion, keyboard reachable, phone layout from 390 px.
- A role that is not configured is absent, not broken: no Decide button, no
  MCP tool, `404` on the routes, a log line at boot. Same pattern as
  `Core::asks()` and `PkdbTools::routes`.
- Token counts use `infer::budget` with the bundled Qwen tokenizer; Clef-flash
  is Qwen-based.
- Nothing here writes model-generated text.

## Approaches

1. **One call over packed evidence, attribution by leave-one-out.** Simple, but
   8k holds few passages, and attribution costs a full-context call per passage.
2. **Read each passage alone, then the bearing ones together (chosen).** Small
   calls that run in parallel; each passage gets its own answer and a bearing
   score for free, so attribution needs no extra work, and the first read
   doubles as a reranker for the joint read. More calls, each tiny.
3. **Pool per-passage answers arithmetically, no joint read.** Cheapest, but
   two notes that only answer the question together are never seen together.

The owner handed the how to the designer; approach 2 is the designer's pick.

## The foundation

### Role

```toml
# Optional decision model; without it Decide and background judgement are absent.
[infer.decide]
base_url = "http://localhost:8080/v1"
model = "clef-flash:9b-q8_0-8192"
# The server's real context window.
context_tokens = 8192
timeout_secs = 30
# api_key = ""
# Requests in flight at once; match the server's slots.
parallel = 4
```

Not tier-based: a decision endpoint is not a chat endpoint.

### Trait

```rust
pub trait Decider: Send + Sync {
    async fn decide(&self, state: &str, questions: &[Question]) -> Result<Answers>;
    fn context_tokens(&self) -> usize;
    fn model(&self) -> &str;
}
```

- `Question { key, kind: Choice { options: Vec<(name, description)> } |
  Score { levels: Vec<String> } | Noul, instructions }`.
- `Answer::Choice { pick, probabilities, confidence }`, `Answer::Score {
  score, probabilities, confidence }`, `Answer::Noul { yes }`. Probabilities are
  returned in the order the options were given.
- Validation mirrors magicshell: 1 to 8 questions, keys `[a-z0-9_]{1,30}`,
  2 to 8 options, 2 to 10 levels, unique option names.
- A request that would exceed `context_tokens` is refused before it is sent.
  `decide::budget(questions) -> usize` gives the tokens left for `state` after
  the questions and a 256-token margin.
- `HttpDecider`, plus `FakeDecider` (fixed answers) and `ScriptedDecider`
  (queue, records requests) in `infer/fake.rs`.
- Retries follow `infer/retry.rs`. A reply that does not parse is an error,
  never a guess.

### Pacing

Decision calls pass `infer/gate.rs`. One made for a waiting person takes the
interactive lease; a background one yields to it. Neither observes
`pacing.cooldown_secs`, which exists for generations.

### Swapped reads

A `choice` asked with its options reversed can answer differently. A *swapped
read* asks twice, forward and reversed, and returns the mean distribution and
whether both picks agreed. It is a helper on top of `Decider`, used wherever a
choice decides something.

## How a decision is made

Input: the question text `q`, typed questions (default: one `noul` whose
instructions are `q`), an optional caller `situation`, the usual filters.

1. **Retrieve.** `search_with` on `q`, `limit` passages (default 16, max 32),
   reranked as configured, retired artifacts excluded, artifacts whose origin
   is a kept decision excluded.
2. **Prior.** One call with the situation alone as state (or a fixed "No
   information." when there is none). Kept for display only.
3. **Single reads.** One call per passage, `parallel` at a time. State: the
   situation, then the one excerpt with its title and capture date. Questions:
   the caller's, plus `bears`, a `noul`: "Does this excerpt contain information
   that helps answer the question: «q»?".
4. **Evidence.** Passages with `bears >= decide.bears_min` (0.5), strongest
   first. None: standing `not_held`, no joint read, no answers.
5. **Joint read.** Evidence packed strongest-first into the state budget;
   what does not fit is counted as `dropped`. The caller's questions are asked
   once; a `choice` gets a swapped read. This is the verdict.
6. **Standing**, per question, on the joint answer. *Top* is the largest
   probability (for `noul`, the larger of yes and no).
   - `split`: the swapped picks disagree, or evidence passages whose own pick
     differs from the joint pick hold at least `decide.split_at` (0.34) of the
     summed bearing.
   - `settled`: not split and top `>= decide.settled_at` (0.80).
   - `leaning`: otherwise.
   - `not_held`: step 4 found nothing.

State layout for the joint read:

```
Situation:
<situation, when given>

Excerpt 1: <title> (captured 2026-09-14)
<text>

Excerpt 2: …
```

Cost at defaults: 1 prior, 16 single reads, 1 or 2 joint reads. Stage 0 says
what that is in seconds on the owner's server.

```toml
[decide]
bears_min = 0.5
settled_at = 0.80
split_at = 0.34
```

## Web UI

`Decide` joins `Search`, `Ask`, `Capture` in the verbs row, shown only when the
role is configured and the base holds something (as `_ask_verb.html` does).
Pressing it with a question in the box opens the answer row below the box;
yes/no is selected, so a second press (or Enter) runs it.

```
[ Should I keep the bge-m3 embedder?                              ]
  Search   Ask   Decide   Capture
  Answer   (•) yes / no   ( ) options   ( ) scale
           [keep        ] [switch to gemma ] [+]
```

- Options: 2 to 8 names, each with an optional description behind a
  disclosure. Scale: 2 to 10 levels, lowest first.
- The answer row keeps its contents while the box text changes.

Result, in the pane, with the evidence in the rail:

```
Pane                                        Rail · Decided from 6 of 16
  Should I keep the bge-m3 embedder?
                                             1 Reindex notes 09/14     keep
  keep             ████████████░░░  81       2 Embedder comparison     keep
  switch to gemma  ███░░░░░░░░░░░░  19       3 GPU budget            switch
                   ▲ without notes: 55       4 Qdrant sizing           keep
  Settled.                                   …
  [Right] [Wrong] [Nothing here] [Keep]      10 more did not bear on it
```

- One bar per option or level; yes/no draws two. Weights are whole numbers
  out of 100 with no percent sign.
- The prior is a marker on each bar, labelled once.
- Standing in one or two words: `Settled.` `Leaning.` `Split.` and, for
  `not_held`, the existing abstention notice ("Nothing here: the base does not
  hold this") with no bars.
- A rail row is a passage, its own pick, and an "against" badge when the pick
  differs from the verdict. Opening a row shows the artifact as any hit does.
- `Split` lists both sides in the rail, grouped by pick.
- Bars are a new component in `73-components.css`; each has a text equivalent
  for screen readers. No animation.
- Progress while running: "Reading 16 passages…", then the result. Transport
  follows Ask (park, then `EventSource`), with events `retrieved`, `evidence`,
  `done`, `error`.

Buttons:

- **Right / Wrong / Nothing here / Undo** work as under an answer. "Nothing
  here" records a gap. Verdicts are stored against the decision.
- **Keep** captures the decision as a note, composed from a template, never by
  a model, with origin `decide`:

  ```
  Decided 2026-11-13: Should I keep the bge-m3 embedder?
  keep 81 · switch to gemma 19 · settled
  From: Reindex notes 09/14; Embedder comparison; GPU budget
  ```

## REST

`POST /api/v1/decide?door=`

```json
{
  "q": "Should I keep the bge-m3 embedder?",
  "questions": {
    "verdict": {
      "type": "choice",
      "instructions": "Which embedder do the notes favour?",
      "criteria": { "keep": "Stay on bge-m3", "switch": "Move to EmbeddingGemma" }
    }
  },
  "situation": "The GPU has 12 GB.",
  "limit": 16,
  "tags": [],
  "category": null,
  "explain": false
}
```

- `q` is required: it drives retrieval and the `bears` question.
- `questions` is System One's own shape, so a client that already builds those
  (magicshell) passes them through. Absent: `{"verdict": {"type": "noul",
  "instructions": q}}`. A question without `instructions` uses `q`. The key
  `bears` is reserved.
- `situation` is the caller's own state, at most 2000 tokens.

```json
{
  "answers":  { "verdict": { "type": "choice", "choice": "keep",
                "probabilities": { "keep": 0.81, "switch": 0.19 },
                "confidence": 0.62, "swapped_agree": true } },
  "prior":    { "verdict": { "type": "choice", "choice": "keep",
                "probabilities": { "keep": 0.55, "switch": 0.45 },
                "confidence": 0.07 } },
  "standing": { "verdict": "settled" },
  "evidence": [ { "n": 1, "artifact_id": "…", "title": "…", "text": "…",
                  "bears": 0.93, "joint": true,
                  "read": { "verdict": { "type": "choice", "choice": "keep",
                            "probabilities": { "keep": 0.9, "switch": 0.1 },
                            "confidence": 0.8 } } } ],
  "read": 16,
  "dropped": 0,
  "model": "ggml-org/Clef-Flash-GGUF:Q8_0",
  "event_id": "…"
}
```

- `not_held`: `answers` is `null`, `evidence` is empty, `prior` and `standing`
  are present, status 200.
- `explain: true` adds `state`, the exact joint-read state, for a bench.
- Follow-ups mirror Ask: `POST /decisions/{id}/verdict {verdict}` and
  `POST /decisions/{id}/keep`.
- Role not configured: 404. Errors: `{"error": "…"}`.
- `docs/api.md` gains the section.

## MCP

`decide { q, options?: [{name, description?}], levels?: [string], situation? }`.
Neither `options` nor `levels`: yes/no. Returns markdown: the weights, the
standing, the numbered evidence with each passage's own pick. Removed from the
routes when the role is absent.

## CLI

```bash
engram -d "Should I keep the bge-m3 embedder?"            # yes / no
engram -d "Which embedder?" --option keep --option "switch=Move to Gemma"
engram -d "How urgent is the migration?" --level "can wait" --level "this month" --level "now"
```

`--situation TEXT` (or `-` for stdin), `--json`, `--tag`, `--category` as
elsewhere. Text output: one line per option with a bar, the standing, numbered
evidence saved for `--show`. Exit `1` on `not_held`, `2` on error.

## What is stored

Only where an ask would be recorded (`learn` on, the same doors).

- `decide_events`: id, question, questions JSON, situation, filters, answers
  JSON, prior JSON, standing, model, read, dropped, created_at, verdict,
  judged_at, kept_artifact_id.
- `decide_evidence`: event_id, n, artifact_id, bears, read JSON, joint.

A judged decision is a labelled example. Spec 2 reads these tables for
calibration; nothing in this spec does.

## Stages

Each ships alone.

0. **Probe.** A script in `scripts/` (magicshell's `probe.sh` is the model)
   that, against the configured endpoint, measures: latency for a 400-token and
   a 7000-token state; time for 16 requests in parallel; the largest question
   count and option count accepted; the same three questions in German; and how
   often a reversed option order changes the pick over a small fixed set. Its
   output goes into this file under "Measured". Defaults above are adjusted to
   it before stage 2.
1. **Foundation.** Role, config, `Decider`, HTTP client, fakes, swapped reads,
   budget, boot log line, `--print-config`.
2. **Core and REST.** `Core::decide`, storage, `/api/v1/decide` and follow-ups,
   `docs/api.md`.
3. **Web UI.**
4. **MCP and CLI.**

## Left to the builder

- Module layout, the `Door`/origin plumbing, table details and migration.
- The keyboard shortcut for Decide and the phone layout of the answer row.
- Whether the answer row pre-fills two options from a question of the form
  "A or B?". Allowed, not required.
- Whether gaps from Decide reuse `GapKind::Ask` or get their own kind.
- Exact copy, within the manifest's rules.

## Calls made by the designer

- Approach 2, and the three thresholds with their defaults.
- A prior call on every decision, for display.
- Swapped reads for every `choice` joint read.
- The reserved question key `bears` and its wording.
- Kept decisions are excluded from Decide's own retrieval.
- No chat-model fallback.
- The API speaks System One's question shape rather than engram's own.

## Done when

- With `[infer.decide]` unset, nothing in any door mentions Decide and the
  test suite passes unchanged.
- With it set, the same question returns the same structure through UI, REST,
  MCP and CLI.
- A question the base holds nothing on returns `not_held` and no answers, with
  a scripted decider that would have answered 0.99.
- A request that cannot fit the context is refused locally; a test proves no
  HTTP call was made.
- The checks in `AGENTS.md` pass, including the browser layout test at 390 px.
- "Measured" is filled in.

## Measured

Empty until stage 0 has run.
