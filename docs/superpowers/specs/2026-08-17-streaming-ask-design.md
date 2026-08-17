# Ask that thinks in the open

Roadmap: [Ask] items 2 and 3, plus the model tiers the two of them finally need.

> **Streaming ask.** The completion streams to the page — reasoning tokens
> included, when the deep model emits them — so the operator watches the model
> think instead of a spinner. `[n]` in the answer is a link […] And a **capture
> this answer** button […]
>
> **The retrieval loop.** Built on streaming, so each step is visible as it
> happens. Pack to the relevance cliff, not to the window […] Pull the
> neighbours of the top hits […] Let the model say once what it still needs and
> retrieve again before it answers […] And a **literal check on the answer**.

Item 1 of that list, situation vectors, is **cut**. It put a model's guess about
what an artifact answers into the ranking path, and the decision was that
generated text influencing retrieval is the wrong side of the fidelity line even
when the text is never displayed. Nothing below depends on it.

## What it is

Ask today is one embedding, top eight, a greedy pack against the context window,
one blocking completion, and a spinner. Everything it knows how to do well
already exists somewhere else in the codebase and is not wired to it:
`search::cliff` decides where a ranked list stops meaning anything, and
`verify::missing_literals` decides whether written text invented a number. Ask
calls neither.

This spec does four things, in the order they can be verified:

1. Gives the config two named model tiers, so a role can pick a model by what
   the call is worth rather than by carrying its own endpoint.
2. Makes retrieval stop at the cliff, reach one hop sideways, and check what the
   model wrote against what it was shown. No streaming involved; all of it is
   testable through the blocking path that exists today.
3. Turns `Core::ask` into an event producer and streams it to the page.
4. Adds the one bounded extra retrieval round, and the button that lets an
   operator keep an answer.

Phases 1 and 2 move harness numbers on their own. If phase 3 turns out worse
than it looks, the retrieval gains still stand.

Two constraints hold throughout. **An answer cannot carry a literal the excerpts
did not** — that is what phase 2's check enforces, and it is the fidelity thesis
extended from synthesis to generation. **A default moves only after the harness
has run** — which is why the second retrieval round ships off.

---

## 1. Model tiers

### The shape

A **tier** is a named endpoint and its defaults. A **role** is a pointer to a
tier, plus optional overrides, plus whatever knobs belong to that role alone.

```toml
[infer.tiers.efficient]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 16384

[infer.tiers.deep]
base_url = "https://llm.example/v1"
model = "…"
context_tokens = 131072
max_output_tokens = 4096

[infer.synthesize]
tier = "efficient"
output_ratio = 8.0
context_opening_tokens = 200
context_overlap_tokens = 150

[infer.ask]
tier = "deep"
max_output_tokens = 4096      # overrides the tier's
follow_up = false
follow_up_tier = "efficient"
```

A tier carries `base_url`, `model`, `api_key`, `context_tokens`,
`max_output_tokens`, `timeout_secs`, `reasoning_effort`, `ceiling_param`,
`structured_output`. Every one of those becomes `Option<T>` on a role and
overrides the tier when set. Role-only fields — `output_ratio`, the two context
knobs, `follow_up` — stay where they are.

### Which roles get one

Only the chat-completion roles: `synthesize`, `ask`, `vision`. `[infer.embed]`
and `[infer.rerank]` are untouched and keep their inline endpoints. An embedding
endpoint and a cross-encoder are different *shapes* of thing, not cheaper
models; routing them through a tier would be the rename pretending to be an
abstraction it is not.

`synthesize` pointing at `efficient` carries the dedupe judge, the gap namer and
the claim checker with it, since those already run on the synthesize endpoint
under their own response shapes (`HttpCompleter::for_claim_checking`,
`for_gap_naming`).

### Why now, when it was deferred before

The roadmap says the rename lands with "whichever item below needs the second
tier first", and until this spec nothing did: ask already ran on the larger
model via `AskRole`, so naming that arrangement bought nothing.

Section 4's follow-up call changes that. "What do I still need?" is a cheap
classification and belongs on the efficient model, while the answer it feeds
belongs on the deep one. Without tiers both are stuck on `AskRole`'s single
endpoint, and the choice cannot be expressed at all. That is a real capability,
not a tidy-up, and it is the first one.

### Migration

