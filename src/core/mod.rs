pub mod ask;
pub mod background;
pub mod ingest;
pub mod search;

use crate::infer::budget::TokenCounter;
use crate::infer::{Chunker, Completer, Embedder, Reranker};
use crate::store::Store;
use crate::vector::VectorStore;
use background::Background;
use std::sync::Arc;

/// Entries kept in the query embedding cache.
pub const QUERY_CACHE_CAPACITY: usize = 256;

/// Bounded cache of query embeddings.
///
/// Search-as-you-type asks for `d`, `dd`, `dd i`, `dd if` inside one search,
/// and the same questions come back across sessions. Each of those is a remote
/// call before the vector store is touched at all, which for a local embedder
/// is the dominant term in the latency the user feels.
///
/// Insertion-ordered rather than a true LRU: at this size the difference does
/// not pay for the bookkeeping.
pub struct QueryCache {
    capacity: usize,
    entries: std::collections::VecDeque<(String, Vec<f32>)>,
}

impl QueryCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: std::collections::VecDeque::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Vec<f32>> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    pub fn put(&mut self, key: String, value: Vec<f32>) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }
}

#[derive(Clone)]
pub struct Core {
    pub store: Store,
    pub vectors: Arc<dyn VectorStore>,
    pub chunker: Arc<dyn Chunker>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    pub completer: Arc<dyn Completer>,
    pub counter: Arc<TokenCounter>,
    /// Writes that run off the request path. Shared by every clone of `Core`,
    /// so draining one drains them all.
    pub background: Arc<Background>,
    /// Shared by every clone of `Core`, like the background queue.
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
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
            background: Arc::new(Background::default()),
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
        }
    }
}
