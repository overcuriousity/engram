# Capture reshape: one mode, a size fork, one judging call

engram's capture has three modes and two classifiers too many. This reshape
unifies `eager` and `earned` into a single behavior decided per capture by
size, makes synthesis the judge of what a small capture *is* (reminder,
journal entry, or plain note), replaces the hand-rolled chunker and the
character estimator with the industry pieces, and removes the judge deck page
whose work the inline eval buttons already do.

Decided with the operator on 2026-09-01. The mode key dies, the no-model
product state dies with it, promotion survives, the moments store survives,
the prototype classifier and every date rule do not.

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

After dedup/parking, one decision per corpus, derived from arithmetic — no
new config key:

- **Small** — the whole text fits one synthesis call's input budget, the
  number `infer::budget::segment_tokens(budget, prompt_overhead)` already
  computes. One synthesis call at capture. The artifacts it writes are the
  index; the verbatim corpus is stored and served back as always.
- **Large** — everything above the line. Verbatim passages via the chunker,
  embedded, zero capture-time inference. Promotion is unchanged: `promote.rs`
  still watches decayed activation against `[promote].activation_above`, and
  a window whose passages earn it is synthesized then. `segments` rows remain
  the promotion unit.

This is the `earned` path with one carve-out: a capture small enough to
synthesize in one call is synthesized now instead of waiting to earn it. A
capture near the line lands fine on either side; the threshold needs no
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

On the **small path only**, the reply gains one top-level field:

```json
{"moment": {"intent": "remind" | "journal" | "none",
            "when": "2026-09-04T09:00",
            "rule": "FREQ=WEEKLY;BYDAY=MO"},
 "artifacts": [ ... ]}
```

`when` and `rule` optional; `rule` still passes `validate_rule` before it is
stored. A door that forces intent (`metadata.intent = "remind"` from the API
or MCP) becomes a hint line in the prompt rather than a bypass. This one call
replaces the separate `REMIND_SYSTEM` call: reading, splitting and judging
are the same pass over the same text. Promotion's synthesis of a large
corpus's window does not request the field — a manual's window is not a
reminder. Salvage behavior on a malformed reply is kept; a missing or
malformed `moment` never fails artifacts that are otherwise fine.

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
echo, which is the machinery this reshape deletes.

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
  parked cases;
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
