# Contained mode, part 2: inference on the device — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `Embedder`, `Reranker` and `Completer` implemented in process over llama.cpp, so a core can embed, rerank and answer with model files and no endpoint.

**Architecture:** `src/infer/local.rs`, behind the `contained` feature. A llama.cpp context is not `Send`, so each loaded model lives on its own thread and is spoken to over a channel; the thread drops the model after an idle period and the next call loads it again. That one mechanism is the spec's whole memory discipline. The three roles are three job types over it. `Core::with_local_models` swaps them into a built core; part 3 calls it from `Core.start`.

**Tech Stack:** `llama-cpp-2` 0.1.156 (bundles a llama.cpp that already carries the `qwen35`, `gemma4` and `gemma-embedding` architectures), CPU only, no OpenMP.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, section 2 "Inference". Speech (whisper.cpp over a shared ggml) is split off as part 2b: it is a build problem of its own, and nothing here waits on it.

## Global Constraints

- `llama-cpp-2` pinned exactly, `=0.1.156`, `default-features = false`: no OpenMP (Android has no system libomp), no GPU backend.
- Everything new is behind `contained`. `cargo build` without it is unchanged.
- Tests that need a model never run in the ordinary suite. They run when `ENGRAM_TEST_MODELS` names a directory holding `embed.gguf`, `rerank.gguf` and `ask.gguf`, and return early otherwise, printing that they were skipped.
- Prompts, templates and budgets stay the server's. This plan implements `embed_raw`, `rerank`, `complete`/`answer`/`answer_streaming` and nothing above them.
- Vectors are L2-normalised, as llama-server returns them, so a base embedded on a phone and one embedded through an endpoint hold the same numbers for the same model.
- Commit messages: `feat(infer): …`, ending `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

## What a spike already settled (2026-09-18, throwaway, not kept)

`llama-cpp-2` 0.1.156 builds on the dev machine in 1m34s with libclang present. EmbeddingGemma Q8_0 from the ungated `ggml-org/embeddinggemma-300M-GGUF` loads and embeds: 768 dimensions, cosine 0.772 between an English sentence and its German translation against 0.243 for an unrelated one; load plus three embeddings in 0.37 s.

## File Structure

- Create `src/infer/local.rs` — the worker, the three roles, their gated tests.
- Modify `src/infer/mod.rs` — declare it.
- Modify `Cargo.toml` — the dependency, added to `contained`.
- Modify `src/core/mod.rs` — `Core::with_local_models`, and one gated end-to-end test.

## Test models

```bash
mkdir -p ~/.cache/engram-test-models && cd ~/.cache/engram-test-models
curl -L -o embed.gguf  https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/main/embeddinggemma-300M-Q8_0.gguf
curl -L -o rerank.gguf https://huggingface.co/gpustack/jina-reranker-v1-tiny-en-GGUF/resolve/main/jina-reranker-v1-tiny-en-Q8_0.gguf
curl -L -o ask.gguf    https://huggingface.co/unsloth/Qwen3.5-0.8B-GGUF/resolve/main/Qwen3.5-0.8B-Q4_K_M.gguf
export ENGRAM_TEST_MODELS=~/.cache/engram-test-models
```

If a URL has moved, any GGUF of the same kind serves: an embedding model, a BERT-family cross-encoder reranker, a small chat model. The tests assert behaviour, not numbers.

---

### Task 1: The worker and the embedder

**Files:** Create `src/infer/local.rs`; modify `src/infer/mod.rs`, `Cargo.toml`.

**Interfaces — produces:**
- `pub struct LocalEmbedder`; `LocalEmbedder::new(path: PathBuf, role: &EmbedRole) -> LocalEmbedder`
- internal `Worker<J>`, `trait Job`, `fn backend()`, `fn failed(role, detail) -> Error`, `fn threads() -> i32`, used by Tasks 2 and 3.

- [ ] **Step 1: Dependency and module**

`Cargo.toml`, beside `sqlite-vec`:

```toml
# llama.cpp in process, for the contained build. CPU only: no OpenMP, which
# Android does not ship, and no GPU backend. Pinned exactly — the C++ under it
# moves fast, and an upgrade is a decision.
llama-cpp-2 = { version = "=0.1.156", default-features = false, optional = true }
encoding_rs = { version = "0.8", optional = true }
```

and `contained = ["dep:sqlite-vec", "dep:libsqlite3-sys", "dep:llama-cpp-2", "dep:encoding_rs"]`.

`src/infer/mod.rs`, after `pub mod lang;`:

```rust
#[cfg(feature = "contained")]
pub mod local;
```

- [ ] **Step 2: The failing test** — at the bottom of the new `src/infer/local.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::Embedder;

    /// The model a gated test needs, or `None` with a line saying it was
    /// skipped. Gated rather than `#[ignore]`d so that one variable turns on
    /// exactly the tests whose files are there.
    pub(super) fn model(name: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("ENGRAM_TEST_MODELS")
            .map(|d| std::path::PathBuf::from(d).join(name))
            .filter(|p| p.exists());
        if path.is_none() {
            eprintln!("skipped: no {name} under ENGRAM_TEST_MODELS");
        }
        path
    }

    fn embed_role() -> crate::config::EmbedRole {
        crate::config::Config::test_default().infer.embed
    }

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        crate::vector::cosine(a, b)
    }

    #[tokio::test]
    async fn a_translation_is_nearer_than_a_stranger_and_vectors_are_unit_length() {
        let Some(path) = model("embed.gguf") else { return };
        let e = LocalEmbedder::new(path, &embed_role());
        let v = e
            .embed_raw(&[
                "the invoice is in the blue folder".into(),
                "die Rechnung liegt im blauen Ordner".into(),
                "a recipe for sourdough bread".into(),
            ])
            .await
            .unwrap();
        assert_eq!(v.len(), 3);
        assert!(cos(&v[0], &v[1]) > cos(&v[0], &v[2]) + 0.2);
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "{norm}");
    }

    #[tokio::test]
    async fn a_text_longer_than_the_window_is_cut_not_refused() {
        let Some(path) = model("embed.gguf") else { return };
        let mut role = embed_role();
        role.max_input_tokens = 64;
        let e = LocalEmbedder::new(path, &role);
        let long = "word ".repeat(2_000);
        assert_eq!(e.embed_raw(&[long]).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error_that_names_it() {
        let e = LocalEmbedder::new("/nonexistent/model.gguf".into(), &embed_role());
        let err = e.embed_raw(&["x".into()]).await.unwrap_err().to_string();
        assert!(err.contains("/nonexistent/model.gguf"), "{err}");
    }

    #[tokio::test]
    async fn the_model_is_dropped_when_idle_and_loaded_again_on_the_next_call() {
        let Some(path) = model("embed.gguf") else { return };
        let mut e = LocalEmbedder::new(path, &embed_role());
        e.worker.idle = std::time::Duration::from_millis(50);
        e.embed_raw(&["one".into()]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(!e.worker.is_loaded(), "the thread outlived its idle period");
        e.embed_raw(&["two".into()]).await.unwrap();
        assert!(e.worker.is_loaded());
    }
}
```

Run: `cargo test --features contained --lib infer::local` — Expected: does not compile.

- [ ] **Step 3: Implement** — the top of `src/infer/local.rs`:

```rust
//! The inference roles, run in this process over llama.cpp.
//!
//! A llama.cpp context is a pointer into C++ state and is not `Send`, so a
//! loaded model lives on a thread of its own and is handed jobs over a
//! channel. The thread drops the model when nothing has asked for it in a
//! while, and the next job starts a new thread that loads it again — weights
//! are memory-mapped, so that is fractions of a second. On a phone that is the
//! whole of memory management: the embedder's idle period is long, the
//! others' short, and a model nobody is using is not resident.
//!
//! Only the wire calls live here. Templates, prompts and budgets are the
//! callers', and are the same ones the HTTP implementations serve.

use crate::config::{AskRole, EmbedRole, EmbedTemplates};
use crate::error::{Error, Result};
use crate::infer::{Completer, Completion, Delta, Embedder, Reranker};
use async_trait::async_trait;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;
use tokio::sync::oneshot;

/// How long each role keeps its model after the last call.
const EMBED_IDLE: Duration = Duration::from_secs(600);
const RERANK_IDLE: Duration = Duration::from_secs(60);
const ASK_IDLE: Duration = Duration::from_secs(20);

/// llama.cpp's process-wide state. It may be initialised once, ever.
fn backend() -> &'static LlamaBackend {
    static BACKEND: OnceLock<LlamaBackend> = OnceLock::new();
    BACKEND.get_or_init(|| {
        let mut b = LlamaBackend::init().expect("llama.cpp initialises once per process");
        // It logs every tensor it loads to stderr; none of that is ours to show.
        b.void_logs();
        b
    })
}

fn failed(role: &'static str, detail: impl std::fmt::Display) -> Error {
    Error::Inference { role, detail: detail.to_string() }
}

/// The big cores, roughly. More threads than that on a phone lands work on
/// the efficiency cores, and the whole batch then waits for the slowest.
fn threads() -> i32 {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4) as i32
}

