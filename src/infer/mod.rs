pub mod budget;
pub mod fake;
pub mod openai;
pub mod prompt;
pub mod split;
pub mod verify;

use crate::error::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq)]
pub struct ProposedArtifact {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_lines: Option<(i64, i64)>,
    /// Conditions under which the artifact does not apply, as the source states
    /// them. The model is already holding this segment, so asking for these
    /// costs output tokens rather than another call.
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SynthesisBudget {
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
}

#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Segment one window of text. Windowing itself is the caller's job.
    async fn segment(&self, text: &str) -> Result<Vec<ProposedArtifact>>;
    fn budget(&self) -> SynthesisBudget;
    /// How long to idle between windows, so a long source is not one
    /// unbroken thermal load on a desktop GPU. Zero for anything remote.
    fn cooldown(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dim(&self) -> usize;
    fn model(&self) -> &str;
    fn max_input_tokens(&self) -> usize;
}

#[async_trait]
pub trait Reranker: Send + Sync {
    /// Returns (original index, score) pairs, best first, at most `top_n`.
    async fn rerank(&self, query: &str, docs: &[String], top_n: usize)
    -> Result<Vec<(usize, f32)>>;
}

#[async_trait]
pub trait Completer: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String>;
    fn context_tokens(&self) -> usize;
}
