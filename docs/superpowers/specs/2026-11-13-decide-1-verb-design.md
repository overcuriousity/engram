# Decide: a third verb, and the decision model behind it

Date: 2026-11-13. Spec 1 of 3. Spec 2 (background judgement) and spec 3
(standing questions) build on the role and client defined here.

File and line references come from a read of `522f26aa`; verify before relying.

## Intent

Search returns passages. Ask returns prose. Neither returns a *verdict*: which
of these options do my notes support, and how firmly. Decision models answer
exactly that: they take a situation and typed questions and return a probability
for each allowed answer, with no text, at a fraction of a chat model's cost.

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
- The owner's instance was probed on 2026-11-13; see "Measured". The two
  facts that shape this design: a call costs about 1.1 ms per input token
  (not the vendor's 39 ms), and requests run one at a time. Time is spent on
  state tokens; an extra question on the same state costs about 0.1 s.

## Goals

- A required `[infer.decide]` role and a `Decider` trait that specs 2 and 3
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
- **Advice.** Decide answers what the notes support. Copy and docs do not
  suggest it for questions the base cannot hold ("should I take the job").

## Constraints

- The manifest (`AGENTS.md`), profile everyday: few words on the page, no
  decorative motion, keyboard reachable, phone layout from 390 px.
- The role is required (owner's decision, 2026-11-13: spec 2 moves the judge
  onto the decision model, and the judge is not optional). A config without
  `[infer.decide]` is refused at start with a message that names the block.
  Every door therefore always has Decide; an endpoint that is down is an error
  when asked, and a warning at boot.
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
# The decision model. Required: Decide and background judgement run on it.
[infer.decide]
base_url = "http://localhost:8080/v1"
model = "clef-flash:9b-q8_0-8192"
# The server's real context window.
context_tokens = 8192
timeout_secs = 30
# api_key = ""
# Requests in flight at once; match the server's slots. The owner's has one.
parallel = 1
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
- Validation: 1 to 16 questions (the server took 64), keys
  `[a-z0-9_]{1,30}`, 2 to 8 options, 2 to 10 levels (the server enforces 10),
  unique option names, `instructions` never empty (the server requires them).
  magicshell's 8000-character cap on state is not mirrored; the token budget
  below is the limit.
- A request that would exceed `context_tokens` is refused before it is sent.
  `decide::budget(questions) -> usize` gives the tokens left for `state` after
  the questions and a 256-token margin.
- `HttpDecider`, plus `FakeDecider` (fixed answers) and `ScriptedDecider`
  (queue, records requests) in `infer/fake.rs`.
- Retries follow `infer/retry.rs`; `429` is retried with backoff (the owner's
  proxy answers it to a burst). The server reports an oversized input as
  `500`, which is why the client refuses first. A reply that does not parse is
  an error, never a guess.

### Pacing

Decision calls pass `infer/gate.rs`. One made for a waiting person takes the
interactive lease; a background one takes the background turn, so it never
overlaps a generation, and yields to a lease. Neither observes
`pacing.cooldown_secs`, which exists for generations. Whoever holds the lease
or the turn may have up to `parallel` requests in flight.

### Swapped reads

A `choice` asked with its options reversed can answer differently (measured:
the same pick, but 0.50 one way and 0.76 the other). A *swapped read* puts the
question into the request twice, options forward and reversed, and returns the
mean distribution and whether both picks agreed. One request, about 0.1 s
more. It is a helper on top of `Decider`, used wherever a choice decides
something.

## How a decision is made

Input: the question text `q`, typed questions (default: one `noul` whose
instructions are `q`), an optional caller `situation`, the usual filters.

1. **Retrieve.** `search_with` on `q`, `limit` passages (default 8, max 16),
   reranked as configured, retired artifacts excluded.
2. **Prior.** One call with the situation alone as state (or a fixed "No
   information." when there is none). Kept for display only.
3. **Single reads.** One call per passage, `parallel` at a time, each result
   sent on as it arrives so the rail fills while the person waits. State: the
   situation, then the one excerpt with its title and capture date. Questions:
   the caller's, plus `bears`, a `noul`: "Does this excerpt contain information
   that helps answer the question: «q»?".
4. **Evidence.** Passages with `bears >= decide.bears_min` (0.5), strongest
   first. None: standing `not_held`, no joint read, no answers.
5. **Joint read.** Evidence packed strongest-first into
   `decide.joint_tokens` (2500, never more than the state budget); what does
   not fit is counted as `dropped`. The caller's questions are asked once; a
   `choice` gets a swapped read. This is the verdict.
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

Cost at defaults on the owner's server: prior 0.2 s, eight single reads of a
400-token passage about 0.6 s each, a joint read of up to 2500 tokens about
2.7 s. Roughly 8 s, of which the person watches the evidence arrive for 5.
The defaults are a time budget: tokens read, times 1.1 ms.

```toml
[decide]
joint_tokens = 2500
bears_min = 0.5
settled_at = 0.80
split_at = 0.34
```

## Web UI

`Decide` joins `Search`, `Ask`, `Capture` in the verbs row, shown once the
base holds something (as `_ask_verb.html` does for Ask).
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
Pane                                        Rail · Decided from 6 of 8
  Should I keep the bge-m3 embedder?
                                             1 Reindex notes 09/14     keep
  keep             ████████████░░░  81       2 Embedder comparison     keep
  switch to gemma  ███░░░░░░░░░░░░  19       3 GPU budget            switch
                   ▲ without notes: 55       4 Qdrant sizing           keep
  Settled.                                   …
  [Right] [Wrong] [Nothing here]             2 more did not bear on it
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
- While running, rail rows appear one by one as passages are read, each with
  its pick or "does not bear"; the pane says "Reading 3 of 8", then "Weighing
  6 together", then the result. Transport follows Ask (park, then
  `EventSource`), with events `retrieved`, `read` (one per passage), `done`,
  `error`. `/api/v1/decide/stream` sends the same frames.

Buttons, under a verdict: **Right**, **Wrong**, **Nothing here**, then **Undo**.
Under "Nothing here: the base does not hold this" only **Right** and **Wrong**.
There is no Keep: a decision is a reading of the notes, not a note (owner's
decision, 2026-11-13).

### What a verdict does

A press is the person's word on the question, and the base learns from it. The
*right answer* is the verdict's pick after **Right**, and after **Wrong** the
other answer where there are only two (yes / no, two options, two levels).
With three or more, the bar asks once, "Which is it?", with a button per
remaining answer and "Not sure"; without an answer the right one stays unknown
and only the verdict is stored.

A note *agrees* when its own reading picks the right answer, and *reads
against* it otherwise.

| Press | Now | Kept for spec 2 |
|---|---|---|
| Right | Notes that agree are stamped confirmed (`Core::verify`), as a right answer stamps its citations. | A confirmed example. |
| Wrong, right answer known | Notes that agree with the right answer are stamped confirmed. Nothing is retired. | A corrected example with the right answer. |
| Either, and a note reads against | Each such note, strongest bearing first and at most three, is queued as a pending pair with the strongest note that agrees, for the pair judge to rule on contradiction or replacement. | The pairs reach `pair_relation`; see spec 2. |
| Nothing here | The decision is an open gap. Nothing is confirmed. | The notes that bore are refuted as relevant. |
| Right, on a decision not held | The gap stands. | A confirmed miss of the base. |
| Wrong, on a decision not held | The base does hold it: the gap closes. | A miss of retrieval or of `bears`. |

- A queued pair carries no similarity (zero, as a pair found through a link
  does) and a detail saying it came from a decision. A pair that already
  exists is left as it is.
- **Undo** clears the verdict and the right answer and reopens or closes the
  gap accordingly. Confirmation stamps and queued pairs stay: the stamp is
  what a right answer leaves too, and the pair judge rules on the notes, not
  on the press.
- Nothing here deletes, retires or rewrites a note on a verdict alone.
- Independently of any press, the notes a held verdict was weighed from are
  marked cited when it is recorded, as an answer's citations are.

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
  "limit": 8,
  "tags": [],
  "category": null,
  "explain": false
}
```

- `q` is required: it drives retrieval and the `bears` question.
- `questions` is System One's own shape, so a client that already builds those
  (magicshell) passes them through. Absent: `{"verdict": {"type": "noul",
  "instructions": q}}`. A question without `instructions` uses `q`. The key
  `bears` is reserved. A caller may send at most eight questions.
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
  "read": 8,
  "dropped": 0,
  "model": "ggml-org/Clef-Flash-GGUF:Q8_0",
  "event_id": "…"
}
```