/// Something a model thread can be asked to do, and told it cannot.
trait Job: Send + 'static {
    fn fail(self, role: &'static str, detail: &str);
}

/// One model, on one thread, for as long as it is being used.
///
/// `tx` is `Some` while a thread is serving. The thread clears it under the
/// lock as the last thing it does, and a sender holds the lock while it sends,
/// so a job is never handed to a thread that has decided to stop.
struct Worker<J: Job> {
    role: &'static str,
    path: PathBuf,
    idle: Duration,
    tx: Arc<Mutex<Option<mpsc::Sender<J>>>>,
    /// Builds the context and answers jobs until `next` says there are no
    /// more. Runs on the model's thread; the context never leaves it.
    serve: Arc<dyn Fn(&LlamaModel, &mut dyn FnMut() -> Option<J>) -> Result<()> + Send + Sync>,
}

impl<J: Job> Worker<J> {
    fn new(
        role: &'static str,
        path: PathBuf,
        idle: Duration,
        serve: impl Fn(&LlamaModel, &mut dyn FnMut() -> Option<J>) -> Result<()> + Send + Sync + 'static,
    ) -> Worker<J> {
        Worker { role, path, idle, tx: Arc::new(Mutex::new(None)), serve: Arc::new(serve) }
    }

    fn is_loaded(&self) -> bool {
        self.tx.lock().unwrap().is_some()
    }