An existing `config.toml` has `base_url` and `model` inline under
`[infer.synthesize]`. `Config::normalize` gains a shim: a role carrying an
inline endpoint synthesises an anonymous tier from its own fields, and logs a
warning at startup naming the exact block to write instead.

Not a hard failure, and the reason is already written down in this codebase.
`SynthesizeRole::cooldown_secs` is retired but deliberately still parsed,
because "unknown keys are ignored, which is right for forward compatibility and
wrong for a setting someone chose on purpose." Making `tier` a required field
would do precisely that to the five keys above it: `base_url`, `model`,
`api_key`, `context_tokens`, `max_output_tokens` would all become unknown keys,
silently ignored, behind a `missing field tier` error that names none of them.

The shim is deletable once no config needs it. It is a branch in one function,
not a permanent second code path.

### What must not change

Phase 1 is a refactor and nothing else. Every existing test passes untouched,
and a config written against the old shape produces a byte-identical
`HttpCompleter` to the one it produces today. A test asserts exactly that:
the old and new spellings of the same endpoint resolve to the same effective
role settings.

---

## 2. Retrieval: the cliff, one hop sideways, and the literal check

`src/core/ask.rs` is 734 lines and roughly doubles here. It becomes a module:

```
src/core/ask/mod.rs        the producer and its orchestration
src/core/ask/retrieve.rs   candidate assembly, cliff packing
src/core/ask/check.rs      the literal check over the answer
src/core/ask/stream.rs     the event type and the SSE encoding (phase 3)
```

### Pack to the cliff, not to the window

Today `Core::ask` packs excerpts with `pack_by_budget`, which fills the context
window highest score first and stops when the next one does not fit. That is a
bound on cost, not on relevance: it will happily hand the model eight excerpts
when the fourth was already noise.

`search::cliff(&scores)` returns how many hits sit above the point where the
ranked list falls off. Ask cuts at:

```
packed = min(cliff.unwrap_or(hits.len()), pack_by_budget(...))
```

The budget bound stays and stays second. The cliff decides what is *worth*
showing; the window decides what *fits*, and the window still wins, because an
excerpt that does not fit cannot be sent whatever its relevance.

`AskResponse::dropped` keeps its present meaning — retrieved, not shown — so a
missing citation stays visible on the page whichever bound removed it. The page
gains no new badge; `dropped` already says what it needs to.

Where there is no cliff (`None` — fewer than three hits, or no single step
stands out), behaviour is exactly today's. That is deliberate: a list with no
cliff is a list with nothing to conclude from, and inventing a cut there would
be worse than the greedy pack.

### One hop sideways

The answer is often in the artifact next to the one that matched. After the
ranked search, for the **top 3 hits above the cliff** — or, when there is no
cliff, the top 3 hits outright, since "no cliff" means no basis for treating any
part of the list as the reliable part:

- **Adjacent ordinals.** A new `Store::adjacent_artifacts(corpus_id, ordinal)`
  returns the artifacts at `ordinal - 1` and `ordinal + 1` in the same corpus.
  `ordinal` already exists on `artifacts` and is already a continuous sequence
  per corpus.
- **One-hop associations.** The existing `Store::links_from` gives the Hebbian
  neighbours already learned from co-retrieval.

Candidates are deduped by `artifact_id` against the ranked hits and against each
other, capped at **6 total**, and **appended after the ranked hits**. They are
never interleaved and never scored.

That ordering is the whole safety property. A neighbour has no score comparable
to a retrieved hit — it was not retrieved, it was reached — so letting one into
the score list would corrupt the cliff computation that just ran. Appending
after keeps the cliff honest, keeps the ranked order intact, and makes a
neighbour the first thing the budget drops, which is right: it is the most
speculative excerpt in the prompt.

This is most of `[Retrieval]`'s **"Continues in"** as a side effect. The store
method and the above-cliff-adjacency rule are the same thing that feature needs;
only the presentation differs.

### The literal check on the answer

`verify::missing_literals(artifact_text, caveats, segment_text)` extracts
commands, paths, numbers, registry keys and error strings from written text and
returns those absent from the source. `jobs/window.rs:406` already runs it over
every synthesised artifact against the window it came from.

Ask runs the same function over the answer against the excerpts it was shown:

```rust
let unsupported = verify::missing_literals(&answer, &[], &excerpts_joined);
```

`AskResponse` gains `unsupported: Vec<String>`. In the rendered answer each
occurrence is wrapped in `<mark class="unsupported">`, and a badge under the
answer names them.

