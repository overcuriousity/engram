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
use std::path::PathBuf;
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
    Error::Inference {
        role,
        detail: detail.to_string(),
    }
}

/// The big cores, roughly. More threads than that on a phone lands work on
/// the efficiency cores, and the whole batch then waits for the slowest.
fn threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 4) as i32
}

/// Something a model thread can be asked to do, and told it cannot.
trait Job: Send + 'static {
    fn fail(self, role: &'static str, detail: &str);
}

/// What a model's thread runs: build the context, then answer jobs until
/// `next` says there are no more. The context never leaves that thread.
type Serve<J> = dyn Fn(&LlamaModel, &mut dyn FnMut() -> Option<J>) -> Result<()> + Send + Sync;

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
                // Looked at first because the binding asserts the file is
                // there, and a panic would tell the caller only that the
                // thread ended — not which file it could not find.
                let loaded = if path.is_file() {
                    LlamaModel::load_from_file(backend(), &path, &LlamaModelParams::default())
                        .map_err(|e| failed(role, format!("{}: {e}", path.display())))
                } else {
                    Err(failed(
                        role,
                        format!("{}: no such model file", path.display()),
                    ))
                };
                let outcome = loaded.and_then(|model| serve(&model, &mut next));
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
    rx.await
        .unwrap_or_else(|_| Err(failed(role, "the model's thread ended before it answered")))
}

fn context<'m>(
    model: &'m LlamaModel,
    role: &'static str,
    params: LlamaContextParams,
) -> Result<LlamaContext<'m>> {
    model
        .new_context(backend(), params)
        .map_err(|e| failed(role, e))
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
        let worker = Worker::new(
            "embed",
            path,
            EMBED_IDLE,
            move |model, next: &mut dyn FnMut() -> Option<EmbedJob>| {
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
    // Cut, not refused: the callers budget against an estimate, and an
    // estimate that ran a few tokens short must not lose the artifact.
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

/// `[BOS] query [EOS] [SEP] passage [EOS]`, the pair layout llama.cpp's own
/// server builds for a rank-pooled model. The passage gives way when the pair
/// is too long; the query never does.
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
    // The model's own template, from its file: the one it was trained on.
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

    // Near-greedy: an answer over retrieved passages wants the likeliest
    // words, and a little temperature only to step out of a repetition.
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::top_k(20),
        LlamaSampler::temp(0.3),
        LlamaSampler::dist(0),
    ]);
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut text = String::new();
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
        if !piece.is_empty() {
            text.push_str(&piece);
            if let Some(sink) = &job.sink {
                // A reader that went away does not stop the answer; see
                // `Completer::answer_streaming`.
                let _ = sink.blocking_send(Delta::Token(piece));
            }
        }
        batch.clear();
        batch
            .add(token, pos, &[0], true)
            .map_err(|e| failed("ask", e))?;
        ctx.decode(&mut batch).map_err(|e| failed("ask", e))?;
    }
    Ok(Completion { text, truncated })
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

/// Which roles run on this device, by the file each one loads. A role left
/// `None` keeps whatever the configuration gave it — an endpoint, or nothing.
#[derive(Debug, Clone, Default)]
pub struct LocalModels {
    pub embed: Option<PathBuf>,
    pub rerank: Option<PathBuf>,
    pub ask: Option<PathBuf>,
    /// A whisper.cpp model. Where there is one, the microphone's door is open.
    pub speech: Option<PathBuf>,
}

// ---- speech ---------------------------------------------------------------

/// Speech to words on this device, over whisper.cpp.
///
/// The model is loaded for a recording and dropped when the words are out.
/// Dictation is a held button a few times a day, not a stream, and on a phone
/// the memory is worth more than the second a load costs: this is the one
/// model here that does not keep a thread of its own. One recording at a time
/// — two held buttons do not exist, and two loaded models should not either.
pub struct LocalTranscriber {
    path: PathBuf,
    /// An ISO-639-1 code, or `None` to let the model decide from the audio.
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
