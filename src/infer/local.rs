//! Inference roles in process over llama.cpp. A context is not `Send`, so each model lives on its own
//! thread fed by a channel and is dropped when idle; weights are memory-mapped, so a reload is cheap.

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
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;
use tokio::sync::oneshot;

const EMBED_IDLE: Duration = Duration::from_secs(600);
const RERANK_IDLE: Duration = Duration::from_secs(60);
const ASK_IDLE: Duration = Duration::from_secs(20);

/// llama.cpp's process-wide state may be initialised once, ever.
fn backend(role: &'static str) -> Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<Option<LlamaBackend>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            LlamaBackend::init().ok().map(|mut b| {
                // It logs every tensor it loads to stderr.
                b.void_logs();
                b
            })
        })
        .as_ref()
        .ok_or_else(|| failed(role, "llama.cpp did not initialise"))
}

fn failed(role: &'static str, detail: impl std::fmt::Display) -> Error {
    Error::Inference {
        role,
        detail: detail.to_string(),
    }
}

/// More threads than the big cores lands work on efficiency cores, and the batch waits for the slowest.
fn threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 4) as i32
}

trait Job: Send + 'static {
    fn fail(self, role: &'static str, detail: &str);
}

/// The context never leaves the model's thread.
type Serve<J> = dyn Fn(&LlamaModel, &mut dyn FnMut() -> Option<J>) -> Result<()> + Send + Sync;

/// The thread clears `tx` under the lock as its last act and senders hold the lock while sending,
/// so a job is never handed to a thread that has decided to stop.
struct Worker<J: Job> {
    role: &'static str,
    path: PathBuf,
    idle: Duration,
    tx: Arc<Mutex<Option<mpsc::Sender<J>>>>,
    serve: Arc<Serve<J>>,
}

impl<J: Job> Worker<J> {
    fn new(
        role: &'static str,
        path: PathBuf,
        idle: Duration,
        serve: impl Fn(&LlamaModel, &mut dyn FnMut() -> Option<J>) -> Result<()> + Send + Sync + 'static,
    ) -> Worker<J> {
        Worker {
            role,
            path,
            idle,
            tx: Arc::new(Mutex::new(None)),
            serve: Arc::new(serve),
        }
    }

    #[cfg(test)]
    fn is_loaded(&self) -> bool {
        self.tx.lock().unwrap().is_some()
    }

    fn submit(&self, job: J) {
        let mut slot = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let job = match slot.as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                // The thread died without clearing the slot: it panicked.
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (tx, rx) = mpsc::channel::<J>();
        // Cannot fail: the receiver is in hand.
        let _ = tx.send(job);
        *slot = Some(tx);
        let (role, path, idle) = (self.role, self.path.clone(), self.idle);
        let (slot, serve) = (self.tx.clone(), self.serve.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("engram-{role}"))
            .spawn(move || {
                // Decided under the lock, so `submit` sees the slot cleared or gets its job in first.
                let mut next = || match rx.recv_timeout(idle) {
                    Ok(job) => Some(job),
                    Err(_) => {
                        let mut slot = slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match rx.try_recv() {
                            Ok(job) => Some(job),
                            Err(_) => {
                                *slot = None;
                                None
                            }
                        }
                    }
                };
                // Checked first: the binding panics on a missing file, which would hide which file.
                let loaded = if path.is_file() {
                    backend(role).and_then(|b| {
                        LlamaModel::load_from_file(b, &path, &LlamaModelParams::default())
                            .map_err(|e| failed(role, format!("{}: {e}", path.display())))
                    })
                } else {
                    Err(failed(
                        role,
                        format!("{}: no such model file", path.display()),
                    ))
                };
                let outcome = loaded.and_then(|model| serve(&model, &mut next));
                if let Err(e) = outcome {
                    let detail = e.to_string();
                    let mut slot = slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *slot = None;
                    drop(slot);
                    while let Ok(job) = rx.try_recv() {
                        job.fail(role, &detail);
                    }
                }
            });
        if let Err(e) = spawned {
            // The job goes with the closure, so its caller sees its reply channel close.
            tracing::error!(error = %e, role = self.role, "could not start the model thread");
        }
    }
}