Marking happens **inside code fences too**. A fabricated command is exactly the
case this exists for, and exempting the place literals actually live would make
the check decorative.

No inference. This is a string operation over text that has already been
generated, and it costs nothing.

**A risk worth stating rather than discovering.** `extract_literals` was tuned
on synthesised artifacts — dense, atomic, mostly literal. An answer is prose,
and the extractor may over-fire on it: a version number the model wrote in a
sentence of explanation is not the same kind of claim as one inside a command.
If the false-positive rate on real answers is bad, the fallback is to mark only
within code spans and fences, and to say so in the spec's own follow-up rather
than quietly loosening the extractor — because loosening it would weaken the
synthesis check that shares it.

---

## 3. Streaming

### `Core::ask` becomes a collector over an event producer

```rust
pub enum AskEvent {
    /// A retrieval round finished. `round` is 1 or 2.
    Retrieved { round: u8, shown: usize, dropped: usize, cliff_at: Option<usize> },
    /// What the model said it still needed. Round 2 only, section 4.
    Needs(String),
    /// The excerpts the model will see, once, after the final retrieval.
    Citations(Vec<SearchResult>),
    /// A reasoning token, when the endpoint emits them.
    Reasoning(String),
    /// A token of the answer.
    Token(String),
    /// Terminal. Carries the same value the blocking path returns.
    Done(Box<AskResponse>),
}

impl Core {
    pub fn ask_events(&self, req: &AskRequest, origin: impl Into<Origin>)
        -> impl Stream<Item = Result<AskEvent>> + 'static;

    pub async fn ask(&self, req: &AskRequest, origin: impl Into<Origin>)
        -> Result<AskResponse>;   // drains ask_events, returns Done
}
```

`ask` becoming a collector is the point. `/api/v1/ask` and the MCP `ask` tool
cannot stream and are not asked to; they keep their present signatures and
behaviour, and there is exactly one implementation of what asking means. An
error arrives as an `Err` item and terminates the stream.

`record_ask` stays inside the producer, immediately before `Done`, so an ask is
recorded exactly once whichever door it came through — and so the harness sees
streamed and blocking asks identically.

The producer holds the interactive lane (`gate.interactive()`) for its whole
life, as `Core::ask` does today, and must be `'static`: an SSE response outlives
the handler that created it, so the stream owns a clone of `Core` rather than
borrowing it.

New dependencies: `async-stream`, for the producer, and reqwest's `stream`
feature, for reading the endpoint's response as it arrives. Nothing else — the
`mpsc` channel the trait method below takes is tokio's, which is already a
dependency.

### Streaming reaches the endpoint through a defaulted trait method

```rust
pub enum Delta { Token(String), Reasoning(String) }

// on trait Completer
async fn answer_streaming(
    &self, system: &str, user: &str, ceiling: usize,
    sink: mpsc::Sender<Delta>,
) -> Result<Completion> {
    let c = self.answer(system, user, ceiling).await?;
    let _ = sink.send(Delta::Token(c.text.clone())).await;
    Ok(c)
}
```

The default implementation is what keeps the blast radius small: `FakeCompleter`
and every other implementor keep working with no change, and every existing ask
test passes untouched. Only `HttpCompleter` overrides it.

The override parses the endpoint's `text/event-stream`: `data:` lines, `[DONE]`
sentinel, line-buffered across chunk boundaries because a JSON object can be
split across two TCP reads. Reasoning tokens are read from **both**
`reasoning_content` and `reasoning` delta fields — endpoints disagree, and
llama.cpp, vLLM and the hosted APIs do not agree on which. Truncation still
comes from `finish_reason == "length"`, now on the final chunk instead of the
whole response.

### The page

The ask page becomes JS-driven. This is a deliberate character change for a
server-rendered htmx app and is recorded as such: the non-streaming fallback is
**not** kept, so `/ui/ask` requires JavaScript from here on. The API and MCP
doors remain fully functional without it.

`EventSource` is GET-only, and a GET that runs a model call and writes an
`ask_events` row is a mutating GET — the kind history and prefetchers replay. So
the flow is two requests:

- `POST /ui/ask` parks the `AskRequest` in a small one-shot map on `AppState`
  (60-second TTL, removed on consumption) and returns its id.
- `GET /ui/ask/{id}/stream` consumes the id and streams the events.

No schema change, no mutating GET carrying a query, and an id that cannot be
replayed.