    fn submit(&self, job: J) {
        let mut slot = self.tx.lock().unwrap();
        let job = match slot.as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                // The thread died without clearing the slot: it panicked.
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (tx, rx) = mpsc::channel::<J>();
        tx.send(job).expect("the receiver is in hand");
        *slot = Some(tx);
        let (role, path, idle) = (self.role, self.path.clone(), self.idle);
        let (slot, serve) = (self.tx.clone(), self.serve.clone());
        std::thread::Builder::new()
            .name(format!("engram-{role}"))
            .spawn(move || {
                // The next job, or `None` once idle — decided under the lock,
                // so `submit` either sees the slot cleared or gets its job in
                // before the last look.
                let mut next = || match rx.recv_timeout(idle) {
                    Ok(job) => Some(job),
                    Err(_) => {
                        let mut slot = slot.lock().unwrap();
                        match rx.try_recv() {
                            Ok(job) => Some(job),
                            Err(_) => {
                                *slot = None;
                                None
                            }
                        }
                    }
                };
                let outcome = LlamaModel::load_from_file(backend(), &path, &LlamaModelParams::default())
                    .map_err(|e| failed(role, format!("{}: {e}", path.display())))
                    .and_then(|model| serve(&model, &mut next));
                if let Err(e) = outcome {
                    // Could not load, or could not build a context: say so to
                    // everyone waiting, then stop.
                    let detail = e.to_string();
                    let mut slot = slot.lock().unwrap();
                    *slot = None;
                    drop(slot);
                    while let Ok(job) = rx.try_recv() {
                        job.fail(role, &detail);
                    }
                }
            })
            .expect("a thread can be started");
    }
}

async fn answer<T>(role: &'static str, rx: oneshot::Receiver<Result<T>>) -> Result<T> {
    rx.await.unwrap_or_else(|_| Err(failed(role, "the model's thread ended before it answered")))
}

