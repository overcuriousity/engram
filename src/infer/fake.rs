use super::{
    Completer, Embedder, ProposedArtifact, Reranker, SegmentInput, SynthesisBudget, Synthesizer,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Hashes text into a fixed-dimension unit vector. Identical text gives an
/// identical vector and different text gives a different one, which is all the
/// retrieval tests need from an embedding model.
pub struct FakeEmbedder {
    dim: usize,
    /// How many times the endpoint was called. Batching is invisible in the
    /// output — only the call count shows whether it happened.
    calls: std::sync::atomic::AtomicUsize,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    /// Fail only on windows containing this marker. Lets a test model the
    /// realistic case — some windows succeed, one does not.
    fail_on_marker: Option<String>,
}

impl FakeSynthesizer {
    pub fn failing(msg: &str) -> Self {
        Self {
            fail_with: Some(msg.to_string()),
            fail_on_marker: None,
        }
    }

    pub fn failing_on(marker: &str) -> Self {
        Self {
            fail_with: None,
            fail_on_marker: Some(marker.to_string()),
        }
    }
}

#[async_trait]
impl Synthesizer for FakeSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let text = input.core;
        if let Some(marker) = &self.fail_on_marker
            && text.contains(marker.as_str())
        {
            return Err(Error::Inference {
                role: "chunk",
                detail: format!("refusing window containing {marker}"),
            });
        }
        if let Some(m) = &self.fail_with {
            return Err(Error::Inference {
                role: "chunk",
                detail: m.clone(),
            });
        }
        Ok(text
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .enumerate()
            .map(|(i, p)| ProposedArtifact {
                text: p.to_string(),
                title: Some(format!("chunk {i}")),
                category: Some("note".into()),
                tags: vec!["fake".into()],
                corpus_lines: None,
                caveats: vec![],
            })
            .collect())
    }
    fn budget(&self) -> SynthesisBudget {
        SynthesisBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
            // Zero on purpose: a fake reproduces today's windowing exactly, so
            // a test that did not ask for context cannot be moved by it.
            context: Default::default(),
        }
    }

    /// Deterministic and obviously synthetic, so a test can assert on it. A
    /// configured failure applies here too: naming is a model call like any
    /// other, and the caller has to survive it failing.
    async fn title(&self, text: &str, _artifact_titles: &[String]) -> Result<Option<String>> {
        if let Some(m) = &self.fail_with {
            return Err(Error::Inference {
                role: "title",
                detail: m.clone(),
            });
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

/// Drops a token from the first window it sees and reproduces it faithfully
/// afterwards. Models the case the retry exists for: a one-off paraphrase that
/// a second attempt gets right.
pub struct ParaphrasingSynthesizer {
    drop_token: String,
    calls: std::sync::atomic::AtomicUsize,
    /// Keep paraphrasing forever rather than recovering on the retry.
    persistent: bool,
}

impl ParaphrasingSynthesizer {
    pub fn recovering(drop_token: &str) -> Self {
        Self {
            drop_token: drop_token.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            persistent: false,
        }
    }

    pub fn persistent(drop_token: &str) -> Self {
        Self {
            drop_token: drop_token.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            persistent: true,
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
            category: Some("note".into()),
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        SynthesisBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
            // Zero on purpose: a fake reproduces today's windowing exactly, so
            // a test that did not ask for context cannot be moved by it.
            context: Default::default(),
        }
    }
}

/// Segments like `FakeSynthesizer` but reports a cooldown, so a test can assert
/// the job actually paces itself instead of running flat out.
pub struct PacedSynthesizer {
    inner: FakeSynthesizer,
    cooldown: std::time::Duration,
}

impl PacedSynthesizer {
    pub fn new(cooldown: std::time::Duration) -> Self {
        Self {
            inner: FakeSynthesizer::default(),
            cooldown,
        }
    }
}

#[async_trait]
impl Synthesizer for PacedSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        self.inner.segment(input).await
    }
    fn budget(&self) -> SynthesisBudget {
        self.inner.budget()
    }
    fn cooldown(&self) -> std::time::Duration {
        self.cooldown
    }
}

/// Claims every chunk came from lines far outside its window. The span check
/// exists because the model's line numbers are taken on trust.
#[derive(Default)]
pub struct LyingSpanSynthesizer;

#[async_trait]
impl Synthesizer for LyingSpanSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let text = input.core;
        Ok(vec![ProposedArtifact {
            text: text.to_string(),
            title: Some("mislabelled".into()),
            category: None,
            tags: vec![],
            corpus_lines: Some((9_000, 9_100)),
            caveats: vec![],
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        SynthesisBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
            // Zero on purpose: a fake reproduces today's windowing exactly, so
            // a test that did not ask for context cannot be moved by it.
            context: Default::default(),
        }
    }
}

/// Returns text that appears nowhere in its window, with a bogus span. The
/// case where nothing can be recovered and the reader has to be told.
#[derive(Default)]
pub struct HallucinatingSynthesizer;

