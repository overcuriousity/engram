pub mod ask;
pub mod background;
pub mod context;
pub mod extract;
pub mod fetch;
pub mod gaps;
pub mod image;
pub mod ingest;
pub mod pdf;
pub mod recommend;
pub mod search;
pub mod sitting;

use crate::config::Config;
use crate::infer::budget::TokenCounter;
use crate::infer::openai::{
    HttpCompleter, HttpDescriber, HttpEmbedder, HttpReranker, HttpSynthesizer,
};
use crate::infer::{Completer, Describer, Embedder, Reranker, Synthesizer};
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
    /// `None` when `[infer.synthesize]` is not configured — `synthesis = "off"`
    /// with no synthesizer. Every stage that would call it checks before
    /// arming, and `run_claimed` closes a unit that slipped through.
    pub synthesizer: Option<Arc<dyn Synthesizer>>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    /// `None` when `[infer.ask]` is not configured: no ask page, no ask tool.
    pub completer: Option<Arc<dyn Completer>>,
    /// The model that rules on duplicate pairs. Separate from `completer`
    /// because judging is background work on the synthesize endpoint, not an
    /// interactive answer: sharing one endpoint put sweep traffic in front of
    /// the user's question and tuned one model for two unrelated tasks.
    /// `None` with no synthesize role.
    pub judge: Option<Arc<dyn Completer>>,
    /// The model that rules on associative links. Same endpoint and same
    /// settings as `judge`, separate because the response format each judge
    /// sends is part of the completer: a link asked under the duplicate
    /// grammar can only answer with a duplicate verdict. `None` with no
    /// synthesize role.
    pub link_judge: Option<Arc<dyn Completer>>,
    /// The model that names a knowledge gap from the questions in it. Same
    /// endpoint as the judges, its own response shape, background only.
    /// `None` with no synthesize role; gaps are then named by their terms.
    pub gap_namer: Option<Arc<dyn Completer>>,
    /// The model that writes an artifact from a pursuit. Same endpoint as the
    /// judges, its own response shape, background only. `None` with no
    /// synthesize role.
    pub generator: Option<Arc<dyn Completer>>,
    /// The model that says, once, which subjects an answer still lacks — and
    /// with it, whether an ask gets a fanned-out second round of retrieval at
    /// all.
    ///
    /// `None` is the whole of the off switch: there is no completer to call, so
    /// the disabled path cannot cost a call however the ask path is later
    /// edited. It is `Some` whenever `infer.ask.plan` is on, which it is by
    /// default.
    pub planner: Option<Arc<dyn Completer>>,
    /// The vision model, when one is configured. `None` closes the image door.
    pub describer: Option<Arc<dyn Describer>>,
    /// How much inference capture spends. See `SynthesisMode`.
    pub synthesis: crate::config::SynthesisMode,
    /// The window budget when there is no synthesizer to derive one from.
    pub segment_tokens: usize,
    /// Passage size at `off`/`earned`, already clamped to the embedder.
    pub chunk_tokens: usize,
    pub counter: Arc<TokenCounter>,
    /// Writes that run off the request path. Shared by every clone of `Core`,
    /// so draining one drains them all.
    pub background: Arc<Background>,
    /// Where this feature reads the time. `System` in the binary; the
    /// recommendation tests set a fixed one so a seventh Friday at 14:52
    /// exists on demand. Nothing else in the tree reads it.
    pub clock: crate::core::context::Clock,
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
    /// Limits for the upload, link and extension capture paths. Read on the
    /// request path, so it lives here rather than being threaded down.
    pub capture: crate::config::CaptureConfig,
    /// Link learning, priming and association. Read on the search path and by
    /// the sweep, so it lives here rather than being threaded down.
    pub associate: crate::config::AssociateConfig,
    pub activation: crate::config::ActivationConfig,
    /// When a passage has earned its window a synthesis call. See
    /// `PromoteConfig`.
    pub promote: crate::config::PromoteConfig,
    /// Whether and how a run of searches may be written up. See
    /// `PursuitConfig`.
    pub pursuit: crate::config::PursuitConfig,
    /// What the queue does with work nobody is waiting on. Read by the repair
    /// pass, which is where ageing happens.
    pub schedule: crate::config::ScheduleConfig,
    /// Whether the sitting may move a result. Carrying needs no setting.
    pub sitting: crate::config::SittingConfig,
    /// Whether and how the area under the search box is filled. Read by the
    /// sweep and on the page-view path, so it lives here rather than being
    /// threaded down.
    pub recommend: crate::config::RecommendConfig,
    /// Every live sitting, keyed by web session. Shared by every clone of
    /// `Core`, like the background queue — a per-clone map would be a per-clone
    /// working memory, which is no working memory at all.
    pub sittings: Arc<crate::core::sitting::Sittings>,
    /// The pacer every inference call passes through. Shared by every clone,
    /// because a per-clone gate would pace nothing: the point is one queue of
    /// calls in front of one GPU.
    pub gate: Arc<crate::infer::gate::InferenceGate>,
    /// One lock per document, so the local writes that rearrange a document
    /// cannot interleave with each other. Reach for it through `corpus_lock`.
    pub corpus_locks: CorpusLocks,
    /// Serializes every lifecycle transition against the sweep's marker
    /// repair. Each transition is two writes to two stores plus a dirty
    /// marker, none of it atomic; without mutual exclusion the repair can
    /// read a stale row mid-reveal, write the old state back over the new
    /// payload, and the reveal then clears the marker — row active, payload
    /// hidden, and nothing left that would ever notice. Shared by every
    /// clone, like the background queue.
    pub lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
    /// How many uploads may be decoded at once. See
    /// `image::MAX_CONCURRENT_DECODES`; shared by every clone, because a
    /// per-clone permit would bound nothing.
    pub decodes: Arc<tokio::sync::Semaphore>,
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

        let synth = cfg.infer.synthesize.as_ref();
        Core {
            store,
            vectors,
            synthesizer: synth.map(|s| {
                Arc::new(HttpSynthesizer::new(s).with_max_artifact_tokens(max_artifact_tokens))
                    as Arc<dyn Synthesizer>
            }),
            embedder: Arc::new(HttpEmbedder::new(&cfg.infer.embed)),
            reranker: cfg
                .infer
                .rerank
                .as_ref()
                .map(|r| Arc::new(HttpReranker::new(r)) as Arc<dyn Reranker>),
            completer: cfg
                .infer
                .ask
                .as_ref()
                .map(|a| Arc::new(HttpCompleter::new(a)) as Arc<dyn Completer>),
            judge: synth.map(|s| Arc::new(HttpCompleter::for_judging(s)) as Arc<dyn Completer>),
            link_judge: synth
                .map(|s| Arc::new(HttpCompleter::for_link_judging(s)) as Arc<dyn Completer>),
            gap_namer: synth
                .map(|s| Arc::new(HttpCompleter::for_gap_naming(s)) as Arc<dyn Completer>),
            generator: synth
                .map(|s| Arc::new(HttpCompleter::for_generating(s)) as Arc<dyn Completer>),
            planner: cfg.infer.ask.as_ref().and_then(|a| {
                a.plan
                    .then(|| Arc::new(HttpCompleter::for_plan(&a.plan_on())) as Arc<dyn Completer>)
            }),
            describer: cfg
                .infer
                .vision
                .as_ref()
                .map(|v| Arc::new(HttpDescriber::new(v, synth)) as Arc<dyn Describer>),
            synthesis: cfg.infer.synthesis,
            segment_tokens: cfg.infer.segment_tokens,
            chunk_tokens: cfg.infer.embed.effective_chunk_tokens(),
            counter: Arc::new(TokenCounter),
            background: Arc::new(Background::default()),
            clock: crate::core::context::Clock::System,
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            consolidate: cfg.consolidate.clone(),
            weak_below: cfg.vector.weak_below,
            feedback: cfg.feedback.clone(),
            capture: cfg.capture.clone(),
            associate: cfg.associate.clone(),
            activation: cfg.activation.clone(),
            promote: cfg.promote.clone(),
            pursuit: cfg.pursuit.clone(),
            schedule: cfg.schedule.clone(),
            sitting: cfg.sitting.clone(),
            recommend: cfg.recommend.clone(),
            sittings: Arc::new(Default::default()),
            gate: Arc::new(crate::infer::gate::InferenceGate::new(
                std::time::Duration::from_secs(cfg.pacing.cooldown_secs),
            )),
            corpus_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            lifecycle_lock: Arc::new(tokio::sync::Mutex::new(())),
            decodes: Arc::new(tokio::sync::Semaphore::new(
                crate::core::image::MAX_CONCURRENT_DECODES,
            )),
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

    /// Whether the associative layer — links and priming — is actually live,
    /// not merely configured on.
    ///
    /// Links are learned from recorded searches (`search_events`), and
    /// recording queries is a separate privacy decision the operator makes
    /// with `feedback.enabled`. Without recordings there is nothing to learn
    /// from, so `associate.enabled` alone must not let the layer read or
    /// write anything: every site that primes, associates, bumps activation
    /// from a search, or renders "seen together" has to check both flags, or
    /// an install that never opted into `feedback` still has its ranking and
    /// activation quietly touched.
    pub fn associating(&self) -> bool {
        self.associate.enabled && self.feedback.enabled
    }

    /// Is there a synthesizer to call? `false` means no `[infer.synthesize]`:
    /// nothing that needs one is armed, offered, or run.
    pub fn synthesizes(&self) -> bool {
        self.synthesizer.is_some()
    }

    /// Is there an ask model to call? `false` means no `[infer.ask]`: no ask
    /// page, no nav entry, no MCP tool, no `/api/ask`.
    pub fn asks(&self) -> bool {
        self.completer.is_some()
    }

    /// Is the area under the search box filled? `false` means the placeholder
    /// is not rendered, the endpoint records nothing, and the sweep does not
    /// run — one gate, in one place.
    pub fn recommends(&self) -> bool {
        self.recommend.enabled
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::infer::fake::{
        FakeCompleter, FakeDescriber, FakeEmbedder, FakeReranker, FakeSynthesizer,
    };
    use crate::vector::memory::MemoryVectors;

    pub const TEST_DIM: usize = 8;

    pub async fn test_core() -> Core {
        build(Arc::new(FakeSynthesizer::default()), None).await
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

    /// A core whose vision model is the given fake, for asserting what it was
    /// asked and answering what a test needs.
    pub async fn test_core_with_describer(d: Arc<FakeDescriber>) -> Core {
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.describer = Some(d);
        core
    }

    /// The shipped default: no `[infer.vision]`, image door closed.
    pub async fn test_core_without_vision() -> Core {
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.describer = None;
        core
    }

    async fn build(synthesizer: Arc<dyn Synthesizer>, reranker: Option<Arc<dyn Reranker>>) -> Core {
        let store = Store::memory().await.unwrap();
        Core {
            store,
            vectors: Arc::new(MemoryVectors::new()),
            synthesizer: Some(synthesizer),
            embedder: Arc::new(FakeEmbedder::new(TEST_DIM)),
            reranker,
            completer: Some(Arc::new(FakeCompleter::default())),
            judge: Some(Arc::new(FakeCompleter::default())),
            link_judge: Some(Arc::new(FakeCompleter::default())),
            gap_namer: Some(Arc::new(FakeCompleter {
                reply: Some(r#"{"label":"Fake topic"}"#.into()),
            })),
            generator: Some(Arc::new(FakeCompleter::default())),
            // Off, unlike the shipped default: a test that wants a fan-out puts
            // a completer here, and every other test gets one round and no
            // extra call to account for.
            planner: None,
            describer: Some(Arc::new(FakeDescriber::default())),
            synthesis: crate::config::SynthesisMode::Eager,
            segment_tokens: crate::config::DEFAULT_SEGMENT_TOKENS,
            chunk_tokens: crate::config::DEFAULT_CHUNK_TOKENS,
            counter: Arc::new(TokenCounter),
            background: Arc::new(Background::default()),
            clock: crate::core::context::Clock::System,
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            consolidate: crate::config::ConsolidateConfig::default(),
            // The fake embedder's vectors are not a semantic space, so a
            // realistic threshold would mark arbitrary results weak and every
            // search test would be asserting against noise. Tests that care
            // about the labelling set it themselves.
            weak_below: 0.0,
            // Off in tests, whatever ships: the capture tests switch it on and
            // the rest assert nothing is recorded.
            feedback: crate::config::FeedbackConfig {
                enabled: false,
                ..Default::default()
            },
            capture: crate::config::CaptureConfig::default(),
            // On, like the shipped default — and inert in most tests, because
            // nothing has learned a link yet. The association tests seed one.
            associate: crate::config::AssociateConfig::default(),
            activation: crate::config::ActivationConfig::default(),
            promote: crate::config::PromoteConfig::default(),
            pursuit: crate::config::PursuitConfig::default(),
            schedule: crate::config::ScheduleConfig::default(),
            sitting: crate::config::SittingConfig::default(),
            // Off, like the shipped default. The recommendation tests switch it
            // on; every other test asserts nothing is offered and nothing is
            // recorded.
            recommend: crate::config::RecommendConfig::default(),
            sittings: Arc::new(Default::default()),
            // No cooldown: a test that wants pacing builds its
            // own gate, and every other test would otherwise pay for one.
            gate: Arc::new(crate::infer::gate::InferenceGate::new(
                std::time::Duration::ZERO,
            )),
            corpus_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            lifecycle_lock: Arc::new(tokio::sync::Mutex::new(())),
            decodes: Arc::new(tokio::sync::Semaphore::new(
                crate::core::image::MAX_CONCURRENT_DECODES,
            )),
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
        // Autonomy is on: a shipped instance consolidates its own knowledge base
        // without being asked. That is a deliberate choice and the largest risk
        // in the design, and what bounds it is not this flag — it is that a
        // value conflict is escalated rather than settled, that a merge which
        // would drop a value is refused, that the originals are superseded
        // rather than deleted, and that every merge has an undo.
        //
        // What stays bounded here is the *spend*. A rate rather than a
        // per-sweep count, so it does not grow with the base.
        assert!(
            cfg.consolidate.max_dedupe_per_tick > 0 && cfg.consolidate.dedupe_interval_mins > 0,
            "the pass must have a rate, or it either never runs or is unbounded"
        );
    }

    #[tokio::test]
    async fn associating_requires_both_flags_and_the_shipped_default_has_both() {
        // Shipped defaults: `associate.enabled = true`, `feedback.enabled =
        // true` — promotion reads activation, and activation only moves while
        // searches are recorded, so recording is opt-out. The test core keeps
        // feedback off so every other test starts from nothing recorded, and
        // the layer must still stay dark until both are on.
        assert!(crate::config::FeedbackConfig::default().enabled);
        assert!(crate::config::AssociateConfig::default().enabled);
        let mut core = test_support::test_core().await;
        assert!(core.associate.enabled && !core.feedback.enabled);
        assert!(!core.associating(), "on with only associate.enabled set");

        core.feedback.enabled = true;
        assert!(core.associating(), "both flags set");

        core.associate.enabled = false;
        assert!(!core.associating(), "on with only feedback.enabled set");
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

    /// The whole of the off switch. Planning costs one model call on every
    /// question, so `plan = false` must not merely skip the call — there must
    /// be nothing to call, which is what `None` here means. The example config
    /// is the shipped default, and this asserts the default it ships.
    #[tokio::test]
    async fn the_planner_is_wired_by_default_and_unwired_when_switched_off() {
        let store = crate::store::Store::memory().await.unwrap();
        let vectors = Arc::new(crate::vector::memory::MemoryVectors::new());
        let mut cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();

        assert!(
            cfg.infer.ask.as_ref().unwrap().plan,
            "the shipped config does not fan out"
        );
        let core = Core::from_config(&cfg, vectors.clone(), store.clone());
        assert!(core.planner.is_some());

        cfg.infer.ask.as_mut().unwrap().plan = false;
        let core = Core::from_config(&cfg, vectors, store);
        assert!(
            core.planner.is_none(),
            "there is a completer to call with the feature off"
        );
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

    #[tokio::test]
    async fn a_core_without_roles_says_so() {
        let cfg_toml = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer]
        synthesis = "off"
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "embeddinggemma"
        dim = 768
        max_input_tokens = 2048
        [auth]
        mode = "local"
        [auth.local]
        username = "dev"
        password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"
        "#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, cfg_toml).unwrap();
        let cfg = crate::config::Config::load(Some(&path)).unwrap();
        let store = Store::memory().await.unwrap();
        let core = Core::from_config(
            &cfg,
            Arc::new(crate::vector::memory::MemoryVectors::new()),
            store,
        );
        assert!(!core.synthesizes());
        assert!(!core.asks());
        assert!(core.synthesizer.is_none() && core.completer.is_none());
        assert!(core.judge.is_none() && core.link_judge.is_none() && core.gap_namer.is_none());
        assert_eq!(core.synthesis, crate::config::SynthesisMode::Off);
    }
}
