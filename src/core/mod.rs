pub mod ask;
pub mod ingest;
pub mod search;

use crate::infer::budget::TokenCounter;
use crate::infer::{Chunker, Completer, Embedder, Reranker};
use crate::store::Store;
use crate::vector::VectorStore;
use std::sync::Arc;

#[derive(Clone)]
pub struct Core {
    pub store: Store,
    pub vectors: Arc<dyn VectorStore>,
    pub chunker: Arc<dyn Chunker>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    pub completer: Arc<dyn Completer>,
    pub counter: Arc<TokenCounter>,
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::infer::fake::{FakeChunker, FakeCompleter, FakeEmbedder, FakeReranker};
    use crate::vector::memory::MemoryVectors;

    pub const TEST_DIM: usize = 8;

    pub async fn test_core() -> Core {
        build(Arc::new(FakeChunker::default()), None).await
    }

    pub async fn test_core_with_failing_chunker() -> Core {
        build(Arc::new(FakeChunker::failing("endpoint down")), None).await
    }

    pub async fn test_core_with_rerank() -> Core {
        build(
            Arc::new(FakeChunker::default()),
            Some(Arc::new(FakeReranker)),
        )
        .await
    }

    /// A core plus a handle on its embedder, for asserting how many times the
    /// endpoint was called rather than only what came back.
    pub async fn test_core_counting_embed_calls() -> (Core, Arc<FakeEmbedder>) {
        let embedder = Arc::new(FakeEmbedder::new(TEST_DIM));
        let mut core = build(Arc::new(FakeChunker::default()), None).await;
        core.embedder = embedder.clone();
        (core, embedder)
    }

    async fn build(chunker: Arc<dyn Chunker>, reranker: Option<Arc<dyn Reranker>>) -> Core {
        let store = Store::memory().await.unwrap();
        Core {
            store,
            vectors: Arc::new(MemoryVectors::new()),
            chunker,
            embedder: Arc::new(FakeEmbedder::new(TEST_DIM)),
            reranker,
            completer: Arc::new(FakeCompleter::default()),
            counter: Arc::new(TokenCounter::Estimate),
        }
    }
}