- `not_held`: `answers` is `null`, `evidence` is empty, `prior` and `standing`
  are present, status 200.
- `explain: true` adds `state`, the exact joint-read state, for a bench.
- Follow-up: `POST /decisions/{id}/verdict {"verdict": "right" | "wrong" |
  "nothing_here" | "none", "correct": {key: answer}}`. `correct` is optional
  and read only with `wrong`: an option name, a level index, or `"yes"` /
  `"no"`. The reply is `{"verdict", "correct"}` as stored, with the implied
  right answer filled in.
- Errors: `{"error": "…"}`.
- `docs/api.md` gains the section.

## MCP

`decide { q, options?: [{name, description?}], levels?: [string], situation? }`.
Neither `options` nor `levels`: yes/no. Returns markdown: the weights, the
standing, the numbered evidence with each passage's own pick.

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
  JSON, prior JSON, standing, model, read, dropped, the query vector and its
  model (a gap is found by it), created_at, verdict, correct JSON, judged_at,
  dismissed_at.
- `decide_evidence`: one row for **every** passage read, bearing or not:
  event_id, rank, artifact_id, bears, bearing, joint, n (its place in the
  evidence, null when it did not bear), read JSON. Every decision is thereby a
  set of relevance labels for its question.

A judged decision is a labelled example. Spec 2 reads these tables to tune
the thresholds and to measure `bears`; this spec only writes them.

