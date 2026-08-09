pub mod budget;
pub mod fake;
pub mod openai;
pub mod prompt;
pub mod split;
pub mod verify;

use crate::error::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq)]
pub struct ProposedChunk {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub source_lines: Option<(i64, i64)>,
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkBudget {
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
}

#[async_trait]
pub trait Chunker: Send + Sync {
    /// Segment one window of text. Windowing itself is the caller's job.
    async fn segment(&self, text: &str) -> Result<Vec<ProposedChunk>>;
    fn budget(&self) -> ChunkBudget;
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
