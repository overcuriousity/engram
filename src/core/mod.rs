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

/// Live per-document locks, keyed by corpus id. Shared by every clone of
/// `Core`: a per-clone map would lock nothing, the same way a per-clone gate
/// would pace nothing.
pub type CorpusLocks =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

#[derive(Clone)]
pub struct Core {
    pub store: Store,
    pub vectors: Arc<dyn VectorStore>,
    pub synthesizer: Arc<dyn Synthesizer>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    pub completer: Arc<dyn Completer>,
    pub counter: Arc<TokenCounter>,
    /// Writes that run off the request path. Shared by every clone of `Core`,
    /// so draining one drains them all.
    pub background: Arc<Background>,
    /// Shared by every clone of `Core`, like the background queue.
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
    /// Thresholds and budgets for duplicate hygiene. Read on the capture path
    /// and by the sweep, so it lives here rather than being passed down.
    pub consolidate: crate::config::ConsolidateConfig,
    /// Cosine similarity below which a result is reported as only loosely
    /// related. See `VectorConfig::weak_below`.
    pub weak_below: f32,
    /// Whether and how real searches are recorded for later judging. Read on
    /// the search path, so it lives here rather than being threaded down.
    pub feedback: crate::config::FeedbackConfig,
    /// The pacer every inference call passes through. Shared by every clone,
    /// because a per-clone gate would pace nothing: the point is one queue of
    /// calls in front of one GPU.
    pub gate: Arc<crate::infer::gate::InferenceGate>,
    /// One lock per document, so the local writes that rearrange a document
    /// cannot interleave with each other. Reach for it through `corpus_lock`.
    pub corpus_locks: CorpusLocks,
}

impl Core {
    /// Build the running core from configuration. Lives here rather than in
    /// `main`, so the evaluation harness drives exactly the `Core` the binary
    /// does — a benchmark against a differently wired core measures the wrong
    /// program.
    pub fn from_config(cfg: &Config, vectors: Arc<dyn VectorStore>, store: Store) -> Core {
        // Chunk size is capped by what the embedder accepts, with headroom for
        // token-count estimation error.
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
            feedback: cfg.feedback.clone(),
            gate: Arc::new(
                crate::infer::gate::InferenceGate::new(std::time::Duration::from_secs(
                    cfg.pacing.cooldown_secs,
                ))
                .with_breaker(
                    cfg.pacing.breaker_after,
                    std::time::Duration::from_secs(cfg.pacing.breaker_probe_secs),
                ),
            ),
            corpus_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Exclusive access to one document's artifact rows, for as long as the
    /// guard is held.
    ///
    /// Windows became independently schedulable units, which means two workers
    /// can be inside two windows of one document at once — where before, one job
    /// owned the whole thing. The two local writes that rearrange a document are
    /// not safe against each other: `write_segment_artifacts` deletes a window's
    /// artifacts and inserts their replacements, while `finish` renumbers every
    /// ordinal in the document. Interleaved, they leave duplicate or gapped
    /// ordinals and a status that depends on which finished last. It converges on
    /// the next settle, and until then the document is wrong.
    ///
    /// Never hold this across an inference call. The whole point of splitting a
    /// document into units was to stop one window blocking the rest, and a lock
    /// held around `segment` would hand that back — serialised, and for minutes.
    /// Every holder does local SQLite work, which SQLite serialises anyway.
    ///
    /// `tokio::sync::Mutex` because a waiter must yield its thread rather than
    /// park a worker on it.
    pub async fn corpus_lock(&self, corpus_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.corpus_locks.lock().expect("corpus locks");
            // Entries nobody is holding or waiting on are dropped rather than
            // accumulating one per document for the life of the process. A
            // holder's guard owns its `Arc`, and so does a task that has cloned
            // one and not yet locked it, so anything still in use counts above
            // one and survives.
            locks.retain(|_, l| Arc::strong_count(l) > 1);
            Arc::clone(locks.entry(corpus_id.to_string()).or_default())
        };
        lock.lock_owned().await
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

    /// A core plus a handle on its reranker, for asserting how wide the
    /// candidate pool it was handed actually was.
    pub async fn test_core_counting_reranked_docs() -> (Core, Arc<FakeReranker>) {
        let reranker = Arc::new(FakeReranker::default());
        let core = build(Arc::new(FakeSynthesizer::default()), Some(reranker.clone())).await;
        (core, reranker)
    }

    /// A core plus a handle on its embedder, for asserting how many times the
    /// endpoint was called rather than only what came back.
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
            // The fake embedder's vectors are not a semantic space, so a
            // realistic threshold would mark arbitrary results weak and every
            // search test would be asserting against noise. Tests that care
            // about the labelling set it themselves.
            weak_below: 0.0,
            // Off, like the shipped default. The capture tests switch it on.
            feedback: crate::config::FeedbackConfig::default(),
            // No cooldown and no breaker: a test that wants pacing builds its
            // own gate, and every other test would otherwise pay for one.
            gate: Arc::new(crate::infer::gate::InferenceGate::new(
                std::time::Duration::ZERO,
            )),
            corpus_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The one wiring decision `from_config` makes that is not a straight
    /// field copy: rerank is optional, and an absent block must leave search
    /// in vector order rather than defaulting to an endpoint.
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
    async fn one_document_is_locked_at_a_time_and_two_documents_are_not() {
        let core = test_support::test_core().await;

        let held = core.corpus_lock("doc-a").await;
        let core2 = core.clone();
        let blocked = tokio::spawn(async move {
            let _second = core2.corpus_lock("doc-a").await;
        });

        // Another document is a different lock, so it must not be behind this
        // one: two workers segmenting two documents was the point of units.
        let other = core.corpus_lock("doc-b").await;
        drop(other);

        tokio::task::yield_now().await;
        assert!(
            !blocked.is_finished(),
            "two writers were let into one document at once"
        );
        drop(held);
        blocked.await.unwrap();
    }

    #[tokio::test]
    async fn locks_nobody_is_holding_do_not_accumulate() {
        // One entry per document, kept for the life of the process, would be a
        // slow leak on a base that ingests continuously.
        let core = test_support::test_core().await;
        for i in 0..50 {
            drop(core.corpus_lock(&format!("doc-{i}")).await);
        }
        let held = core.corpus_lock("doc-held").await;
        assert_eq!(
            core.corpus_locks.lock().unwrap().len(),
            1,
            "released locks were kept"
        );
        drop(held);
    }

    #[tokio::test]
    async fn rerank_is_wired_only_when_configured() {
        let store = crate::store::Store::memory().await.unwrap();
        let vectors = Arc::new(crate::vector::memory::MemoryVectors::new());

        // `Config` has no `Default`, and adding one just for a test would put
        // a fake endpoint in the type. The committed example file is a real
        // config and costs nothing to read.
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
