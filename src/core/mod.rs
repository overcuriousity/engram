pub mod ask;
pub mod background;
pub mod ingest;
pub mod search;

use crate::config::Config;
use crate::infer::budget::TokenCounter;
use crate::infer::openai::{HttpCompleter, HttpEmbedder, HttpReranker, HttpSynthesizer};
use crate::infer::{Completer, Embedder, Reranker, Synthesizer};
use crate::store::Store;
use crate::vector::VectorStore;
use background::Background;
use std::sync::Arc;

pub const QUERY_CACHE_CAPACITY: usize = 256;

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
    pub synthesizer: Arc<dyn Synthesizer>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    pub completer: Arc<dyn Completer>,
    pub counter: Arc<TokenCounter>,
    pub background: Arc<Background>,
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
    pub consolidate: crate::config::ConsolidateConfig,
    pub weak_below: f32,
}

impl Core {
    pub fn from_config(cfg: &Config, vectors: Arc<dyn VectorStore>, store: Store) -> Core {
        let max_artifact_tokens = (cfg.infer.embed.max_input_tokens as f32 * 0.8) as usize;

        Core {
            store,
            vectors,
            synthesizer: Arc::new(
                HttpSynthesizer::new(&cfg.infer.synthesize)
                    .with_max_artifact_tokens(max_artifact_tokens),
            ),
            embedder: Arc::new(HttpEmbedder::new(&cfg.infer.embed)),
            reranker: cfg
                .infer
                .rerank
                .as_ref()
                .map(|r| Arc::new(HttpReranker::new(r)) as Arc<dyn Reranker>),
            completer: Arc::new(HttpCompleter::new(&cfg.infer.ask)),
            counter: Arc::new(TokenCounter::load(
                cfg.infer.synthesize.tokenizer_path.as_deref(),
            )),
            background: Arc::new(Background::default()),
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            consolidate: cfg.consolidate.clone(),
            weak_below: cfg.vector.weak_below,
        }
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::infer::fake::{FakeCompleter, FakeEmbedder, FakeReranker, FakeSynthesizer};
    use crate::vector::memory::MemoryVectors;

    pub const TEST_DIM: usize = 8;

    pub async fn test_core() -> Core {
        build(Arc::new(FakeSynthesizer::default()), None).await
    }

    pub async fn test_core_with_failing_synthesizer() -> Core {
        build(Arc::new(FakeSynthesizer::failing("endpoint down")), None).await
    }

    pub async fn test_core_with_rerank() -> Core {
        test_core_counting_reranked_docs().await.0
    }

    pub async fn test_core_counting_reranked_docs() -> (Core, Arc<FakeReranker>) {
        let reranker = Arc::new(FakeReranker::default());
        let core = build(Arc::new(FakeSynthesizer::default()), Some(reranker.clone())).await;
        (core, reranker)
    }

    pub async fn test_core_counting_embed_calls() -> (Core, Arc<FakeEmbedder>) {
        let embedder = Arc::new(FakeEmbedder::new(TEST_DIM));
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.embedder = embedder.clone();
        (core, embedder)
    }

    async fn build(synthesizer: Arc<dyn Synthesizer>, reranker: Option<Arc<dyn Reranker>>) -> Core {
        let store = Store::memory().await.unwrap();
        Core {
            store,
            vectors: Arc::new(MemoryVectors::new()),
            synthesizer,
            embedder: Arc::new(FakeEmbedder::new(TEST_DIM)),
            reranker,
            completer: Arc::new(FakeCompleter::default()),
            counter: Arc::new(TokenCounter::Estimate),
            background: Arc::new(Background::default()),
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            consolidate: crate::config::ConsolidateConfig::default(),
            weak_below: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn the_example_config_carries_the_consolidation_defaults() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(cfg.consolidate.enabled);
        assert!(
            cfg.consolidate.auto_supersede > cfg.consolidate.review_min,
            "superseding at or below the review threshold would hide distinct artifacts"
        );
        assert!(
            !cfg.consolidate.judge,
            "the only inference-costing stage must be opt-in"
        );
    }

    #[tokio::test]
    async fn rerank_is_wired_only_when_configured() {
        let store = crate::store::Store::memory().await.unwrap();
        let vectors = Arc::new(crate::vector::memory::MemoryVectors::new());

        let mut cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(
            cfg.infer.rerank.is_none(),
            "the example config sets no reranker"
        );
        let core = Core::from_config(&cfg, vectors.clone(), store.clone());
        assert!(core.reranker.is_none());

        cfg.infer.rerank = Some(crate::config::RerankRole {
            base_url: "http://localhost:8001".into(),
            model: "bge-reranker-v2-m3".into(),
            api_key: None,
            style: crate::config::RerankStyle::Tei,
            timeout_secs: 60,
        });
        let core = Core::from_config(&cfg, vectors, store);
        assert!(core.reranker.is_some());
    }
}