async fn answer<T>(role: &'static str, rx: oneshot::Receiver<Result<T>>) -> Result<T> {
    rx.await
        .unwrap_or_else(|_| Err(failed(role, "the model's thread ended before it answered")))
}

fn context<'m>(
    model: &'m LlamaModel,
    role: &'static str,
    params: LlamaContextParams,
) -> Result<LlamaContext<'m>> {
    model
        .new_context(backend(role)?, params)
        .map_err(|e| failed(role, e))
}

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
    /// `role` still says what a vector means (dimension, window, templates); only its endpoint goes unused.
    pub fn new(path: PathBuf, role: &EmbedRole) -> LocalEmbedder {
        let n_ctx = role.max_input_tokens.max(8) as u32;
        let worker = Worker::new(
            "embed",
            path,
            EMBED_IDLE,
            move |model, next: &mut dyn FnMut() -> Option<EmbedJob>| {
                let params = LlamaContextParams::default()
                    .with_embeddings(true)
                    .with_n_ctx(NonZeroU32::new(n_ctx))
                    // Embedding attends both ways, so a text must fit one physical batch.
                    .with_n_batch(n_ctx)
                    .with_n_ubatch(n_ctx)
                    .with_n_threads(threads())
                    .with_n_threads_batch(threads());
                let mut ctx = context(model, "embed", params)?;
                while let Some(job) = next() {
                    let out = job
                        .texts
                        .iter()
                        .map(|t| embed_one(model, &mut ctx, t, n_ctx as usize))
                        .collect();
                    let _ = job.reply.send(out);
                }
                Ok(())
            },
        );
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

fn embed_one(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    text: &str,
    n_ctx: usize,
) -> Result<Vec<f32>> {
    let mut tokens = model
        .str_to_token(text, AddBos::Always)
        .map_err(|e| failed("embed", e))?;
    // Cut, not refused: callers budget by estimate, and a short estimate must not lose the artifact.
    tokens.truncate(n_ctx);
    let mut batch = LlamaBatch::new(n_ctx, 1);
    batch
        .add_sequence(&tokens, 0, false)
        .map_err(|e| failed("embed", e))?;
    ctx.clear_kv_cache();
    ctx.decode(&mut batch).map_err(|e| failed("embed", e))?;
    let raw = ctx.embeddings_seq_ith(0).map_err(|e| failed("embed", e))?;
    let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    Ok(if norm > 0.0 {
        raw.iter().map(|x| x / norm).collect()
    } else {
        raw.to_vec()
    })
}

#[async_trait]
impl Embedder for LocalEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let (reply, rx) = oneshot::channel();
        self.worker.submit(EmbedJob {
            texts: texts.to_vec(),
            reply,
        });
        answer("embed", rx).await
    }
    fn templates(&self) -> &EmbedTemplates {
        &self.templates
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn max_input_tokens(&self) -> usize {
        self.max_input_tokens
    }
}

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

pub struct LocalReranker {
    worker: Worker<RerankJob>,
}

impl LocalReranker {
    pub fn new(path: PathBuf, n_ctx: u32) -> LocalReranker {
        let worker = Worker::new(
            "rerank",
            path,
            RERANK_IDLE,
            move |model, next: &mut dyn FnMut() -> Option<RerankJob>| {
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
                    let out = job
                        .docs
                        .iter()
                        .map(|d| score_one(model, &mut ctx, &job.query, d, n_ctx as usize))
                        .collect();
                    let _ = job.reply.send(out);
                }
                Ok(())
            },
        );
        LocalReranker { worker }
    }
}

