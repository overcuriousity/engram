use super::{ChunkBudget, Chunker, Completer, Embedder, ProposedChunk, Reranker};
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
pub struct FakeChunker {
    fail_with: Option<String>,
}

impl FakeChunker {
    pub fn failing(msg: &str) -> Self {
        Self {
            fail_with: Some(msg.to_string()),
        }
    }
}

#[async_trait]
impl Chunker for FakeChunker {
    async fn segment(&self, text: &str) -> Result<Vec<ProposedChunk>> {
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
            .map(|(i, p)| ProposedChunk {
                text: p.to_string(),
                title: Some(format!("chunk {i}")),
                category: Some("note".into()),
                tags: vec!["fake".into()],
                source_lines: None,
            })
            .collect())
    }
    fn budget(&self) -> ChunkBudget {
        ChunkBudget {
            context_tokens: 4096,
            max_output_tokens: 1024,
            output_ratio: 1.4,
        }
    }
}

/// Reverses the candidate order. Deliberately not identity: a test asserting
/// rerank ran can only tell the difference if the order actually changes.
#[derive(Default)]
pub struct FakeReranker;

#[async_trait]
impl Reranker for FakeReranker {
    async fn rerank(
        &self,
        _query: &str,
        docs: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f32)>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::{Chunker, Embedder, Reranker};

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
        let c = FakeChunker::default();
        let out = c.segment("first para\n\nsecond para").await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "first para");
        assert!(out[0].title.is_some());
    }

    #[tokio::test]
    async fn fake_chunker_can_be_told_to_fail() {
        let c = FakeChunker::failing("endpoint down");
        assert!(matches!(
            c.segment("x").await,
            Err(crate::error::Error::Inference { .. })
        ));
    }

    #[tokio::test]
    async fn fake_reranker_reverses_order_so_tests_can_observe_it() {
        let r = FakeReranker;
        let out = r
            .rerank("q", &["a".into(), "b".into(), "c".into()], 2)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 2, "fake ranks last document first");
    }
}