#[async_trait]
impl Synthesizer for HallucinatingSynthesizer {
    async fn segment(&self, _input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        Ok(vec![ProposedArtifact {
            text: "Entirely invented material about unrelated subjects".into(),
            title: Some("invented".into()),
            category: None,
            tags: vec![],
            corpus_lines: Some((9_000, 9_100)),
            caveats: vec![],
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        SynthesisBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
            // Zero on purpose: a fake reproduces today's windowing exactly, so
            // a test that did not ask for context cannot be moved by it.
            context: Default::default(),
        }
    }
}

/// Reverses the candidate order. Deliberately not identity: a test asserting
/// rerank ran can only tell the difference if the order actually changes.
/// Refuses any single input over `limit` tokens the way llama.cpp does, with
/// its physical-batch message. The configured ceiling cannot see this limit.
pub struct StrictEmbedder {
    inner: FakeEmbedder,
    limit: usize,
}

impl StrictEmbedder {
    pub fn new(dim: usize, limit: usize) -> Self {
        Self {
            inner: FakeEmbedder::new(dim),
            limit,
        }
    }
    pub fn calls(&self) -> usize {
        self.inner.calls()
    }
}

#[async_trait]
impl Embedder for StrictEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        for t in texts {
            // The same crude estimate the budget code uses, so the test does
            // not depend on a tokenizer.
            let tokens = t.len() / 4;
            if tokens > self.limit {
                return Err(Error::Inference {
                    role: "embed",
                    detail: format!(
                        "input ({tokens} tokens) is too large to process. increase the \
                         physical batch size (current batch size: {})",
                        self.limit
                    ),
                });
            }
        }
        self.inner.embed(texts).await
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
    /// How many documents the last call was handed. A reranker can only promote
    /// what it is given, so a test about over-fetching has to look at this
    /// rather than at the answer.
    saw: std::sync::atomic::AtomicUsize,
}

impl FakeReranker {
    pub fn docs_seen(&self) -> usize {
        self.saw.load(std::sync::atomic::Ordering::SeqCst)
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
        let mut out: Vec<(usize, f32)> = (0..docs.len()).map(|i| (i, i as f32)).collect();
        out.reverse();
        out.truncate(top_n);
        Ok(out)
    }
}

pub struct FakeCompleter {
    pub reply: String,
}

impl Default for FakeCompleter {
    fn default() -> Self {
        Self {
            reply: "fake answer".into(),
        }
    }
}

#[async_trait]
impl Completer for FakeCompleter {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        Ok(self.reply.clone())
    }
    fn context_tokens(&self) -> usize {
        4096
    }
}

/// A completer that answers with the prompt it was handed.
///
/// What `ask` puts in front of the model is the whole of what the model can
/// use, and it is otherwise invisible: the reply is a fake, so a test asserting
/// on it proves nothing about the excerpts. This makes the prompt the thing
/// under test.
#[derive(Default)]
pub struct EchoCompleter;

#[async_trait]
impl Completer for EchoCompleter {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        Ok(user.to_string())
    }
    fn context_tokens(&self) -> usize {
        4096
    }
}

/// A completer that answers from a script and counts how often it was asked.
///
/// The consolidation tests are largely about *not* calling the model, so what
/// they assert on is the call count as much as the reply.
pub struct ScriptedCompleter {
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl ScriptedCompleter {
    pub fn new(replies: Vec<String>) -> Self {
        Self {
            replies: std::sync::Mutex::new(replies.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Completer for ScriptedCompleter {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| crate::error::Error::Inference {
                role: "ask",
                detail: "the script ran out of replies".into(),
            })
    }
    fn context_tokens(&self) -> usize {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty context every one of these tests wants: they exercise the
    /// transport and the parser, not the windowing.
    static EMPTY_CONTEXT: crate::infer::context::WindowContext =
        crate::infer::context::WindowContext {
            opening: None,
            before: None,
            after: None,
        };

    fn window(text: &str) -> SegmentInput<'_> {
        SegmentInput {
            core: text,
            context: &EMPTY_CONTEXT,
        }
    }
    use crate::infer::{Embedder, Reranker, Synthesizer};

    #[tokio::test]
    async fn fake_embedder_is_deterministic_and_correctly_sized() {
        let e = FakeEmbedder::new(8);
        let a = e.embed(&["hello".to_string()]).await.unwrap();
        let b = e.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(a, b, "same input must give the same vector");
        assert_eq!(a[0].len(), 8);

        let c = e.embed(&["different".to_string()]).await.unwrap();
        assert_ne!(a[0], c[0]);
    }

    #[tokio::test]
    async fn fake_embedder_vectors_are_normalized() {
        let e = FakeEmbedder::new(16);
        let v = e.embed(&["anything".to_string()]).await.unwrap();
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[tokio::test]
    async fn fake_embedder_batches_stay_in_order() {
        let e = FakeEmbedder::new(8);
        let batch = e
            .embed(&["first".to_string(), "second".to_string()])
            .await
            .unwrap();
        let first_alone = e.embed(&["first".to_string()]).await.unwrap();
        let second_alone = e.embed(&["second".to_string()]).await.unwrap();
        assert_eq!(batch[0], first_alone[0]);
        assert_eq!(batch[1], second_alone[0]);
    }

    #[tokio::test]
    async fn fake_chunker_splits_on_blank_lines() {
        let c = FakeSynthesizer::default();
        let out = c
            .segment(window("first para\n\nsecond para"))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first para");
        assert!(out[0].title.is_some());
    }

    #[tokio::test]
    async fn fake_chunker_can_be_told_to_fail() {
        let c = FakeSynthesizer::failing("endpoint down");
        assert!(matches!(
            c.segment(window("x")).await,
            Err(crate::error::Error::Inference { .. })
        ));
    }

    #[tokio::test]
    async fn fake_reranker_reverses_order_so_tests_can_observe_it() {
        let r = FakeReranker::default();
        let out = r
            .rerank("q", &["a".into(), "b".into(), "c".into()], 2)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 2, "fake ranks last document first");
    }
}