/// The pair layout llama.cpp's server builds for a rank-pooled model; the passage gives way, never the query.
fn score_one(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    query: &str,
    doc: &str,
    n_ctx: usize,
) -> Result<f32> {
    let mut tokens = model
        .str_to_token(query, AddBos::Always)
        .map_err(|e| failed("rerank", e))?;
    tokens.truncate(n_ctx / 2);
    tokens.push(model.token_eos());
    tokens.push(model.token_sep());
    let mut passage = model
        .str_to_token(doc, AddBos::Never)
        .map_err(|e| failed("rerank", e))?;
    passage.truncate(n_ctx.saturating_sub(tokens.len() + 1));
    tokens.extend(passage);
    tokens.push(model.token_eos());
    let mut batch = LlamaBatch::new(n_ctx, 1);
    batch
        .add_sequence(&tokens, 0, false)
        .map_err(|e| failed("rerank", e))?;
    ctx.clear_kv_cache();
    ctx.decode(&mut batch).map_err(|e| failed("rerank", e))?;
    let out = ctx.embeddings_seq_ith(0).map_err(|e| failed("rerank", e))?;
    out.first()
        .copied()
        .ok_or_else(|| failed("rerank", "the model returned no score; is it a reranker?"))
}

#[async_trait]
impl Reranker for LocalReranker {
    async fn rerank(
        &self,
        query: &str,
        docs: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f32)>> {
        let (reply, rx) = oneshot::channel();
        self.worker.submit(RerankJob {
            query: query.into(),
            docs: docs.to_vec(),
            reply,
        });
        let mut scored: Vec<(usize, f32)> = answer("rerank", rx)
            .await?
            .into_iter()
            .enumerate()
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(top_n);
        Ok(scored)
    }
}

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

/// Small enough that a long prompt does not hold one window-sized allocation.
const PROMPT_BATCH: usize = 512;

impl LocalCompleter {
    pub fn new(path: PathBuf, role: &AskRole) -> LocalCompleter {
        let n_ctx = role.context_tokens as u32;
        let worker = Worker::new(
            "ask",
            path,
            ASK_IDLE,
            move |model, next: &mut dyn FnMut() -> Option<AskJob>| {
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
            },
        );
        LocalCompleter {
            worker,
            context_tokens: role.context_tokens,
            max_output_tokens: role.max_output_tokens,
        }
    }

    async fn run(
        &self,
        system: &str,
        user: &str,
        ceiling: usize,
        sink: Option<tokio::sync::mpsc::Sender<Delta>>,
    ) -> Result<Completion> {
        let (reply, rx) = oneshot::channel();
        self.worker.submit(AskJob {
            system: system.into(),
            user: user.into(),
            ceiling,
            sink,
            reply,
        });
        answer("ask", rx).await
    }
}