fn context<'m>(model: &'m LlamaModel, role: &'static str, params: LlamaContextParams) -> Result<LlamaContext<'m>> {
    model.new_context(backend(), params).map_err(|e| failed(role, e))
}

// ---- embed ----------------------------------------------------------------

struct EmbedJob {
    texts: Vec<String>,
    reply: oneshot::Sender<Result<Vec<Vec<f32>>>>,
}

impl Job for EmbedJob {
    fn fail(self, role: &'static str, detail: &str) {
        let _ = self.reply.send(Err(failed(role, detail)));
    }
}

pub struct LocalEmbedder {
    worker: Worker<EmbedJob>,
    model: String,
    dim: usize,
    max_input_tokens: usize,
    templates: EmbedTemplates,
}

impl LocalEmbedder {
    /// `role` still says what a vector *means* — the dimension, the window,
    /// the templates. Only its endpoint goes unused.
    pub fn new(path: PathBuf, role: &EmbedRole) -> LocalEmbedder {
        let n_ctx = role.max_input_tokens.max(8) as u32;
        let worker = Worker::new("embed", path, EMBED_IDLE, move |model, next| {
            let params = LlamaContextParams::default()
                .with_embeddings(true)
                .with_n_ctx(NonZeroU32::new(n_ctx))
                // An embedding model attends both ways, so a text has to fit
                // one physical batch: there is no carrying state across two.
                .with_n_batch(n_ctx)
                .with_n_ubatch(n_ctx)
                .with_n_threads(threads())
                .with_n_threads_batch(threads());
            let mut ctx = context(model, "embed", params)?;
            while let Some(job) = next() {
                let out = job.texts.iter().map(|t| embed_one(model, &mut ctx, t, n_ctx as usize)).collect();
                let _ = job.reply.send(out);
            }
            Ok(())
        });
        LocalEmbedder {
            worker,
            model: role.model.clone(),
            dim: role.dim,
            max_input_tokens: role.max_input_tokens,
            templates: EmbedTemplates {
                query_template: role.query_template.clone(),
                document_template: role.document_template.clone(),
                document_template_untitled: role.document_template_untitled.clone(),
            },
        }
    }
}

fn embed_one(model: &LlamaModel, ctx: &mut LlamaContext, text: &str, n_ctx: usize) -> Result<Vec<f32>> {
    let mut tokens = model.str_to_token(text, AddBos::Always).map_err(|e| failed("embed", e))?;
    // Cut, not refused: the callers budget against an estimate, and an
    // estimate that ran a few tokens short must not lose the artifact.
    tokens.truncate(n_ctx);
    let mut batch = LlamaBatch::new(n_ctx, 1);
    batch.add_sequence(&tokens, 0, false).map_err(|e| failed("embed", e))?;
    ctx.clear_kv_cache();
    ctx.decode(&mut batch).map_err(|e| failed("embed", e))?;
    let raw = ctx.embeddings_seq_ith(0).map_err(|e| failed("embed", e))?;
    let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    Ok(if norm > 0.0 { raw.iter().map(|x| x / norm).collect() } else { raw.to_vec() })
}

#[async_trait]
impl Embedder for LocalEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let (reply, rx) = oneshot::channel();
        self.worker.submit(EmbedJob { texts: texts.to_vec(), reply });
        answer("embed", rx).await
    }
    fn templates(&self) -> &EmbedTemplates { &self.templates }
    fn dim(&self) -> usize { self.dim }
    fn model(&self) -> &str { &self.model }
    fn max_input_tokens(&self) -> usize { self.max_input_tokens }
}
```

A context borrows its model, so the two cannot sit in one struct: that is why the worker is generic over a serving closure that owns the loop, and not over a stored context. If `EmbedTemplates` has other fields or a constructor, use what `HttpEmbedder::new` does at `src/infer/openai.rs:739`.

- [ ] **Step 4: Run** — `cargo test --features contained --lib infer::local` with `ENGRAM_TEST_MODELS` set. Expected: 4 passed.

- [ ] **Step 5: Commit** — `feat(infer): an embedder that runs in this process`

---

### Task 2: The reranker

**Interfaces — produces:** `pub struct LocalReranker`; `LocalReranker::new(path: PathBuf, n_ctx: u32) -> LocalReranker`.

- [ ] **Step 1: Failing test**, in the test module:

```rust
    use crate::infer::Reranker;

    #[tokio::test]
    async fn the_passage_that_answers_comes_first_with_its_own_index() {
        let Some(path) = model("rerank.gguf") else { return };
        let r = LocalReranker::new(path, 512);
        let docs = vec![
            "Bread needs flour, water, salt and time.".to_string(),
            "The invoice for the tax adviser is in the blue folder.".to_string(),
            "Trains to Hamburg leave from platform 7.".to_string(),
        ];
        let got = r.rerank("where is the invoice for the tax adviser", &docs, 2).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, 1);
        assert!(got[0].1 > got[1].1);
    }
