# Capture reshape: one mode, a size fork, one judging call

engram's capture has three modes and two classifiers too many. This reshape
unifies `eager` and `earned` into a single behavior decided per capture by
size, makes synthesis the judge of what a small capture *is* (reminder,
journal entry, or plain note), replaces the hand-rolled chunker and the
character estimator with the industry pieces, and removes the judge deck page
whose work the inline eval buttons already do.

Decided with the operator on 2026-09-01, and strengthened the same day: the
mode key dies, the no-model product state dies with it, promotion survives,
the moments store survives, the prototype classifier and every date rule do
not. Every capture is verbatim-first — a small one is synthesized
immediately and its artifacts supersede the passages they cover; the
synthesis call sees the base's nearest artifacts and answers with judgement,
events, links and tags in one pass; and the capture box says which fate a
paste will meet before it is captured.

## 1. One mode

`infer.synthesis` (`off` / `earned` / `eager`) is deleted, and `SynthesisMode`
with it.

- `[infer.synthesize]` becomes **required**. Startup refuses a config without
  it, and refuses a config that still sets `synthesis = ...`, each with a
  message naming this change. No compatibility shim, no value mapping.
- Dead keys: `infer.synthesis`, `infer.segment_tokens` (existed only for
  synthesizer-less configs), `promote.resynthesize_after_unconfirmed`
  (eager-only).
- `README` and `config.example.toml` lose the three-mode story and the
  "zero-inference product state" paragraph. A chat model is part of the
  product now.

Existing bases are unaffected in their data — only their config must change,
and the startup error says exactly how.

## 2. The size fork

Every capture runs **one pipeline**: store the corpus → verbatim passages
via the chunker → embed. The fork decides only *when* synthesis happens,
from arithmetic — no new config key:

- **Small** — the whole text fits one synthesis call's input budget, the
  number `infer::budget::segment_tokens(budget, prompt_overhead)` already
  computes. The synthesis job is armed **immediately at capture** instead of
  waiting for activation. Its artifacts supersede the covered verbatim
  passages through the existing `supersede_covered` mechanics — the same
  code promotion runs, activation first, links second, supersede last.
- **Large** — everything above the line. Zero capture-time inference;
  promotion is unchanged: `promote.rs` still watches decayed activation
  against `[promote].activation_above`, and a window whose passages earn it
  is synthesized then. `segments` rows remain the promotion unit.

This is the `earned` path with the earning waived where one call covers the
whole capture — not a second pipeline. What the ordering buys:

- a capture is **searchable the instant its passages embed**, before the
  model answers;
- a failed synthesis call leaves a fully functional verbatim capture and a
  retryable job, never a lost note;
- the superseded passages stay reachable through lineage, which is the
  "original in front of you" promise kept by mechanics rather than by
  prompt discipline.

A capture near the line lands fine on either side; the threshold needs no
precision beyond what the tokenizer gives it.

## 3. A real tokenizer, with a generic loading path

The chars/3.5 estimator (`src/infer/budget.rs`) stays as the fallback, but
budgets are counted by a real tokenizer where one can be had:

- **Bundled default:** the `tokenizer.json` of the model family the default
  `[infer.tiers.efficient]` example serves — Qwen3.8 as of this writing —
  vendored into `assets/` and loaded via the HF `tokenizers` crate
  (`include_bytes!`, a few MB of binary weight). The spec names the family,
  not a frozen minor version; the vendored file tracks the example config.
- **Generic override:** `tokenizer.json` in the HF format is the de-facto
  standard every open-weights family ships (Qwen, Llama, Gemma, Mistral,
  Phi, DeepSeek). One key covers them all:

  ```toml
  [infer]
  # tokenizer = "/path/to/tokenizer.json"
  # tokenizer = "https://huggingface.co/<repo>/resolve/main/tokenizer.json"
  ```

  Prefix decides: `http://` / `https://` is a link, anything else a path. A
  link is fetched **once** with the existing reqwest client and cached beside
  the store, keyed by a hash of the URL so a changed link re-fetches; every
  later boot reads the cache and touches no network. No cache and the fetch
  fails: log loudly, run on the estimator, try again next boot. A tokenizer
  is an accuracy upgrade, never a reason to refuse startup. Gated HF repos
  are out of scope — download by hand and use a path.