fn generate(
    model: &LlamaModel,
    ctx: &mut LlamaContext,
    job: &AskJob,
    n_ctx: usize,
) -> Result<Completion> {
    let template = model.chat_template(None).map_err(|e| failed("ask", e))?;
    let chat = [
        LlamaChatMessage::new("system".into(), job.system.clone()).map_err(|e| failed("ask", e))?,
        LlamaChatMessage::new("user".into(), job.user.clone()).map_err(|e| failed("ask", e))?,
    ];
    let prompt = model
        .apply_chat_template(&template, &chat, true)
        .map_err(|e| failed("ask", e))?;
    // The template writes its own opening token where the model wants one.
    let tokens = model
        .str_to_token(&prompt, AddBos::Never)
        .map_err(|e| failed("ask", e))?;
    if tokens.len() + job.ceiling > n_ctx {
        return Err(failed(
            "ask",
            format!(
                "the prompt is {} tokens and the answer may be {}; the window is {n_ctx}",
                tokens.len(),
                job.ceiling
            ),
        ));
    }

    ctx.clear_kv_cache();
    let mut batch = LlamaBatch::new(PROMPT_BATCH, 1);
    let last = tokens.len() - 1;
    for (n, chunk) in tokens.chunks(PROMPT_BATCH).enumerate() {
        batch.clear();
        for (i, token) in chunk.iter().enumerate() {
            let pos = n * PROMPT_BATCH + i;
            batch
                .add(*token, pos as i32, &[0], pos == last)
                .map_err(|e| failed("ask", e))?;
        }
        ctx.decode(&mut batch).map_err(|e| failed("ask", e))?;
    }

    // Near-greedy, with a little temperature only to escape repetition.
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::top_k(20),
        LlamaSampler::temp(0.3),
        LlamaSampler::dist(0),
    ]);
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    // Some templates open the thinking themselves; the model then writes only the closing tag.
    let mut split = ThinkSplit::new(prompt.trim_end().ends_with(THINK_OPEN));
    let mut text = String::new();
    let emit = |parts: Vec<(bool, String)>, text: &mut String| {
        for (thinking, mut part) in parts {
            if !thinking && text.is_empty() {
                part = part.trim_start().to_string();
            }
            if part.is_empty() {
                continue;
            }
            if !thinking {
                text.push_str(&part);
            }
            if let Some(sink) = &job.sink {
                // A reader that went away does not stop the answer; see `Completer::answer_streaming`.
                let _ = sink.blocking_send(if thinking {
                    Delta::Reasoning(part)
                } else {
                    Delta::Token(part)
                });
            }
        }
    };
    let mut truncated = true;
    for step in 0..job.ceiling {
        let pos = (tokens.len() + step) as i32;
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            truncated = false;
            break;
        }
        let piece = model
            .token_to_piece(token, &mut decoder, false, None)
            .map_err(|e| failed("ask", e))?;
        emit(split.feed(&piece), &mut text);
        batch.clear();
        batch
            .add(token, pos, &[0], true)
            .map_err(|e| failed("ask", e))?;
        ctx.decode(&mut batch).map_err(|e| failed("ask", e))?;
    }
    emit(split.finish(), &mut text);
    Ok(Completion { text, truncated })
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// In process the think tags are in the text; a piece ending mid-tag is held until the next settles it.
struct ThinkSplit {
    thinking: bool,
    held: String,
}

impl ThinkSplit {
    fn new(thinking: bool) -> ThinkSplit {
        ThinkSplit {
            thinking,
            held: String::new(),
        }
    }

    fn feed(&mut self, piece: &str) -> Vec<(bool, String)> {
        self.held.push_str(piece);
        let mut out = Vec::new();
        loop {
            let tag = if self.thinking {
                THINK_CLOSE
            } else {
                THINK_OPEN
            };
            if let Some(at) = self.held.find(tag) {
                let rest = self.held.split_off(at + tag.len());
                self.held.truncate(at);
                out.push((self.thinking, std::mem::replace(&mut self.held, rest)));
                self.thinking = !self.thinking;
                continue;
            }
            // The longest tail a tag could still grow out of.
            let keep = (1..tag.len())
                .rev()
                .find(|n| self.held.ends_with(&tag[..*n]))
                .unwrap_or(0);
            let tail = self.held.split_off(self.held.len() - keep);
            out.push((self.thinking, std::mem::replace(&mut self.held, tail)));
            break;
        }
        out.retain(|(_, part)| !part.is_empty());
        out
    }

    /// A held tail that never became a tag was text after all.
    fn finish(&mut self) -> Vec<(bool, String)> {
        let held = std::mem::take(&mut self.held);
        if held.is_empty() {
            Vec::new()
        } else {
            vec![(self.thinking, held)]
        }
    }
}