```

- [ ] **Step 2: Implement**, after the embed section:

```rust
// ---- rerank ---------------------------------------------------------------

struct RerankJob {
    query: String,
    docs: Vec<String>,
    reply: oneshot::Sender<Result<Vec<f32>>>,
}

impl Job for RerankJob {
    fn fail(self, role: &'static str, detail: &str) {
        let _ = self.reply.send(Err(failed(role, detail)));
    }
}

/// A cross-encoder: query and passage read together, one score out.
pub struct LocalReranker {
    worker: Worker<RerankJob>,
}

impl LocalReranker {
    pub fn new(path: PathBuf, n_ctx: u32) -> LocalReranker {
        let worker = Worker::new("rerank", path, RERANK_IDLE, move |model, next| {
            let params = LlamaContextParams::default()
                .with_embeddings(true)
                .with_pooling_type(LlamaPoolingType::Rank)
                .with_n_ctx(NonZeroU32::new(n_ctx))
                .with_n_batch(n_ctx)
                .with_n_ubatch(n_ctx)
                .with_n_threads(threads())
                .with_n_threads_batch(threads());
            let mut ctx = context(model, "rerank", params)?;
            while let Some(job) = next() {
                let out = job.docs.iter().map(|d| score_one(model, &mut ctx, &job.query, d, n_ctx as usize)).collect();
                let _ = job.reply.send(out);
            }
            Ok(())
        });
        LocalReranker { worker }
    }
}

/// `[BOS] query [EOS] [SEP] passage [EOS]`, the pair layout llama.cpp's own
/// server builds for a rank-pooled model. The passage gives way when the pair
/// is too long; the query never does.
fn score_one(model: &LlamaModel, ctx: &mut LlamaContext, query: &str, doc: &str, n_ctx: usize) -> Result<f32> {
    let mut tokens = model.str_to_token(query, AddBos::Always).map_err(|e| failed("rerank", e))?;
    tokens.truncate(n_ctx / 2);
    tokens.push(model.token_eos());
    tokens.push(model.token_sep());
    let mut passage = model.str_to_token(doc, AddBos::Never).map_err(|e| failed("rerank", e))?;
    passage.truncate(n_ctx.saturating_sub(tokens.len() + 1));
    tokens.extend(passage);
    tokens.push(model.token_eos());
    let mut batch = LlamaBatch::new(n_ctx, 1);
    batch.add_sequence(&tokens, 0, false).map_err(|e| failed("rerank", e))?;
    ctx.clear_kv_cache();
    ctx.decode(&mut batch).map_err(|e| failed("rerank", e))?;
    let out = ctx.embeddings_seq_ith(0).map_err(|e| failed("rerank", e))?;
    out.first().copied().ok_or_else(|| failed("rerank", "the model returned no score; is it a reranker?"))
}

