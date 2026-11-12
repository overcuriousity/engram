pub mod ask;
pub mod background;
pub mod context;
pub mod coverage;
pub mod explain;
pub mod extract;
pub mod fetch;
pub mod gaps;
pub mod image;
pub mod ingest;
pub mod moments;
pub mod pdf;
pub mod ranking;
pub mod recommend;
pub mod search;
pub mod sitting;

use crate::config::Config;
use crate::infer::budget::TokenCounter;
use crate::infer::openai::{
    HttpCompleter, HttpDescriber, HttpEmbedder, HttpReranker, HttpSynthesizer, HttpTranscriber,
};
use crate::infer::{Completer, Describer, Embedder, Reranker, Synthesizer, Transcriber};
use crate::store::Store;
use crate::vector::VectorStore;
use background::Background;
use std::sync::Arc;

/// Entries kept in the query embedding cache.
pub const QUERY_CACHE_CAPACITY: usize = 256;

/// Read back by `adopt_measured_line`, so a restart does not spend an hour at `weak_floor`.
const CALIBRATION_KEY: &str = "calibration.line";

/// Search-as-you-type embeds every prefix; insertion-ordered, since a true LRU does not pay at this
/// size.
pub struct QueryCache {
    capacity: usize,
    entries: std::collections::VecDeque<(String, Vec<f32>)>,
}

/// Per-subject working state, held by `Tenants` and handed back on reopen so eviction stays
/// transparent.
#[derive(Clone)]
pub struct Working {
    pub sittings: Arc<crate::core::sitting::Sittings>,
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
    /// Held here so a transient `Core` (`background::open_for_pass`) shares its measurement with
    /// the serving one.
    pub line: Arc<std::sync::atomic::AtomicU32>,
}