**Rendering stays on the server.** Tokens stream into a plain live region —
unstyled text, exactly what the model is emitting, reasoning in a dimmed block
above the answer. On `Done` the server sends the finished HTML fragment:
markdown through the existing `web::markdown`, `[n]` linkified, unsupported
literals marked. The client swaps it in. Nothing renders markdown twice and
nothing renders it in JavaScript.

### Citations and the rail

The ask page gains a rail of cited excerpts, reusing the search rail's CSS and
`_results.html` conventions. In the final HTML each `[n]` becomes
`<a class="cite" href="#cite-n">`; clicking scrolls its rail item into view and
highlights it, hovering shows the excerpt.

`Retrieved` and `Citations` arrive before the first token, so the rail is
populated and readable while the model is still writing.

---

## 4. The second round, and keeping an answer

### One bounded extra retrieval

Config `[infer.ask] follow_up = false`. **Off by default**, because the roadmap's
rule is that a default moves only after the harness has run, and this one costs
a call.

When on, after the first round is packed and before the answer is written: one
structured call on `follow_up_tier` (default `efficient`) returning

```json
{"need": "one short search query" }   or   {"need": null}
```

`null` — the model has enough — skips straight to the answer. Otherwise the
query runs through the same retrieval path as round one, cliff and neighbours
included, results are merged deduped into the candidate list, and the whole set
is re-packed.

Exactly one extra round. Not a loop: there is no condition under which a third
retrieval happens, because "let the model say once what it still needs" is the
bounded version of a mechanism whose unbounded version is an agent, and an agent
is not what this is.

`AskEvent::Needs` carries the query to the page, so the operator sees what the
model went looking for as it happens. That visibility is the reason this piece
comes after streaming rather than before it.

The harness decides whether it stays. `evaluate_ask` already measures citation
recall, abstention accuracy and faithfulness; a run with `follow_up` on and off
over the same `questions.json` is the whole argument.

### Capture this answer

A button on a finished answer. `POST /ui/ask/{id}/capture` redirects to
`/ui/capture` **prefilled** with the answer text and a provenance note carrying
the question and the cited artifact ids. The capture page gains prefill support;
it has none today.

It is prefilled, not saved. The operator edits and presses save, and that is the
line the roadmap draws: this is a person pasting something the model wrote, with
the trace recording that it was model-written and from what. It is not the
system writing memory to itself. Synthesis then treats it like any other paste.

The corpus is created with kind `ask`.

---

## Testing

**Phase 1.** Old and new config spellings of the same endpoint resolve to
identical effective role settings. The shim logs. Every existing test passes
with no edit — that is the phase's acceptance criterion.

**Phase 2.**
- A list with a cliff packs to it; a list without one packs exactly as today.
- The budget still wins when the cliff would exceed the window.
- Neighbours are appended, never interleaved, and never enter the cliff scores.
- The neighbour cap holds when a hit has many links.
- `missing_literals` over an answer flags a fabricated command and does not flag
  one present in an excerpt.

**Phase 3.**
- Event ordering: `Retrieved` and `Citations` precede the first `Token`; `Done`
  is last and terminal.
- The default `answer_streaming` emits one delta, so `FakeCompleter` streams.
- **Equivalence:** for one question, the blocking collector's `AskResponse` and
  the streamed `Done` payload are equal. This is the test that keeps the two
  doors honest as the code changes.
- SSE line-buffering across a split JSON object.
- The one-shot id cannot be consumed twice.

**Phase 4.**
- `follow_up = false` makes no extra call at all — asserted on a counting fake,
  not inferred.
- `{"need": null}` skips the second retrieval.
- Capture prefills and does not save.

**Harness.** `evaluate_ask` gains an unsupported-literal count, which is what
makes phase 2's check a number rather than a badge.

---

## What is deliberately not here

**Situation vectors** — cut, above.

**A retrieval loop with more than one extra round.** Bounded means bounded. The
cost of the mechanism is one call, stated up front, and a mechanism that decides
its own budget is a different design that would need its own argument.

**LLM excerpt compression before answering** — already cut on the roadmap. One
more call to shave tokens off the next one; the cliff now does the same for
free, which is exactly the reason it was cut.

**A non-streaming fallback for `/ui/ask`.** Chosen against deliberately. The
cost is that the page needs JS; the benefit is one render path in the UI, and
the API and MCP doors are the JS-free way in.

**Tiers for embed and rerank.** Different shapes, not cheaper models.