- **Proprietary models** (tiktoken, Anthropic) publish no `tokenizer.json`;
  the estimator plus the existing headroom margins is the honest answer
  there, and no tiktoken machinery is added speculatively.

`TokenCounter` becomes: configured tokenizer if loadable, else bundled
default, else estimator. One instance, used everywhere a count is used today
— the size fork, the chunker, prompt budgets. The headroom margins stay: even
an exact tokenizer is exact only for its own family.

## 4. The industry chunker

`text-splitter` (0.32, with the `markdown` and `tokenizers` features)
replaces `split_into_segments`; `src/infer/split.rs` dies. The nesting is
unchanged — segment-sized windows for the synthesis/promotion unit,
chunk-sized passages within them for the embedder — only the engine is new,
and it sizes by the same `TokenCounter`.

- Start/end line numbers are derived from the byte offsets text-splitter
  returns; the spans the lineage and corpus views read keep working.
- Heading-carry (the governing heading repeated into continuation windows) is
  gone. Accepted cost: the crate's semantic boundaries replace it, and the
  synthesis prompt's context blocks already carry the document opening.

## 5. Embedding-optimal synthesis, with judgement

The synthesizer prompt is revised to target the embedder rather than a
generic reader:

- Artifacts are capped at `effective_chunk_tokens()` — `chunk_tokens`
  (default 384) clamped to the embedder's `max_input_tokens * 0.8` — so every
  artifact embeds whole, no post-split. The cap already flows into the prompt
  as `max_artifact_tokens`; the prompt's wording tightens around it: one idea
  per artifact, self-contained, key terms early, literals verbatim (kept
  unchanged).
- Whether 384 is optimal for embeddinggemma stays a question for the judge
  harness, as the config comment reserves it. This reshape does not move the
  number.

### Neighbor context

The small path's call sees the base. The capture's passages embed before
synthesis runs (§2), so the job retrieves top-k artifacts from *other*
corpora with the just-stored dense vectors — no extra embedding call — and
injects them as a context-only block. `ContextBudget` gains a `neighbors`
allowance beside `opening` and `overlap`. The prompt directs the model to:

- structure the operator's unstructured paste into a high-quality, readable
  artifact — markdown, the literal-verbatim rule unchanged;
- write what is **not yet in the base**: use neighbors to resolve references
  ("the migration issue" → which one) and for continuity, never to restate
  their content — the same rule the existing context-only blocks enforce;
- name the artifacts this capture relates to.

### The reply

On the **small path only**, the reply grows around the artifact list:

```json
{"moment":   {"intent": "remind" | "journal" | "none",
              "when": "2026-09-04T09:00",
              "rule": "FREQ=WEEKLY;BYDAY=MO"},
 "events":   [{"when": "2026-09-12T00:00"}],
 "links":    [{"artifact_id": "...", "reason": "..."}],
 "artifacts": [{..., "tags": ["..."], "pinned": false}]}
```

- **`moment`** — `when` and `rule` optional; `rule` still passes
  `validate_rule` before it is stored. A door that forces intent
  (`metadata.intent = "remind"` from the API or MCP) becomes a hint line in
  the prompt rather than a bypass. This replaces the separate
  `REMIND_SYSTEM` call: reading, splitting and judging are one pass over one
  text.
- **`events`** — dates the note mentions without being a reminder become
  `Kind::Event` rows on the first artifact, `Source::Classified`: the
  day-page presence that dropping `absolute_dates` removed, LLM-read, small
  captures only.
- **`links`** — validated against the artifact ids actually shown in the
  neighbor block (the model can only link to what it saw), then written as
  `LinkState::Related` with the model's reason through one small store fn.
  Dedup and supersession of neighbors stay with the existing relate/dedupe
  machinery — the model proposes no merges.
- **`tags` / `pinned`** — `ProposedArtifact.tags` already exists; `pinned`
  maps to the existing `pinned` tag that `pinned_boost` reads, prompted
  cautiously: only for decision-shaped notes, never as a default.

Promotion's synthesis of a large corpus's window requests none of the new
fields — a manual's window is not a reminder, and its links wait for the
sweeps. Salvage behavior on a malformed reply is kept; a missing or
malformed `moment`, `events`, or `links` never fails artifacts that are
otherwise fine.

### The operator knows before pressing capture

