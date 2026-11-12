use super::{
    Completer, Describer, Embedder, Judgement, ProposedArtifact, Reranker, SegmentInput,
    SegmentReply, SynthesisBudget, Synthesizer, Transcriber,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Context is zero so a test that did not ask for context cannot be moved by it.
pub const FAKE_BUDGET: SynthesisBudget = SynthesisBudget {
    context_tokens: 4096,
    max_output_tokens: 1024,
    output_ratio: 1.4,
    context: crate::infer::context::ContextBudget {
        opening: 0,
        overlap: 0,
        neighbors: 0,
    },
};

/// Identical text, identical unit vector. Legacy templates by default, so `"title\ntext"` finds its seeded document.
pub struct FakeEmbedder {
    dim: usize,
    templates: crate::config::EmbedTemplates,
    /// Batching is invisible in the output; only the call count shows it.
    calls: std::sync::atomic::AtomicUsize,
    sent: std::sync::Mutex<Vec<String>>,
    /// The endpoint's "no", which a worker must not retry.
    reject_with: Option<String>,
    /// Calls answered "too many requests" first: the one answer asking again can fix.
    busy_for: std::sync::atomic::AtomicUsize,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self::with_templates(dim, crate::config::EmbedTemplates::legacy())
    }

    pub fn with_templates(dim: usize, templates: crate::config::EmbedTemplates) -> Self {
        Self {
            dim,
            templates,
            calls: std::sync::atomic::AtomicUsize::new(0),
            sent: std::sync::Mutex::new(Vec::new()),
            reject_with: None,
            busy_for: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn rejecting(msg: &str) -> Self {
        let mut e = Self::new(8);
        e.reject_with = Some(msg.to_string());
        e
    }

    pub fn busy_for_the_first(self, n: usize) -> Self {
        self.busy_for.store(n, std::sync::atomic::Ordering::Relaxed);
        self
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn sent(&self) -> Vec<String> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut s) = self.sent.lock() {
            s.extend(texts.iter().cloned());
        }
        if let Some(m) = &self.reject_with {
            return Err(Error::InferenceRejected {
                role: "embed",
                detail: m.clone(),
            });
        }
        // Counted before refusing: the request reached the limiter. `try_update` is newer than the MSRV.
        #[allow(deprecated)]
        let refused = self
            .busy_for
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |n| n.checked_sub(1),
            )
            .is_ok();
        if refused {
            return Err(Error::InferenceBusy {
                role: "embed",
                detail: "HTTP 429 Too Many Requests".into(),
            });
        }
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; self.dim];
                let mut seed = Sha256::digest(t.as_bytes()).to_vec();
                for i in 0..self.dim {
                    if i % 32 == 0 && i > 0 {
                        seed = Sha256::digest(&seed).to_vec();
                    }
                    v[i] = (seed[i % 32] as f32 - 128.0) / 128.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
    fn templates(&self) -> &crate::config::EmbedTemplates {
        &self.templates
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn model(&self) -> &str {
        "fake-embed"
    }
    fn max_input_tokens(&self) -> usize {
        8192
    }
}

#[derive(Default)]
pub struct FakeSynthesizer {
    fail_with: Option<String>,
    /// Distinct from `fail_with`, which never answers at all.
    unparsable_on_marker: Option<String>,
    /// The endpoint's "no" rather than its "not now".
    reject: bool,
}

impl FakeSynthesizer {
    pub fn failing(msg: &str) -> Self {
        Self {
            fail_with: Some(msg.to_string()),
            unparsable_on_marker: None,
            reject: false,
        }
    }

    /// The 400 shape: asking again sends the same request.
    pub fn rejecting(msg: &str) -> Self {
        let mut s = Self::failing(msg);
        s.reject = true;
        s
    }

    /// A property of the window's text, as a duplicate JSON key is, not of the endpoint.
    pub fn unparsable_on(marker: &str) -> Self {
        Self {
            fail_with: None,
            unparsable_on_marker: Some(marker.to_string()),
            reject: false,
        }
    }

    fn refusal(&self, role: &'static str, detail: String) -> Error {
        if self.reject {
            Error::InferenceRejected { role, detail }
        } else {
            Error::Inference { role, detail }
        }
    }
}

#[async_trait]
impl Synthesizer for FakeSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let text = input.core;
        if let Some(marker) = &self.unparsable_on_marker
            && text.contains(marker.as_str())
        {
            return Err(Error::MalformedLlmOutput(
                "duplicate field `tags` at line 1 column 630".into(),
            ));
        }
        if let Some(m) = &self.fail_with {
            return Err(self.refusal("chunk", m.clone()));
        }
        Ok(text
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .enumerate()
            .map(|(i, p)| ProposedArtifact {
                text: p.to_string(),
                title: Some(format!("chunk {i}")),
                category: Some("reference".into()),
                tags: vec!["fake".into()],
                corpus_lines: None,
                caveats: vec![],
                pinned: false,
            })
            .collect())
    }
    /// The door's forced intent, else "none", with no date and no links.
    async fn segment_judged(&self, input: SegmentInput<'_>) -> Result<crate::infer::SegmentReply> {
        let judgement = input.judge.map(|j| crate::infer::Judgement {
            intent: Some(j.forced_intent.clone().unwrap_or_else(|| "none".into())),
            when: None,
            rule: None,
            events: vec![],
            links: vec![],
        });
        Ok(crate::infer::SegmentReply {
            artifacts: self.segment(input).await?,
            judgement,
        })
    }

    fn budget(&self) -> SynthesisBudget {
        FAKE_BUDGET
    }

    /// A configured failure applies here too: naming is a model call the caller must survive.
    async fn title(
        &self,
        text: &str,
        _artifact_titles: &[String],
        _lang: crate::infer::lang::Lang,
    ) -> Result<Option<String>> {
        if let Some(m) = &self.fail_with {
            return Err(self.refusal("title", m.clone()));
        }
        let first: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(40)
            .collect();
        Ok(Some(format!("Fake title: {}", first.trim())))
    }
}