#[async_trait]
impl Completer for LocalCompleter {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        Ok(self
            .run(system, user, self.max_output_tokens, None)
            .await?
            .text)
    }
    async fn answer(&self, system: &str, user: &str, ceiling: usize) -> Result<Completion> {
        self.run(system, user, ceiling.min(self.max_output_tokens), None)
            .await
    }
    async fn answer_streaming(
        &self,
        system: &str,
        user: &str,
        ceiling: usize,
        sink: tokio::sync::mpsc::Sender<Delta>,
    ) -> Result<Completion> {
        self.run(
            system,
            user,
            ceiling.min(self.max_output_tokens),
            Some(sink),
        )
        .await
    }
    fn context_tokens(&self) -> usize {
        self.context_tokens
    }
    fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }
}

/// A role left `None` keeps whatever the configuration gave it.
#[derive(Debug, Clone, Default)]
pub struct LocalModels {
    pub embed: Option<PathBuf>,
    pub rerank: Option<PathBuf>,
    pub ask: Option<PathBuf>,
    /// Where there is one, the microphone's door is open.
    pub speech: Option<PathBuf>,
}

/// Loaded per recording and dropped after: dictation is rare, and phone memory is worth more than the load.
pub struct LocalTranscriber {
    path: PathBuf,
    /// ISO-639-1, or `None` to detect.
    lang: Option<String>,
    one: tokio::sync::Mutex<()>,
}

impl LocalTranscriber {
    pub fn new(path: PathBuf, lang: Option<String>) -> LocalTranscriber {
        LocalTranscriber {
            path,
            lang,
            one: tokio::sync::Mutex::new(()),
        }
    }
}