## Stages

Each ships alone.

0. **Probe.** Done by hand on 2026-11-13; see "Measured". What remains is to
   keep it: a script in `scripts/` (magicshell's `probe.sh` is the model) that
   repeats those measurements against the configured endpoint, so a new server
   or quantization can be checked in a minute.
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
- Eight passages and a 2500-token joint read by default, from the measured
  cost per token: about 8 s a decision.
- A prior call on every decision, for display.
- Swapped reads for every `choice` joint read.
- The reserved question key `bears` and its wording.
- What each verdict does to the base, and that none of it retires a note.
- No chat-model fallback.
- The API speaks System One's question shape rather than engram's own.

## Done when

- A config without `[infer.decide]` is refused at start, and the refusal names
  the block.
- The same question returns the same structure through UI, REST,
  MCP and CLI.
- A question the base holds nothing on returns `not_held` and no answers, with
  a scripted decider that would have answered 0.99.
- A request that cannot fit the context is refused locally; a test proves no
  HTTP call was made.
- The checks in `AGENTS.md` pass, including the browser layout test at 390 px.
- The probe script reproduces "Measured" on the owner's endpoint.

## Measured

On the owner's instance, 2026-11-13: `clef-flash:9b-q8_0-8192`, answering as
`ggml-org/Clef-Flash-GGUF:Q8_0`. Synthetic notes, small samples: indications,
not benchmarks.

**Cost**

| Input tokens | Seconds |
|---|---|
| 146 (near-empty state, one question) | 0.21 |
| 572 | 0.53 |
| 2585 | 2.4 |
| 5025 | 5.1 |
| 7770 | 8.7 |

- About 1.1 ms per input token; a request has about 140 tokens of overhead.
- Questions are cheap once the state is read: 1, 2, 4 questions on a
  2585-token state took 2.45, 2.53, 2.77 s. 64 questions on a short state: 6 s.
- No caching between requests: the same state with a new question costs the
  same again.
- No parallelism: 16 requests of 572 tokens took 11.3 s one at a time, 9.8 s
  four at a time, 10.0 s eight at a time. Sixteen at once drew `429`s.
- The first call after idle took 7 s.
- Identical requests return identical numbers.

**Limits**

- 64 questions and 32 options were accepted. Score levels above 10 are refused
  (`400`). `instructions` is required (`400` without). An empty state is fine.
- 12,345 tokens of input: `500`, "input … is too large to process".

**Behaviour**

- `bears` separates cleanly: 0.94 to 0.97 on passages that answer the question,
  0.02 on unrelated ones, the same in German.
- With no information the model still leans: asked which of three ports a
  service uses, it gave the first-listed option 0.77. With the two relevant
  notes and an unrelated one it gave the newer note's port 0.98. This is why
  nothing bearing means no verdict, and why the prior is shown.
- Option order moved a correct pick's probability between 0.50 and 0.76; it
  did not change a pick in ten pairs.
- German notes with English questions scored within 0.05 of the English ones.
- A fact placed at the start, middle or end of a 4400-token state was found
  equally (0.96, 0.96, 0.97; a false variant 0.01 to 0.04).