#[async_trait]
impl Reranker for LocalReranker {
    async fn rerank(&self, query: &str, docs: &[String], top_n: usize) -> Result<Vec<(usize, f32)>> {
        let (reply, rx) = oneshot::channel();
        self.worker.submit(RerankJob { query: query.into(), docs: docs.to_vec(), reply });
        let mut scored: Vec<(usize, f32)> = answer("rerank", rx).await?.into_iter().enumerate().collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(top_n);
        Ok(scored)
    }
}
```

- [ ] **Step 3: Run** — Expected: 5 passed. If the right passage does not come first, print all three scores before changing anything: a reranker whose scores barely differ is being fed a pair layout it was not trained on, and the fix is in `score_one`'s token order, which `llama.cpp/tools/server/utils.hpp` (`format_rerank`) states for the bundled version.

- [ ] **Step 4: Commit** — `feat(infer): a cross-encoder reranker in process`

---

### Task 3: The completer

**Interfaces — produces:** `pub struct LocalCompleter`; `LocalCompleter::new(path: PathBuf, role: &AskRole) -> LocalCompleter`.

- [ ] **Step 1: Failing tests**:

```rust
    use crate::infer::{Completer, Delta};

    fn ask_role() -> crate::config::AskRole {
        let mut role = crate::config::Config::test_default().infer.ask.expect("the test config asks");
        role.context_tokens = 2048;
        role.max_output_tokens = 64;
        role
    }

    #[tokio::test]
    async fn it_answers_from_what_it_was_shown_and_streams_the_same_words() {
        let Some(path) = model("ask.gguf") else { return };
        let c = LocalCompleter::new(path, &ask_role());
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let done = c
            .answer_streaming(
                "Answer in one short sentence, using only the note.",
                "Note: the invoice is in the blue folder.\nQuestion: where is the invoice?",
                64,
                tx,
            )
            .await
            .unwrap();
        assert!(done.text.to_lowercase().contains("blue"), "{}", done.text);
        let mut streamed = String::new();
        while let Some(d) = rx.recv().await {
            if let Delta::Token(t) = d { streamed.push_str(&t) }
        }
        assert_eq!(streamed, done.text);
    }

    #[tokio::test]
    async fn a_ceiling_reached_is_reported_as_truncated() {
        let Some(path) = model("ask.gguf") else { return };
        let c = LocalCompleter::new(path, &ask_role());
        let done = c.answer("You are verbose.", "Count from one to five hundred in words.", 8).await.unwrap();
        assert!(done.truncated);
    }

    #[tokio::test]
    async fn a_prompt_larger_than_the_window_is_refused_in_words() {
        let Some(path) = model("ask.gguf") else { return };
        let c = LocalCompleter::new(path, &ask_role());
        let err = c.complete("s", &"word ".repeat(5_000)).await.unwrap_err().to_string();
        assert!(err.contains("2048"), "{err}");
    }
```

If `test_default()` has no `[infer.ask]`, build the `AskRole` the way `src/config.rs`'s own tests do.

- [ ] **Step 2: Implement**:

```rust
// ---- ask ------------------------------------------------------------------

struct AskJob {
    system: String,
    user: String,
    ceiling: usize,
    sink: Option<tokio::sync::mpsc::Sender<Delta>>,
    reply: oneshot::Sender<Result<Completion>>,
}

impl Job for AskJob {
    fn fail(self, role: &'static str, detail: &str) {
        let _ = self.reply.send(Err(failed(role, detail)));
    }
}

pub struct LocalCompleter {
    worker: Worker<AskJob>,
    context_tokens: usize,
    max_output_tokens: usize,
}

/// Prompt tokens decoded per step. Small enough that a long prompt does not
/// hold one allocation the size of the window.
const PROMPT_BATCH: usize = 512;

impl LocalCompleter {
    pub fn new(path: PathBuf, role: &AskRole) -> LocalCompleter {
        let n_ctx = role.context_tokens as u32;
        let worker = Worker::new("ask", path, ASK_IDLE, move |model, next| {
            let params = LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(n_ctx))
                .with_n_batch(PROMPT_BATCH as u32)
                .with_n_threads(threads())
                .with_n_threads_batch(threads());
            let mut ctx = context(model, "ask", params)?;
            while let Some(job) = next() {
                let out = generate(model, &mut ctx, &job, n_ctx as usize);
                let _ = job.reply.send(out);
            }
            Ok(())
        });
        LocalCompleter { worker, context_tokens: role.context_tokens, max_output_tokens: role.max_output_tokens }
    }

    async fn run(&self, system: &str, user: &str, ceiling: usize, sink: Option<tokio::sync::mpsc::Sender<Delta>>) -> Result<Completion> {
        let (reply, rx) = oneshot::channel();
        self.worker.submit(AskJob { system: system.into(), user: user.into(), ceiling, sink, reply });
        answer("ask", rx).await
    }
}