#[async_trait]
impl crate::infer::Transcriber for LocalTranscriber {
    async fn transcribe(&self, audio: &[u8], mime: &str) -> Result<String> {
        let samples = crate::infer::pcm::samples_16k(audio, mime)?;
        if samples.is_empty() {
            return Ok(String::new());
        }
        let _one = self.one.lock().await;
        let (path, lang) = (self.path.clone(), self.lang.clone());
        tokio::task::spawn_blocking(move || {
            let mut model = crate::infer::whisper::Whisper::open(&path)?;
            model.run(&samples, lang.as_deref(), threads())
        })
        .await
        .map_err(|e| failed("transcribe", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::Embedder;

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
        let Some(path) = model("embed.gguf") else {
            return;
        };
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
        let Some(path) = model("embed.gguf") else {
            return;
        };
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
        let Some(path) = model("embed.gguf") else {
            return;
        };
        let mut e = LocalEmbedder::new(path, &embed_role());
        e.worker.idle = std::time::Duration::from_millis(50);
        e.embed_raw(&["one".into()]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(!e.worker.is_loaded(), "the thread outlived its idle period");
        e.embed_raw(&["two".into()]).await.unwrap();
        assert!(e.worker.is_loaded());
    }

    use crate::infer::Reranker;

    #[tokio::test]
    async fn the_passage_that_answers_comes_first_with_its_own_index() {
        let Some(path) = model("rerank.gguf") else {
            return;
        };
        let r = LocalReranker::new(path, 512);
        let docs = vec![
            "Bread needs flour, water, salt and time.".to_string(),
            "The invoice for the tax adviser is in the blue folder.".to_string(),
            "Trains to Hamburg leave from platform 7.".to_string(),
        ];
        let got = r
            .rerank("where is the invoice for the tax adviser", &docs, 2)
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, 1);
        assert!(got[0].1 > got[1].1);
    }

    use crate::infer::{Completer, Delta};

    fn split(thinking: bool, pieces: &[&str]) -> (String, String) {
        let mut s = ThinkSplit::new(thinking);
        let mut parts: Vec<_> = pieces.iter().flat_map(|p| s.feed(p)).collect();
        parts.extend(s.finish());
        let of = |want: bool| {
            parts
                .iter()
                .filter(|(t, _)| *t == want)
                .map(|(_, p)| p.as_str())
                .collect::<String>()
        };
        (of(true), of(false))
    }

    #[test]
    fn thinking_is_parted_from_the_answer_even_when_a_tag_is_cut_in_two() {
        assert_eq!(
            split(false, &["<thi", "nk>hm, ", "two</th", "ink>\n\nFour."]),
            ("hm, two".into(), "\n\nFour.".into())
        );
    }

    #[test]
    fn a_template_that_opened_the_thinking_needs_only_the_closing_tag() {
        assert_eq!(
            split(true, &["hm</think>Four."]),
            ("hm".into(), "Four.".into())
        );
    }

    #[test]
    fn an_answer_with_no_thinking_and_a_stray_bracket_comes_through_whole() {
        assert_eq!(
            split(false, &["a <", "b and 1 <t"]),
            (String::new(), "a <b and 1 <t".into())
        );
    }

    fn ask_role() -> crate::config::AskRole {
        let mut role = crate::config::Config::test_default()
            .infer
            .ask
            .expect("the test config asks");
        role.context_tokens = 2048;
        role.max_output_tokens = 64;
        role
    }

    #[tokio::test]
    async fn it_answers_from_what_it_was_shown_and_streams_the_same_words() {
        let Some(path) = model("ask.gguf") else {
            return;
        };
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
            if let Delta::Token(t) = d {
                streamed.push_str(&t)
            }
        }
        assert_eq!(streamed, done.text);
    }

    #[tokio::test]
    async fn a_model_that_thinks_aloud_answers_without_its_thinking() {
        let Some(path) = model("ask.gguf") else {
            return;
        };
        // Thinking is billed against the ceiling; sixty-four tokens leave none for the answer.
        let mut role = ask_role();
        role.max_output_tokens = 1024;
        let c = LocalCompleter::new(path, &role);
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let done = c
            .answer_streaming(
                "Answer in one short sentence, using only the note.",
                "Note: the invoice is in the blue folder.\nQuestion: where is the invoice?",
                1024,
                tx,
            )
            .await
            .unwrap();
        assert!(!done.text.contains("think>"), "{}", done.text);
        assert!(done.text.to_lowercase().contains("blue"), "{}", done.text);
        let mut streamed = String::new();
        while let Some(d) = rx.recv().await {
            if let Delta::Token(t) = d {
                streamed.push_str(&t)
            }
        }
        assert_eq!(streamed, done.text);
    }

    #[tokio::test]
    async fn a_ceiling_reached_is_reported_as_truncated() {
        let Some(path) = model("ask.gguf") else {
            return;
        };
        let c = LocalCompleter::new(path, &ask_role());
        let done = c
            .answer(
                "You are verbose.",
                "Count from one to five hundred in words.",
                8,
            )
            .await
            .unwrap();
        assert!(done.truncated);
    }

    #[tokio::test]
    async fn a_prompt_larger_than_the_window_is_refused_in_words() {
        let Some(path) = model("ask.gguf") else {
            return;
        };
        let c = LocalCompleter::new(path, &ask_role());
        let err = c
            .complete("s", &"word ".repeat(5_000))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("2048"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recording_becomes_its_words() {
        use crate::infer::Transcriber;
        let (Some(model), Some(wav)) = (model("speech.bin"), model("speech.wav")) else {
            return;
        };
        let t = LocalTranscriber::new(model, None);
        let words = t
            .transcribe(&std::fs::read(wav).unwrap(), "audio/wav")
            .await
            .unwrap()
            .to_lowercase();
        assert!(words.contains("ask not what your country"), "{words}");
    }

    #[tokio::test]
    async fn silence_of_no_length_is_no_words_and_no_model_is_loaded_for_it() {
        use crate::infer::Transcriber;
        let t = LocalTranscriber::new("/nowhere/speech.bin".into(), None);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        hound::WavWriter::new(&mut bytes, spec)
            .unwrap()
            .finalize()
            .unwrap();
        assert_eq!(
            t.transcribe(&bytes.into_inner(), "audio/wav")
                .await
                .unwrap(),
            ""
        );
    }
}