impl Default for Working {
    fn default() -> Self {
        Working {
            sittings: Arc::new(Default::default()),
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            line: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

impl Working {
    /// Lets the registry drop departed subjects; `Sittings` expires its own entries.
    pub fn is_idle(&self) -> bool {
        Arc::strong_count(&self.sittings) == 1 && self.sittings.is_empty()
    }
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

/// Shared by every clone of `Core`: a per-clone map would lock nothing.
pub type CorpusLocks =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

pub const TRASH_RETENTION_SECS: i64 = 30 * 24 * 3600;
pub const UNDO_MS: u32 = 6000;

#[derive(Clone)]
pub struct Core {
    /// Tells open pages that the queue or the idle line moved.
    pub live: crate::web::live::Live,
    pub store: Store,
    pub vectors: Arc<dyn VectorStore>,
    /// `None` without `[infer.synthesize]`: the embed-only floor, where captures stay verbatim
    /// passages.
    pub synthesizer: Option<Arc<dyn Synthesizer>>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    /// `[infer.rerank].apply`; meaningless without a reranker.
    pub rerank_apply: Vec<crate::config::RerankApply>,
    /// `None` when `[infer.ask]` is not configured: no ask page, no ask tool.
    pub completer: Option<Arc<dyn Completer>>,
    /// Rules on duplicate pairs, on the synthesize endpoint so sweep traffic stays off the ask
    /// model.
    pub judge: Option<Arc<dyn Completer>>,
    /// Separate from `judge` because the response format is part of the completer.
    pub link_judge: Option<Arc<dyn Completer>>,
    /// Names a knowledge gap; `None` with no synthesize role, and gaps are then named by their
    /// terms.
    pub gap_namer: Option<Arc<dyn Completer>>,
    /// Rules on a retired artifact; its own response shape. `None` with no synthesize role.
    pub reaper: Option<Arc<dyn Completer>>,
    /// Writes an artifact from a pursuit. `None` with no synthesize role.
    pub generator: Option<Arc<dyn Completer>>,
    /// Not `judge`: the press was the verdict, and the judge splits this class nine to three.
    pub pair_synthesizer: Option<Arc<dyn Completer>>,
    /// `None` is the whole off switch for the fanned-out second round, so disabled costs no call.
    pub planner: Option<Arc<dyn Completer>>,
    /// `None` closes the image door.
    pub describer: Option<Arc<dyn Describer>>,
    /// `None` takes the microphone off the search box.
    pub transcriber: Option<Arc<dyn Transcriber>>,
    /// Passage size, already clamped to the embedder.
    pub chunk_tokens: usize,
    pub counter: Arc<TokenCounter>,
    /// Shared by every clone of `Core`, so draining one drains them all.
    pub background: Arc<Background>,
    /// `System` in the binary; recommendation tests fix it.
    pub clock: crate::core::context::Clock,
    /// Shared by every clone and across registry evictions; see `Working`.
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
    pub consolidate: crate::config::ConsolidateConfig,
    /// The floor under the weak line; see `Core::weak_below`.
    pub weak_floor: f32,
    /// The measured unrelated line as `f32` bits, zero until measured; lives on `Working`.
    pub line: Arc<std::sync::atomic::AtomicU32>,
    /// Held so the explanation reconstructs the score from the store's own configuration.
    pub pinned_boost: f32,
    /// The one switch over everything learned from what happens here.
    pub learn: crate::config::LearnConfig,
    pub feedback: crate::config::FeedbackConfig,
    /// Never read on a path a person waits on.
    pub evolve: crate::config::EvolveConfig,
    pub capture: crate::config::CaptureConfig,
    pub associate: crate::config::AssociateConfig,
    pub activation: crate::config::ActivationConfig,
    pub promote: crate::config::PromoteConfig,
    pub pursuit: crate::config::PursuitConfig,
    pub schedule: crate::config::ScheduleConfig,
    pub time: crate::config::TimeConfig,
    pub reap: crate::config::ReapConfig,
    /// Whether and how the area under the search box is filled.
    pub recommend: crate::config::RecommendConfig,
    pub ui: crate::config::UiConfig,
    /// Shared by every clone and across registry evictions; see `Working`.
    pub sittings: Arc<crate::core::sitting::Sittings>,
    /// Shared by every clone: the point is one queue of calls in front of one GPU.
    pub gate: Arc<crate::infer::gate::InferenceGate>,
    /// One lock per document; reach for it through `corpus_lock`.
    pub corpus_locks: CorpusLocks,
    /// Serializes lifecycle transitions (two stores plus a marker, not atomic) against the marker
    /// repair.
    pub lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
    /// See `image::MAX_CONCURRENT_DECODES`; shared, because a per-clone permit would bound nothing.
    pub decodes: Arc<tokio::sync::Semaphore>,
    /// Shared, so an adoption changes the next request's search.
    pub ranking: Arc<std::sync::RwLock<crate::core::ranking::RankingParams>>,
}

/// A job's writes to the corpus in the last 24 hours against its own daily pace; see
/// `Store::actions_since`.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub used: u32,
    pub cap: u32,
}

impl Budget {
    pub fn spent(&self) -> bool {
        self.used >= self.cap
    }
}

impl Core {
    /// One job's day: what it has written, and what it may.
    pub async fn budget(&self, job: crate::store::actions::Job) -> crate::error::Result<Budget> {
        let used = self
            .store
            .actions_since(job, crate::store::now() - 86_400)
            .await?;
        Ok(Budget {
            used: used.clamp(0, u32::MAX as i64) as u32,
            cap: self.evolve.max_actions_per_day,
        })
    }

    /// Whether `job` may act at all and has not spent its own day.
    pub async fn may_act(&self, job: crate::store::actions::Job) -> crate::error::Result<bool> {
        if !self.evolve.autonomous.acts_on_corpus() {
            return Ok(true);
        }
        Ok(!self.budget(job).await?.spent())
    }

    /// Put back the version a condensation retired; called by the button and by the base.
    pub async fn uncondense(
        &self,
        action_id: &str,
        by: crate::store::actions::UndoneBy,
    ) -> crate::error::Result<()> {
        use crate::error::Error;
        let a = self.store.action(action_id).await?.ok_or(Error::NotFound)?;
        if a.kind != crate::store::actions::Kind::Condense {
            return Err(Error::Validation("not a condensation".into()));
        }
        if a.undone_at.is_some() {
            return Ok(());
        }
        let n = serde_json::from_str::<serde_json::Value>(&a.evidence_json)
            .ok()
            .and_then(|v| v.get("version").and_then(|n| n.as_i64()))
            .ok_or_else(|| Error::Store("a condense row names its version".into()))?;
        self.store.restore_version(&a.subject_id, n, &a.id).await?;
        self.store
            .undo_action_on(
                &a.subject_id,
                crate::store::actions::Kind::Condense,
                by,
                "the earlier version is back",
            )
            .await?;
        self.store
            .enqueue(crate::store::jobs::Stage::Embed, "artifact", &a.subject_id)
            .await?;
        Ok(())
    }

    /// The measured line, or zero while unmeasured.
    fn measured_line(&self) -> f32 {
        f32::from_bits(self.line.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The one "is this near that" line for search, gaps, `gaps::cover` and pursuit: measured, or
    /// the floor.
    pub fn weak_below(&self) -> f32 {
        crate::core::gaps::line_above(self.weak_floor, self.measured_line())
    }

    /// The same measurement as `weak_below`, over a higher floor.
    pub fn link_at(&self) -> f32 {
        crate::core::gaps::line_above(crate::core::gaps::GAP_LINK_AT, self.measured_line())
    }

    /// For tests: sets the floor and drops the measurement.
    pub fn set_weak_below(&mut self, at: f32) {
        self.weak_floor = at;
        self.line = Arc::new(std::sync::atomic::AtomicU32::new(0));
    }

    /// Without this, a restart reads `weak_floor` (0.35) and treats unrelated things as related
    /// until the next pass. Never overwrites a measurement in hand, which is at least as fresh as
    /// `meta`.
    pub async fn adopt_measured_line(&self) -> crate::error::Result<()> {
        if self.measured_line() > 0.0 {
            return Ok(());
        }
        let stored = self
            .store
            .meta_get(CALIBRATION_KEY)
            .await?
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.0);
        if stored > 0.0 {
            self.line
                .store(stored.to_bits(), std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// Measure where "unrelated" stops in this base and store it in memory and `meta`; runs on the
    /// repair pass.
    pub async fn calibrate(&self) -> crate::error::Result<f32> {
        let queries = self.store.calibration_vecs(self.embedder.model()).await?;
        let artifacts: Vec<Vec<f32>> = self
            .vectors
            .sample(crate::store::gaps::CALIBRATION_SAMPLE as usize)
            .await?
            .into_iter()
            .map(|(_, v)| v)
            .filter(|v| queries.first().is_none_or(|q| q.len() == v.len()))
            .collect();
        let line = match crate::core::gaps::unrelated_line(&queries, &artifacts) {
            Some(l) => l,
            None => self
                .store
                .meta_get(CALIBRATION_KEY)
                .await?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
        };
        self.store
            .meta_set(CALIBRATION_KEY, &line.to_string())
            .await?;
        self.line
            .store(line.to_bits(), std::sync::atomic::Ordering::Relaxed);
        Ok(line)
    }

    /// The same core with the named roles served by models in this process.
    #[cfg(feature = "contained")]
    pub fn with_local_models(
        mut self,
        cfg: &Config,
        models: &crate::infer::local::LocalModels,
    ) -> Core {
        use crate::infer::local::{LocalCompleter, LocalEmbedder, LocalReranker};
        if let Some(path) = &models.embed {
            self.embedder = Arc::new(LocalEmbedder::new(path.clone(), &cfg.infer.embed));
        }
        if let Some(path) = &models.rerank {
            // A pair is a query and one passage; 512 holds both with room.
            self.reranker = Some(Arc::new(LocalReranker::new(path.clone(), 512)));
            if self.rerank_apply.is_empty() {
                self.rerank_apply = vec![
                    crate::config::RerankApply::Ask,
                    crate::config::RerankApply::Search,
                ];
            }
        }
        if let (Some(path), Some(role)) = (&models.ask, cfg.infer.ask.as_ref()) {
            self.completer = Some(Arc::new(LocalCompleter::new(path.clone(), role)));
        }
        // No config role stands behind this: the file's presence opens the microphone.
        if let Some(path) = &models.speech {
            self.transcriber = Some(Arc::new(crate::infer::local::LocalTranscriber::new(
                path.clone(),
                None,
            )));
        }
        self
    }

    pub fn from_config(cfg: &Config, vectors: Arc<dyn VectorStore>, store: Store) -> Core {
        Core::from_config_with(cfg, vectors, store, Working::default())
    }

    /// The registry's door: a reopened tenant keeps its working memory.
    pub fn from_config_with(
        cfg: &Config,
        vectors: Arc<dyn VectorStore>,
        store: Store,
        working: Working,
    ) -> Core {
        // Headroom for token-count estimation error.
        let max_artifact_tokens = (cfg.infer.embed.max_input_tokens as f32 * 0.8) as usize;

        let synth = cfg.infer.synthesize.as_ref();
        // One counter for request budgeting and windowing, or prompts are sized at 0.6N and refused
        // with a 400.
        let counter = Arc::new(TokenCounter::load(
            cfg.infer.tokenizer.as_deref(),
            std::path::Path::new(&cfg.store.dir),
        ));
        Core {
            live: Default::default(),
            evolve: cfg.evolve.clone(),
            store,
            vectors,
            synthesizer: synth.map(|s| {
                Arc::new(
                    HttpSynthesizer::new(s)
                        .with_max_artifact_tokens(max_artifact_tokens)
                        .with_counter(counter.clone()),
                ) as Arc<dyn Synthesizer>
            }),
            embedder: Arc::new(HttpEmbedder::new(&cfg.infer.embed)),
            reranker: cfg
                .infer
                .rerank
                .as_ref()
                .map(|r| Arc::new(HttpReranker::new(r)) as Arc<dyn Reranker>),
            rerank_apply: cfg
                .infer
                .rerank
                .as_ref()
                .map(|r| r.apply.clone())
                .unwrap_or_default(),
            completer: cfg.infer.ask.as_ref().map(|a| {
                Arc::new(HttpCompleter::new(a).with_counter(counter.clone())) as Arc<dyn Completer>
            }),
            judge: synth.map(|s| {
                Arc::new(HttpCompleter::for_judging(s).with_counter(counter.clone()))
                    as Arc<dyn Completer>
            }),
            link_judge: synth.map(|s| {
                Arc::new(HttpCompleter::for_link_judging(s).with_counter(counter.clone()))
                    as Arc<dyn Completer>
            }),
            gap_namer: synth.map(|s| {
                Arc::new(HttpCompleter::for_gap_naming(s).with_counter(counter.clone()))
                    as Arc<dyn Completer>
            }),
            reaper: synth.map(|s| {
                Arc::new(HttpCompleter::for_reaping(s).with_counter(counter.clone()))
                    as Arc<dyn Completer>
            }),
            generator: synth.map(|s| {
                Arc::new(HttpCompleter::for_generating(s).with_counter(counter.clone()))
                    as Arc<dyn Completer>
            }),
            pair_synthesizer: synth.map(|s| {
                Arc::new(HttpCompleter::for_pair_synthesis(s).with_counter(counter.clone()))
                    as Arc<dyn Completer>
            }),
            planner: cfg.infer.ask.as_ref().and_then(|a| {
                a.plan.then(|| {
                    Arc::new(HttpCompleter::for_plan(&a.plan_on()).with_counter(counter.clone()))
                        as Arc<dyn Completer>
                })
            }),
            describer: cfg
                .infer
                .vision
                .as_ref()
                .map(|v| Arc::new(HttpDescriber::new(v, synth)) as Arc<dyn Describer>),
            transcriber: cfg
                .infer
                .transcribe
                .as_ref()
                .map(|t| Arc::new(HttpTranscriber::new(t)) as Arc<dyn Transcriber>),
            chunk_tokens: cfg.infer.embed.effective_chunk_tokens(),
            counter,
            background: Arc::new(Background::default()),
            clock: crate::core::context::Clock::System,
            query_cache: working.query_cache,
            consolidate: cfg.consolidate.clone(),
            ranking: Arc::new(std::sync::RwLock::new(
                crate::core::ranking::RankingParams::from_config(
                    &cfg.vector,
                    &cfg.associate,
                    &cfg.consolidate,
                    &cfg.sitting,
                    cfg.infer.rerank.is_some(),
                ),
            )),
            weak_floor: cfg.vector.weak_below,
            // See `Working::line`.
            line: working.line,
            pinned_boost: cfg.vector.pinned_boost,
            learn: cfg.learn.clone(),
            feedback: cfg.feedback.clone(),
            capture: cfg.capture.clone(),
            associate: cfg.associate.clone(),
            activation: cfg.activation.clone(),
            promote: cfg.promote.clone(),
            pursuit: cfg.pursuit.clone(),
            schedule: cfg.schedule.clone(),
            time: cfg.time.clone(),
            reap: cfg.reap.clone(),
            recommend: cfg.recommend.clone(),
            ui: cfg.ui.clone(),
            sittings: working.sittings,
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

    /// Exclusive access to one document's artifact rows; segment writes and `finish` renumbering
    /// must not interleave. Never hold this across an inference call. `tokio` mutex, so a waiter
    /// yields its thread.
    pub async fn corpus_lock(&self, corpus_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .corpus_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Anything still held or about to be locked has a strong count above one.
            locks.retain(|_, l| Arc::strong_count(l) > 1);
            Arc::clone(locks.entry(corpus_id.to_string()).or_default())
        };
        lock.lock_owned().await
    }

    /// Links are learned from recorded searches, and nothing is recorded while `[learn]` is off.
    pub fn associating(&self) -> bool {
        self.learn.enabled
    }

    /// Only under `[learn] mode = "full"`; in `learning` the harness measures a ranking nothing has
    /// moved.
    pub fn moves_ranking(&self) -> bool {
        self.evolve.autonomous.moves_ranking() && self.learn.mode == crate::config::LearnMode::Full
    }

    /// `false` means no `[infer.ask]`: no ask page, no nav entry, no MCP tool, no `/api/ask`.
    pub fn asks(&self) -> bool {
        self.completer.is_some()
    }

    /// `false` is the embed-only floor: no judged read, reminders, moments, promotion or day
    /// reading.
    pub fn synthesizes(&self) -> bool {
        self.synthesizer.is_some()
    }

    /// The one spelling of the question, so a guard added here holds for every door.
    fn reranks(&self, place: crate::config::RerankApply) -> bool {
        self.reranker.is_some() && self.rerank_apply.contains(&place)
    }

    /// `false` means the UI never claims a refining pass happened.
    pub fn reranks_search(&self) -> bool {
        self.reranks(crate::config::RerankApply::Search)
    }

    /// Does a reranker serve the ask path? The retrieval behind an answer is
    /// ordered by it before the citations are chosen.
    pub fn reranks_ask(&self) -> bool {
        self.reranks(crate::config::RerankApply::Ask)
    }

    /// Needs `[learn]` too: without the log the sweep profiles nothing, silently.
    pub fn recommends(&self) -> bool {
        self.recommend.enabled && self.learn.enabled
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::infer::fake::{
        FailingReranker, FakeCompleter, FakeDescriber, FakeEmbedder, FakeReranker, FakeSynthesizer,
        FakeTranscriber,
    };
    use crate::vector::memory::MemoryVectors;

    pub const TEST_DIM: usize = 8;

    /// A core with the recommender and the learning layer on, and a clock that does not move.
    pub async fn recommending_core(now: i64) -> Core {
        let mut core = test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        core.clock = crate::core::context::Clock::Fixed(now);
        core
    }

    /// The client bundle of a phone, for the two modules that profile one.
    pub fn phone_bundle() -> crate::core::context::Bundle {
        crate::core::context::Bundle {
            tz: Some("Europe/Berlin".into()),
            platform: Some("Android".into()),
            ua_family: Some("Chrome".into()),
            screen_w: Some(390.0),
            screen_h: Some(844.0),
            viewport_w: Some(390.0),
            viewport_h: Some(844.0),
            dpr: Some(3.0),
            cores: Some(8.0),
            memory_gb: Some(4.0),
            language: Some("de-DE".into()),
            color_scheme: Some("dark".into()),
            touch: Some(true),
            orientation: Some("portrait".into()),
            network: Some("cellular".into()),
            ..Default::default()
        }
    }

    /// The point is needed: `context_query` renders from the payload.
    pub async fn seed_artifact(core: &Core, title: &str) -> String {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: format!("text of {title}"),
                    title: Some(title.into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0);
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: vec![1.0; TEST_DIM],
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: a.id.clone(),
                    corpus_id: src.id.clone(),
                    text: a.text.clone(),
                    title: Some(title.into()),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();
        a.id
    }

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

    /// A core whose reranker errors on every call — the endpoint outage or
    /// cold start a deployment actually sees.
    pub async fn test_core_with_failing_reranker() -> Core {
        build(
            Arc::new(FakeSynthesizer::default()),
            Some(Arc::new(FailingReranker)),
        )
        .await
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

    /// A core whose speech model is the given fake, so the microphone is on
    /// the page and answers.
    pub async fn test_core_with_transcriber(t: Arc<FakeTranscriber>) -> Core {
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.transcriber = Some(t);
        core
    }

    /// The shipped default: no `[infer.vision]`, image door closed.
    pub async fn test_core_without_vision() -> Core {
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.describer = None;
        core
    }

    /// An installation with no `[infer.ask]`. `Core::asks` is false, so the
    /// ask door is not there at all — no button, no route, no MCP tool.
    pub async fn test_core_without_ask() -> Core {
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.completer = None;
        core
    }

    /// The embed-only floor: no `[infer.synthesize]`, so no synthesizer and
    /// none of the judges, writers and namers that share its endpoint.
    pub async fn test_core_without_synthesizer() -> Core {
        let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
        core.synthesizer = None;
        core.judge = None;
        core.link_judge = None;
        core.gap_namer = None;
        core.reaper = None;
        core.generator = None;
        core.pair_synthesizer = None;
        core
    }

    async fn build(synthesizer: Arc<dyn Synthesizer>, reranker: Option<Arc<dyn Reranker>>) -> Core {
        let store = Store::memory().await.unwrap();
        Core {
            live: Default::default(),
            evolve: crate::config::EvolveConfig::default(),
            store,
            vectors: Arc::new(MemoryVectors::new()),
            synthesizer: Some(synthesizer),
            embedder: Arc::new(FakeEmbedder::new(TEST_DIM)),
            reranker,
            rerank_apply: vec![
                crate::config::RerankApply::Ask,
                crate::config::RerankApply::Search,
            ],
            completer: Some(Arc::new(FakeCompleter::default())),
            judge: Some(Arc::new(FakeCompleter::default())),
            link_judge: Some(Arc::new(FakeCompleter::default())),
            gap_namer: Some(Arc::new(FakeCompleter {
                reply: Some(r#"{"label":"Fake topic"}"#.into()),
            })),
            reaper: Some(Arc::new(FakeCompleter::default())),
            generator: Some(Arc::new(FakeCompleter::default())),
            // Only a pair a person pressed is synthesized; tests of that path set this.
            pair_synthesizer: None,
            // Off, unlike the shipped default, so other tests get one round and no extra call.
            planner: None,
            describer: Some(Arc::new(FakeDescriber::default())),
            transcriber: None,
            chunk_tokens: crate::config::DEFAULT_CHUNK_TOKENS,
            counter: Arc::new(TokenCounter::default()),
            background: Arc::new(Background::default()),
            clock: crate::core::context::Clock::System,
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            consolidate: crate::config::ConsolidateConfig::default(),
            // Recency off: the fake embedder's ordering is all a search test can assert against.
            ranking: Arc::new(std::sync::RwLock::new(
                crate::core::ranking::RankingParams {
                    recency_weight: 0.0,
                    per_source_cap: Some(crate::core::search::MAX_PER_CORPUS),
                    ..Default::default()
                },
            )),
            // The fake embedder's vectors are no semantic space; a realistic threshold would mark
            // noise weak.
            weak_floor: 0.0,
            line: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            pinned_boost: 0.15,
            learn: crate::config::LearnConfig {
                enabled: false,
                ..Default::default()
            },
            feedback: crate::config::FeedbackConfig::default(),
            capture: crate::config::CaptureConfig::default(),
            associate: crate::config::AssociateConfig::default(),
            activation: crate::config::ActivationConfig::default(),
            promote: crate::config::PromoteConfig::default(),
            pursuit: crate::config::PursuitConfig::default(),
            schedule: crate::config::ScheduleConfig::default(),
            time: crate::config::TimeConfig::default(),
            reap: crate::config::ReapConfig::default(),
            // Off, unlike the shipped default: a test that leaves both flags alone must offer
            // nothing.
            recommend: crate::config::RecommendConfig {
                enabled: false,
                ..Default::default()
            },
            ui: Default::default(),
            sittings: Arc::new(Default::default()),
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

    fn crowded(n: usize) -> Vec<Vec<f32>> {
        let mut state = 0x2545_f491u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        (0..n)
            .map(|_| {
                (0..test_support::TEST_DIM)
                    .map(|d| next() * 0.3 + if d == 0 { 1.0 } else { 0.0 })
                    .collect()
            })
            .collect()
    }

    #[tokio::test]
    async fn the_weak_line_is_measured_from_the_base_and_shared() {
        let mut core = test_support::test_core().await;
        core.weak_floor = 0.35;
        let reader = core.clone();
        assert_eq!(core.calibrate().await.unwrap(), 0.0);
        assert_eq!(reader.weak_below(), 0.35);
        assert_eq!(reader.link_at(), crate::core::gaps::GAP_LINK_AT);

        for (i, v) in crowded(40).into_iter().enumerate() {
            core.store
                .record_search(
                    crate::store::feedback::NewEvent {
                        fold_onto: None,
                        query: format!("q{i}"),
                        door: crate::store::feedback::Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: v,
                        embed_model: core.embedder.model().into(),
                        candidates: vec![],
                        answered: false,
                        context: None,
                    },
                    0,
                )
                .await
                .unwrap();
        }
        let line = core.calibrate().await.unwrap();
        assert!(line > 0.35, "{line}");
        assert_eq!(
            reader.weak_below(),
            line.min(crate::core::gaps::LINE_CEILING)
        );
        assert!(reader.link_at() >= reader.weak_below());
        assert_eq!(
            core.store.meta_get("calibration.line").await.unwrap(),
            Some(line.to_string())
        );
    }

    #[tokio::test]
    async fn a_core_starts_from_the_line_the_base_last_measured() {
        let mut core = test_support::test_core().await;
        core.weak_floor = 0.35;
        core.adopt_measured_line().await.unwrap();
        assert_eq!(core.weak_below(), 0.35);

        core.store.meta_set(CALIBRATION_KEY, "0.6").await.unwrap();
        core.adopt_measured_line().await.unwrap();
        assert_eq!(core.weak_below(), 0.6);

        core.store.meta_set(CALIBRATION_KEY, "0.4").await.unwrap();
        core.adopt_measured_line().await.unwrap();
        assert_eq!(core.weak_below(), 0.6);
    }

    #[tokio::test]
    async fn one_subjects_cores_share_the_measurement_whichever_took_it() {
        let store = crate::store::Store::memory().await.unwrap();
        let vectors = Arc::new(crate::vector::memory::MemoryVectors::new());
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        let working = Working::default();
        let serving = Core::from_config_with(&cfg, vectors.clone(), store.clone(), working.clone());
        let passing = Core::from_config_with(&cfg, vectors, store, working);
        assert_eq!(serving.measured_line(), 0.0);
        passing
            .line
            .store(0.6f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            serving.measured_line(),
            0.6,
            "a measurement taken on a transient core did not reach the one serving"
        );
    }

    #[tokio::test]
    async fn the_example_config_and_the_compiled_default_ship_primed() {
        let cfg = Config::load(std::path::Path::new("config.example.toml").into()).unwrap();
        assert_eq!(
            cfg.associate.prime_lift, 1,
            "a base started from this file begins on the lowest non-zero rung"
        );
        assert!(
            crate::core::ranking::PRIME_LIFTS.contains(&cfg.associate.prime_lift),
            "and that rung is one the idle pass can walk away from"
        );
        assert!(
            cfg.sitting.prime,
            "with the sitting taking part in the lift it shares"
        );

        assert_eq!(
            crate::config::default_prime_lift(),
            cfg.associate.prime_lift,
            "the compiled default is the file's rung, so an unwritten key is primed too"
        );
        assert!(!crate::config::default_sitting_prime());
    }

    #[tokio::test]
    async fn the_example_config_carries_the_consolidation_defaults() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(cfg.consolidate.enabled);
        assert!(
            cfg.consolidate.auto_supersede > cfg.consolidate.review_min,
            "superseding at or below the review threshold would hide distinct artifacts"
        );
        assert!(
            cfg.consolidate.max_dedupe_per_tick > 0 && cfg.consolidate.dedupe_interval_mins > 0,
            "the pass must have a rate, or it either never runs or is unbounded"
        );
    }

    #[tokio::test]
    async fn the_layer_is_one_switch_and_it_ships_on() {
        assert!(crate::config::LearnConfig::default().enabled);
        let mut core = test_support::test_core().await;
        assert!(!core.learn.enabled);
        assert!(
            !core.associating(),
            "the layer is dark while `[learn]` is off"
        );
        assert!(
            !core.recommends(),
            "and so is the area under the search box"
        );

        core.learn.enabled = true;
        assert!(core.associating());
    }

    #[tokio::test]
    async fn the_offer_needs_the_log_as_well_as_its_own_switch() {
        let mut core = test_support::test_core().await;
        core.recommend.enabled = true;
        assert!(!core.recommends(), "its own switch is not enough");
        core.learn.enabled = true;
        assert!(core.recommends());
        core.recommend.enabled = false;
        assert!(!core.recommends(), "and neither is the log");
    }

    #[tokio::test]
    async fn one_document_is_locked_at_a_time_and_two_documents_are_not() {
        let core = test_support::test_core().await;

        let held = core.corpus_lock("doc-a").await;
        let core2 = core.clone();
        let blocked = tokio::spawn(async move {
            let _second = core2.corpus_lock("doc-a").await;
        });

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
        let core = test_support::test_core().await;
        for i in 0..50 {
            drop(core.corpus_lock(&format!("doc-{i}")).await);
        }
        let held = core.corpus_lock("doc-held").await;
        assert_eq!(
            core.corpus_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "released locks were kept"
        );
        drop(held);
    }

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

        // `Config` has no `Default`; the committed example file is a real config.
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
            apply: vec![
                crate::config::RerankApply::Ask,
                crate::config::RerankApply::Search,
            ],
        });
        let core = Core::from_config(&cfg, vectors, store);
        assert!(core.reranker.is_some());
    }

    #[test]
    fn a_config_still_setting_a_mode_is_refused_naming_the_reshape() {
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
        let err = crate::config::Config::load(Some(&path))
            .unwrap_err()
            .to_string();
        assert!(err.contains("2026-09 capture reshape"), "{err}");
        assert!(err.contains("infer.synthesis"), "{err}");
    }
}

#[cfg(all(test, feature = "contained"))]
mod contained_tests {
    use crate::core::test_support::{TEST_DIM, test_core};
    use crate::store::feedback::Door;
    use crate::tenants::VectorFactory;

    #[tokio::test]
    async fn a_capture_is_found_again_with_only_a_file_for_a_vector_store() {
        let dir = tempfile::tempdir().unwrap();
        let factory = crate::tenants::SqliteFactory {
            dir: dir.path().into(),
            prefix: "artifacts".into(),
            scoring: crate::vector::sqlite::Scoring::off(),
        };
        let mut core = test_core().await;
        core.vectors = factory.open("artifacts_phone", TEST_DIM).await.unwrap();

        let out = core
            .ingest(
                "Die Rechnung für den Steuerberater liegt im blauen Ordner.",
                "web",
                None,
            )
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;

        let q = crate::core::search::SearchQuery {
            q: "Steuerberater".into(),
            limit: 5,
            tags: vec![],
            category: None,
            mark: true,
            rerank: false,
            explain: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let hits = core.search(&q, Door::Cli).await.unwrap();
        assert!(
            hits.iter().any(|h| h.corpus_id == out.id),
            "the capture was not found"
        );

        let again = factory.open("artifacts_phone", TEST_DIM).await.unwrap();
        assert!(again.count().await.unwrap() > 0);
        assert_eq!(
            again.count().await.unwrap(),
            core.vectors.count().await.unwrap()
        );
    }

    #[tokio::test]
    async fn a_german_capture_is_found_by_an_english_question_with_no_endpoint_anywhere() {
        let Some(dir) = std::env::var_os("ENGRAM_TEST_MODELS").map(std::path::PathBuf::from) else {
            eprintln!("skipped: ENGRAM_TEST_MODELS is not set");
            return;
        };
        let mut cfg = crate::config::Config::test_default();
        cfg.infer.embed.dim = 768;
        cfg.infer.embed.max_input_tokens = 512;
        let models = crate::infer::local::LocalModels {
            embed: Some(dir.join("embed.gguf")),
            rerank: None,
            ask: None,
            speech: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let factory = crate::tenants::SqliteFactory {
            dir: tmp.path().into(),
            prefix: "artifacts".into(),
            scoring: crate::vector::sqlite::Scoring::off(),
        };
        let mut core = test_core().await.with_local_models(&cfg, &models);
        core.vectors = factory.open("artifacts_phone", 768).await.unwrap();

        let out = core
            .ingest(
                "Die Rechnung für den Steuerberater liegt im blauen Ordner.",
                "web",
                None,
            )
            .await
            .unwrap();
        core.ingest("Brot braucht Mehl, Wasser, Salz und Zeit.", "web", None)
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;

        let q = crate::core::search::SearchQuery {
            q: "where did I put the invoice for the tax adviser".into(),
            limit: 5,
            tags: vec![],
            category: None,
            mark: true,
            rerank: false,
            explain: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let hits = core.search(&q, Door::Cli).await.unwrap();
        assert_eq!(
            hits[0].corpus_id, out.id,
            "no word is shared; only the meaning is"
        );
    }
}