fn generate(model: &LlamaModel, ctx: &mut LlamaContext, job: &AskJob, n_ctx: usize) -> Result<Completion> {
    // The model's own template, from its file: the one it was trained on.
    let template = model.chat_template(None).map_err(|e| failed("ask", e))?;
    let chat = [
        LlamaChatMessage::new("system".into(), job.system.clone()).map_err(|e| failed("ask", e))?,
        LlamaChatMessage::new("user".into(), job.user.clone()).map_err(|e| failed("ask", e))?,
    ];
    let prompt = model.apply_chat_template(&template, &chat, true).map_err(|e| failed("ask", e))?;
    // The template writes its own opening token where the model wants one.
    let tokens = model.str_to_token(&prompt, AddBos::Never).map_err(|e| failed("ask", e))?;
    if tokens.len() + job.ceiling > n_ctx {
        return Err(failed("ask", format!(
            "the prompt is {} tokens and the answer may be {}; the window is {n_ctx}",
            tokens.len(), job.ceiling
        )));
    }

    ctx.clear_kv_cache();
    let mut batch = LlamaBatch::new(PROMPT_BATCH, 1);
    let last = tokens.len() - 1;
    for (n, chunk) in tokens.chunks(PROMPT_BATCH).enumerate() {
        batch.clear();
        for (i, token) in chunk.iter().enumerate() {
            let pos = n * PROMPT_BATCH + i;
            batch.add(*token, pos as i32, &[0], pos == last).map_err(|e| failed("ask", e))?;
        }
        ctx.decode(&mut batch).map_err(|e| failed("ask", e))?;
    }

    // Near-greedy: an answer over retrieved passages wants the likeliest
    // words, and a little temperature only to step out of a repetition.
    let mut sampler = LlamaSampler::chain_simple([LlamaSampler::top_k(20), LlamaSampler::temp(0.3), LlamaSampler::dist(0)]);
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut text = String::new();
    let mut pos = tokens.len() as i32;
    let mut truncated = true;
    for _ in 0..job.ceiling {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            truncated = false;
            break;
        }
        let piece = model.token_to_piece(token, &mut decoder, false, None).map_err(|e| failed("ask", e))?;
        if !piece.is_empty() {
            text.push_str(&piece);
            if let Some(sink) = &job.sink {
                // A reader that went away does not stop the answer; see
                // `Completer::answer_streaming`.
                let _ = sink.blocking_send(Delta::Token(piece));
            }
        }
        batch.clear();
        batch.add(token, pos, &[0], true).map_err(|e| failed("ask", e))?;
        pos += 1;
        ctx.decode(&mut batch).map_err(|e| failed("ask", e))?;
    }
    Ok(Completion { text, truncated })
}