The fate echo runs the real `TokenCounter` against the same
`segment_tokens` budget the fork uses — exact, not a client-side guess —
and swaps a one-line hint into the slot the old intent echo used: this
paste **will be synthesized** into structured artifacts, or it is **large —
stored verbatim** in N windows. The hint is the nudge: it says, before the
fact, that a smaller paste becomes a higher-quality retrievable piece, and
it makes the fork legible instead of silent.

*(Implementation deviation, deliberate: no separate `POST /ui/capture/probe`
endpoint — the echo rides the search response the box already makes on
every keystroke, the same vehicle the old intent echo used, and the
over-long-paste guard branch carries it too, since a whole pasted document
never reaches the search itself.)*

## 6. Moments: the classifier dies, the store survives

Deleted:

- the prototype classifier, cue and weak-cue tables, `absolute_dates`,
  `relative_date`, `Core::prototypes()`, `Core::reminder`, `REMIND_SYSTEM`
  and `parse_remind` — most of `core/moments.rs`;
- `jobs/moments.rs` and the `Moments` job stage. The small path writes the
  moment row directly from the synthesis reply; there is nothing left for a
  post-embed stage to read.

Survives:

- the moments **store**, day page, due band, recurrence arming
  (`Source::Armed`), and everything a person sets (`Source::Set`);
- the `intent_refused` guard — an operator's "no" must outlive re-synthesis
  exactly as it outlives a re-embed today;
- `validate_rule`, the timezone table, and `parse_local`, which the surviving
  write path still needs.

Large corpora produce **no moments at all** — no intent, no extracted event
dates. A manual is not a reminder, and the day page's dates now come only
from small captures and from people.

**Accepted cost, flagged during design:** the capture bar's instant,
model-free "reminder set" echo (`web/ui.rs` reads dates rule-based today)
goes async. The judgement now arrives when the synthesis job lands —
seconds to minutes on local hardware — through the existing confirmation
badge and push. The alternative was keeping a date-rule kernel alive for one
echo, which is the machinery this reshape deletes. The capture-fate probe
(§5) partially compensates: the *fate* echo is instant; only the *intent*
echo waits for the model.

`_box_hint.html`'s example chips lose their source with the prototypes
(`moments::examples_for` reads the classifier's own table); they become
static strings, same UX.

`Source::Classified` rows now mean "the synthesis call judged it". A
classified reminder with no date keeps the current guard: it stays an
ordinary capture rather than an undated nag.

## 7. The judge page dies, the metrics move

- `web/judge.rs` — the deck, its eleven routes, `state::judge_pending` and
  its nav counter — is deleted. Inline labelling at search time (the eval
  buttons on artifacts, `open_event`, the answer-bar verdicts) is untouched
  and becomes the only labeller.
- Moving to the **insights page**, behind the same `CanJudge` gate: the
  compact recall@10 / MRR readout, the forget button, and the tuning-sweep
  apply (`[feedback.tune]`'s recommendation currently applies from the judge
  page).
- The miss list dies. The labelled-pairs store, the metrics, and the tuning
  sweep keep working — they never belonged to the page.

## 8. Data, migration, testing

**No data migration.** Artifacts, passages, segments and every `provenance`
value remain valid; an eager-built base simply continues under the size fork.
Old config fails at startup with a message that says what to change.

**Testing, per module:**

- the size fork on both sides of the threshold, including the empty-text and
  parked cases; a small capture searchable before its synthesis job lands; a
  failed synthesis call leaving the verbatim passages live and the job
  retryable; capture-time supersession through `supersede_covered`;
- the probe endpoint agreeing with the fork's own arithmetic on both sides
  of the line;
- `links` validation: an id not shown in the neighbor block is dropped; a
  shown id lands as `Related` with its reason; `events` rows appear on the
  first artifact; a `pinned` proposal becomes the tag;
- tokenizer loading: path, URL-with-cache, cache hit on second boot, fetch
  failure falling back to the estimator without refusing startup;
- line-number derivation from text-splitter byte offsets against multi-line
  and cut-line inputs;
- synthesis parse with `moment` present, absent, and malformed; the
  forced-intent hint; `validate_rule` rejection leaving the artifacts alive;
- the moments write path: remind with date, remind without date (stays a
  capture), journal on a journalable origin, `intent_refused` honored across
  re-synthesis;
- judge routes answer 404; the insights block renders the readout, forget,
  and tune-apply behind `CanJudge`;
- a fresh-instance end-to-end run (podman qdrant + TEI recipe) capturing one
  small reminder, one small note, and one large document.