/// A one-off paraphrase that the retry gets right.
pub struct ParaphrasingSynthesizer {
    drop_token: String,
    calls: std::sync::atomic::AtomicUsize,
    persistent: bool,
}

impl ParaphrasingSynthesizer {
    pub fn new(drop_token: &str, persistent: bool) -> Self {
        Self {
            drop_token: drop_token.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            persistent,
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl Synthesizer for ParaphrasingSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let text = input.core;
        let n = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = if self.persistent || n == 0 {
            text.replace(&self.drop_token, "")
        } else {
            text.to_string()
        };
        Ok(vec![ProposedArtifact {
            text: body,
            title: Some("paraphrased".into()),
            category: Some("reference".into()),
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
            pinned: false,
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        FAKE_BUDGET
    }
}

/// The retry fixes the literals and drops the JUDGE block, which the merge must treat as ordinary, not a retraction.
pub struct JudgingParaphraser {
    drop_token: String,
    judgement: Judgement,
    empty_on_retry: bool,
    calls: std::sync::atomic::AtomicUsize,
}

impl JudgingParaphraser {
    pub fn new(drop_token: &str, judgement: Judgement) -> Self {
        Self {
            drop_token: drop_token.to_string(),
            judgement,
            empty_on_retry: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// `Some(Judgement::default())` is what a retry omitting the JUDGE block parses to, since its fields default.
    pub fn answering_an_empty_judgement_on_retry(mut self) -> Self {
        self.empty_on_retry = true;
        self
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl Synthesizer for JudgingParaphraser {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        Ok(self.segment_judged(input).await?.artifacts)
    }

    async fn segment_judged(&self, input: SegmentInput<'_>) -> Result<SegmentReply> {
        let first = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            == 0;
        let text = if first {
            input.core.replace(&self.drop_token, "")
        } else {
            input.core.to_string()
        };
        Ok(SegmentReply {
            artifacts: vec![ProposedArtifact {
                text,
                title: Some("read".into()),
                category: Some("reference".into()),
                tags: vec![],
                corpus_lines: None,
                caveats: vec![],
                pinned: false,
            }],
            judgement: if first {
                Some(self.judgement.clone())
            } else {
                self.empty_on_retry.then(Judgement::default)
            },
        })
    }

    fn budget(&self) -> SynthesisBudget {
        FAKE_BUDGET
    }
}

/// A small model that ignores the instruction not to extract from context.
pub struct GreedySynthesizer {
    pub budget: SynthesisBudget,
}

#[async_trait]
impl Synthesizer for GreedySynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let mut out: Vec<ProposedArtifact> = Vec::new();
        let from_context = input.context.blocks().flat_map(|b| b.lines());
        for line in input.core.lines().chain(from_context) {
            if line.trim().is_empty() {
                continue;
            }
            out.push(ProposedArtifact {
                text: line.to_string(),
                title: Some("greedy".into()),
                category: Some("reference".into()),
                tags: vec![],
                corpus_lines: None,
                caveats: vec![],
                pinned: false,
            });
        }
        Ok(out)
    }
    fn budget(&self) -> SynthesisBudget {
        self.budget
    }
}

/// So a test asserts what the window was given, not what it was supposed to be given.
pub struct RecordingSynthesizer {
    pub seen: std::sync::Mutex<Vec<(String, crate::infer::context::WindowContext)>>,
    pub budget: SynthesisBudget,
}

impl RecordingSynthesizer {
    pub fn new(budget: SynthesisBudget) -> Self {
        Self {
            seen: std::sync::Mutex::new(Vec::new()),
            budget,
        }
    }
}

#[async_trait]
impl Synthesizer for RecordingSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((input.core.to_string(), input.context.clone()));
        Ok(vec![ProposedArtifact {
            text: input.core.lines().next().unwrap_or("empty").to_string(),
            title: Some("recorded".into()),
            category: Some("reference".into()),
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
            pinned: false,
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        self.budget
    }
}

/// The span check exists because model line numbers are taken on trust. `echo_text` makes the body recoverable.
pub struct MisreportingSynthesizer {
    pub echo_text: bool,
}

#[async_trait]
impl Synthesizer for MisreportingSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let (text, title) = if self.echo_text {
            (input.core.to_string(), "mislabelled")
        } else {
            (
                "Entirely invented material about unrelated subjects".to_string(),
                "invented",
            )
        };
        Ok(vec![ProposedArtifact {
            text,
            title: Some(title.into()),
            category: None,
            tags: vec![],
            corpus_lines: Some((9_000, 9_100)),
            caveats: vec![],
            pinned: false,
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        FAKE_BUDGET
    }
}

/// Refuses any input over `limit` tokens with llama.cpp's physical-batch message, which the ceiling cannot see.
pub struct StrictEmbedder {
    inner: FakeEmbedder,
    limit: usize,
    /// A 413 classified as a rejection; a worker that understands only one form stops splitting.
    over_http: bool,
}

impl StrictEmbedder {
    pub fn new(dim: usize, limit: usize) -> Self {
        Self {
            inner: FakeEmbedder::new(dim),
            limit,
            over_http: false,
        }
    }
    pub fn over_http(dim: usize, limit: usize) -> Self {
        let mut e = Self::new(dim, limit);
        e.over_http = true;
        e
    }
    pub fn calls(&self) -> usize {
        self.inner.calls()
    }
}

#[async_trait]
impl Embedder for StrictEmbedder {
    async fn embed_raw(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        for t in texts {
            // The budget code's crude estimate, so the test needs no tokenizer.
            let tokens = t.len() / 4;
            if tokens > self.limit {
                let detail = if self.over_http {
                    format!(
                        "HTTP 413 Payload Too Large: input ({tokens} tokens) is too large \
                         to process (limit {})",
                        self.limit
                    )
                } else {
                    format!(
                        "input ({tokens} tokens) is too large to process. increase the \
                         physical batch size (current batch size: {})",
                        self.limit
                    )
                };
                return Err(if self.over_http {
                    Error::InferenceRejected {
                        role: "embed",
                        detail,
                    }
                } else {
                    Error::Inference {
                        role: "embed",
                        detail,
                    }
                });
            }
        }
        self.inner.embed_raw(texts).await
    }
    fn templates(&self) -> &crate::config::EmbedTemplates {
        self.inner.templates()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn model(&self) -> &str {
        "strict-embed"
    }
    fn max_input_tokens(&self) -> usize {
        // Deliberately a lie, as a misconfigured deployment is.
        8192
    }
}

#[derive(Default)]
pub struct FakeReranker {
    /// A reranker can only promote what it is given, so over-fetching tests look here.
    saw: std::sync::atomic::AtomicUsize,
    calls: std::sync::atomic::AtomicUsize,
}

impl FakeReranker {
    pub fn docs_seen(&self) -> usize {
        self.saw.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Reranker for FakeReranker {
    async fn rerank(
        &self,
        _query: &str,
        docs: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f32)>> {
        self.saw
            .store(docs.len(), std::sync::atomic::Ordering::SeqCst);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut out: Vec<(usize, f32)> = (0..docs.len()).map(|i| (i, i as f32)).collect();
        out.reverse();
        out.truncate(top_n);
        Ok(out)
    }
}

/// Vector order and no claim of refinement is the behavior under test.
#[derive(Default)]
pub struct FailingReranker;

#[async_trait]
impl Reranker for FailingReranker {
    async fn rerank(
        &self,
        _query: &str,
        _docs: &[String],
        _top_n: usize,
    ) -> Result<Vec<(usize, f32)>> {
        Err(Error::Inference {
            role: "rerank",
            detail: "endpoint is down".into(),
        })
    }
}

/// `None` echoes the user prompt, making the prompt the thing under test.
pub struct FakeCompleter {
    pub reply: Option<String>,
}

impl Default for FakeCompleter {
    fn default() -> Self {
        Self {
            reply: Some("fake answer".into()),
        }
    }
}

#[async_trait]
impl Completer for FakeCompleter {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        Ok(self.reply.clone().unwrap_or_else(|| user.to_string()))
    }
    fn context_tokens(&self) -> usize {
        4096
    }
    fn max_output_tokens(&self) -> usize {
        1024
    }
}

/// Consolidation tests assert on not calling the model, so the call count matters as much as the reply.
pub struct ScriptedCompleter {
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    calls: std::sync::atomic::AtomicUsize,
    /// The dedupe unit's trimming is visible nowhere else.
    prompts: std::sync::Mutex<Vec<String>>,
    /// So a test can make the window binding without a megabyte of text.
    context_tokens: std::sync::atomic::AtomicUsize,
    /// `InferenceRejected`, never worth repeating.
    rejects: Option<String>,
}

impl ScriptedCompleter {
    pub fn new(replies: Vec<String>) -> Self {
        Self {
            replies: std::sync::Mutex::new(replies.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
            context_tokens: std::sync::atomic::AtomicUsize::new(4096),
            rejects: None,
        }
    }

    /// Distinct from an exhausted script, which did not answer.
    pub fn rejecting(msg: &str) -> Self {
        let mut s = Self::new(Vec::new());
        s.rejects = Some(msg.to_string());
        s
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn set_context_tokens(&self, n: usize) {
        self.context_tokens
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl Completer for ScriptedCompleter {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(user.to_string());
        if let Some(detail) = &self.rejects {
            return Err(crate::error::Error::InferenceRejected {
                role: "ask",
                detail: detail.clone(),
            });
        }
        self.replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| crate::error::Error::Inference {
                role: "ask",
                detail: "the script ran out of replies".into(),
            })
    }
    fn context_tokens(&self) -> usize {
        self.context_tokens
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    fn max_output_tokens(&self) -> usize {
        1024
    }
}

pub struct FakeDescriber {
    pub reply: String,
    pub fail_with: Option<String>,
    /// The endpoint's "no" rather than its "not now".
    pub reject: bool,
    calls: std::sync::atomic::AtomicUsize,
    last_context: std::sync::Mutex<String>,
}

impl Default for FakeDescriber {
    fn default() -> Self {
        Self::saying("# Photo\n\nA whiteboard listing three tasks: ship, test, rest.")
    }
}

impl FakeDescriber {
    pub fn saying(reply: &str) -> Self {
        Self {
            reply: reply.into(),
            fail_with: None,
            reject: false,
            calls: Default::default(),
            last_context: Default::default(),
        }
    }
    pub fn failing(msg: &str) -> Self {
        let mut d = Self::saying("");
        d.fail_with = Some(msg.into());
        d
    }
    /// What a non-multimodal model answers, and a worker must not retry.
    pub fn rejecting(msg: &str) -> Self {
        let mut d = Self::failing(msg);
        d.reject = true;
        d
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn last_context(&self) -> String {
        self.last_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Describer for FakeDescriber {
    async fn describe(&self, _image_jpeg: &[u8], context: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self
            .last_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = context.to_string();
        match &self.fail_with {
            Some(m) if self.reject => Err(Error::InferenceRejected {
                role: "vision",
                detail: m.clone(),
            }),
            Some(m) => Err(Error::Inference {
                role: "vision",
                detail: m.clone(),
            }),
            None => Ok(self.reply.clone()),
        }
    }
}

pub struct FakeTranscriber {
    pub reply: String,
    pub fail_with: Option<String>,
    calls: std::sync::atomic::AtomicUsize,
    last_mime: std::sync::Mutex<String>,
    last_len: std::sync::atomic::AtomicUsize,
}

impl Default for FakeTranscriber {
    fn default() -> Self {
        Self::saying("the thing I said out loud")
    }
}

impl FakeTranscriber {
    pub fn saying(reply: &str) -> Self {
        Self {
            reply: reply.into(),
            fail_with: None,
            calls: Default::default(),
            last_mime: Default::default(),
            last_len: Default::default(),
        }
    }
    pub fn failing(msg: &str) -> Self {
        let mut t = Self::saying("");
        t.fail_with = Some(msg.into());
        t
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn last_mime(&self) -> String {
        self.last_mime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn last_len(&self) -> usize {
        self.last_len.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Transcriber for FakeTranscriber {
    async fn transcribe(&self, audio: &[u8], mime: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self
            .last_mime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mime.to_string();
        self.last_len
            .store(audio.len(), std::sync::atomic::Ordering::SeqCst);
        match &self.fail_with {
            Some(m) => Err(Error::Inference {
                role: "transcribe",
                detail: m.clone(),
            }),
            None => Ok(self.reply.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_completer_without_an_override_streams_its_whole_answer_as_one_delta() {
        let c = FakeCompleter {
            reply: Some("the answer".into()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let done = c.answer_streaming("sys", "usr", 128, tx).await.unwrap();
        let mut got = String::new();
        while let Some(d) = rx.recv().await {
            if let crate::infer::Delta::Token(t) = d {
                got.push_str(&t);
            }
        }
        assert_eq!(got, "the answer");
        assert_eq!(done.text, "the answer");
    }

    #[tokio::test]
    async fn a_dropped_receiver_neither_fails_the_call_nor_truncates_its_answer() {
        let c = FakeCompleter {
            reply: Some("the answer".into()),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        drop(rx);
        let done = c.answer_streaming("sys", "usr", 128, tx).await.unwrap();
        assert_eq!(done.text, "the answer");
    }

    use crate::config::EmbedTemplates;
    use crate::infer::EmbedDoc;

    #[tokio::test]
    async fn the_fake_renders_like_the_real_one_and_keeps_what_it_sent() {
        let e = FakeEmbedder::with_templates(8, EmbedTemplates::default());
        let d = e
            .embed_documents(&[EmbedDoc {
                title: None,
                text: "alpha".into(),
            }])
            .await
            .unwrap();
        let q = e.embed_query("alpha").await.unwrap();
        assert_ne!(d[0], q);
        assert_eq!(
            e.sent(),
            vec![
                "title: none | text: alpha".to_string(),
                "task: search result | query: alpha".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn the_default_fake_is_symmetric_so_a_query_can_name_a_document() {
        let e = FakeEmbedder::new(8);
        let d = e
            .embed_documents(&[EmbedDoc {
                title: Some("t0".into()),
                text: "alpha".into(),
            }])
            .await
            .unwrap();
        let q = e.embed_query("t0\nalpha").await.unwrap();
        assert_eq!(d[0], q);
    }
}