#[async_trait]
impl Completer for LocalCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        Ok(self.run(system, user, self.max_output_tokens, None).await?.text)
    }
    async fn answer(&self, system: &str, user: &str, ceiling: usize) -> Result<Completion> {
        self.run(system, user, ceiling.min(self.max_output_tokens), None).await
    }
    async fn answer_streaming(&self, system: &str, user: &str, ceiling: usize, sink: tokio::sync::mpsc::Sender<Delta>) -> Result<Completion> {
        self.run(system, user, ceiling.min(self.max_output_tokens), Some(sink)).await
    }
    fn context_tokens(&self) -> usize { self.context_tokens }
    fn max_output_tokens(&self) -> usize { self.max_output_tokens }
}
```

Known and left for the device pass, where the ask model is chosen: a reasoning model's thinking arrives here as answer text, not as `Delta::Reasoning`. The split belongs on token ids the chosen model defines, not on text, and there is no model chosen yet to define them.

- [ ] **Step 3: Run** — Expected: 8 passed.

- [ ] **Step 4: Commit** — `feat(infer): ask answered by a model in this process`

---

### Task 4: Into the core, end to end

**Files:** Modify `src/core/mod.rs`.

**Interfaces — produces:** `pub struct crate::infer::local::LocalModels { pub embed: Option<PathBuf>, pub rerank: Option<PathBuf>, pub ask: Option<PathBuf> }` and `Core::with_local_models(self, cfg: &Config, models: &LocalModels) -> Core`. Part 3 builds `LocalModels` from what `Core.start` is handed.

- [ ] **Step 1: Failing test** — in `contained_tests` in `src/core/mod.rs`:

```rust
    /// Part 1's test with the fake embedder gone: real vectors from a real
    /// model, into a file, found by a query in another language.
    #[tokio::test]
    async fn a_german_capture_is_found_by_an_english_question_with_no_endpoint_anywhere() {
        let Some(dir) = std::env::var_os("ENGRAM_TEST_MODELS").map(std::path::PathBuf::from) else {
            eprintln!("skipped: ENGRAM_TEST_MODELS is not set");
            return;
        };
        let mut cfg = crate::config::Config::test_default();
        cfg.infer.embed.dim = 768;
        cfg.infer.embed.max_input_tokens = 512;
        let models = crate::infer::local::LocalModels { embed: Some(dir.join("embed.gguf")), rerank: None, ask: None };
        let tmp = tempfile::tempdir().unwrap();
        let factory = crate::tenants::SqliteFactory { path: tmp.path().join("engram.db"), scoring: crate::vector::sqlite::Scoring::off() };
        let mut core = test_core().await.with_local_models(&cfg, &models);
        core.vectors = factory.open("ignored", 768).await.unwrap();

        let out = core.ingest("Die Rechnung für den Steuerberater liegt im blauen Ordner.", "web", None).await.unwrap();
        core.ingest("Brot braucht Mehl, Wasser, Salz und Zeit.", "web", None).await.unwrap();
        crate::jobs::test_support::drain(&core).await;

        let q = crate::core::search::SearchQuery {
            q: "where did I put the invoice for the tax adviser".into(), limit: 5, tags: vec![], category: None,
            mark: true, rerank: false, explain: false, include_deprecated: false, include_superseded: false,
        };
        let hits = core.search(&q, Door::Cli).await.unwrap();
        assert_eq!(hits[0].corpus_id, out.id, "no word is shared; only the meaning is");
    }
```

- [ ] **Step 2: Implement** — in `src/infer/local.rs`:

```rust
/// Which roles run on this device, by the file each one loads. A role left
/// `None` keeps whatever the configuration gave it — an endpoint, or nothing.
#[derive(Debug, Clone, Default)]
pub struct LocalModels {
    pub embed: Option<PathBuf>,
    pub rerank: Option<PathBuf>,
    pub ask: Option<PathBuf>,
}
```

and in `src/core/mod.rs`, in `impl Core`:

```rust
    /// The same core with the named roles served by models in this process.
    #[cfg(feature = "contained")]
    pub fn with_local_models(mut self, cfg: &Config, models: &crate::infer::local::LocalModels) -> Core {
        use crate::infer::local::{LocalCompleter, LocalEmbedder, LocalReranker};
        if let Some(path) = &models.embed {
            self.embedder = Arc::new(LocalEmbedder::new(path.clone(), &cfg.infer.embed));
        }
        if let Some(path) = &models.rerank {
            // A pair is a query and one passage; 512 holds both with room.
            self.reranker = Some(Arc::new(LocalReranker::new(path.clone(), 512)));
            if self.rerank_apply.is_empty() {
                self.rerank_apply = vec![crate::config::RerankApply::Ask, crate::config::RerankApply::Search];
            }
        }
        if let (Some(path), Some(role)) = (&models.ask, cfg.infer.ask.as_ref()) {
            self.completer = Some(Arc::new(LocalCompleter::new(path.clone(), role)));
        }
        self
    }
```

- [ ] **Step 3: Run**

`cargo test --features contained --lib core::contained_tests infer::local` with models — Expected: all pass.
`cargo test --features contained --lib infer::local` with `ENGRAM_TEST_MODELS` unset — Expected: all pass, each gated one printing `skipped`.
`cargo build` and `cargo clippy --features contained --all-targets -- -D warnings` — Expected: clean.

- [ ] **Step 4: Commit** — `feat(contained): a core whose models are files`
