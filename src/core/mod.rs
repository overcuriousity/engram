pub mod ask;
pub mod background;
pub mod context;
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

/// Where the measured weak line is kept between runs. Written by `calibrate`
/// on every repair pass and read back by `adopt_measured_line` when a core is
/// built, so a restart does not spend an hour at `weak_floor`.
const CALIBRATION_KEY: &str = "calibration.line";

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

/// The in-memory working state one subject keeps, independent of any `Core`.
///
/// Both fields are documented on `Core` as "shared by every clone", and for the
/// life of one `Core` they are. The registry is what broke that: `Tenants` caps
/// how many bases it holds open and builds a fresh `Core::from_config` on every
/// cache miss, so a user evicted between two requests came back with an empty
/// sitting and a cold query cache — search and ask carrying nothing, on an
/// instance whose own `config.example.toml` says eviction is transparent.
///
/// Held by subject in `Tenants` and handed back on reopen, which is what makes
/// that sentence true. Nothing here is persistent state: a sitting exists only
/// while it is warm and a cached embedding is a saved call, so losing this to a
/// restart is nothing, and losing it to the cap was a working memory that
/// silently stopped working past `store.max_open_tenants` active users.
#[derive(Clone)]
pub struct Working {
    pub sittings: Arc<crate::core::sitting::Sittings>,
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
    /// Where "unrelated" stops, as `Core::line` reads it — held here so that
    /// every `Core` this subject is served through shares one measurement.
    ///
    /// It is not working memory in the sense the two above are: it is a
    /// measurement of the base, and `meta.calibration.line` is where it
    /// actually lives. It is here because the hourly pass opens its own
    /// transient `Core` (`background::open_for_pass`) to calibrate on, and an
    /// atomic per `Core` meant that measurement was taken, written to `meta`,
    /// and then dropped with the core that took it — leaving whichever core is
    /// serving requests running at `weak_floor`, the configured 0.35, until
    /// some later pass happened to find it in the registry. A capture landing
    /// in that window answered every open gap it came within 0.40 of, and the
    /// next pass, calibrating properly, deleted all of it again as being under
    /// the line.
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
    /// Whether this is worth keeping for a subject nobody is serving.
    ///
    /// Read by the registry to drop the entries of users who have gone away,
    /// so holding working memory across eviction does not turn into a map with
    /// a row per subject the process has ever seen. A sitting is the whole of
    /// what would be missed — `Sittings` expires its own entries as they go
    /// cold — and a query cache without one is a handful of saved embeddings
    /// for a search nobody is running.
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

/// Live per-document locks, keyed by corpus id. Shared by every clone of
/// `Core`: a per-clone map would lock nothing, the same way a per-clone gate
/// would pace nothing.
pub type CorpusLocks =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

#[derive(Clone)]
pub struct Core {
    pub store: Store,
    pub vectors: Arc<dyn VectorStore>,
    /// Always present: `[infer.synthesize]` is required — capture synthesizes.
    pub synthesizer: Arc<dyn Synthesizer>,
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Option<Arc<dyn Reranker>>,
    /// Where the configured reranker is consulted — `[infer.rerank].apply`.
    /// Meaningless without a reranker; both places when one is configured
    /// without narrowing.
    pub rerank_apply: Vec<crate::config::RerankApply>,
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
    /// The judge that rules on a retired artifact: still worth something, or
    /// nothing the live base does not already say. Same endpoint as the other
    /// judges, its own response shape — the reap verdict is not a duplicate
    /// verdict, and asking it under that grammar could only ever fail to
    /// parse. `None` with no synthesize role.
    pub reaper: Option<Arc<dyn Completer>>,
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
    /// The speech model, when one is configured. `None` takes the microphone
    /// off the search box. Nothing on any background path reads it: this is
    /// the one model role that exists only for a control someone is holding.
    pub transcriber: Option<Arc<dyn Transcriber>>,
    /// Passage size, already clamped to the embedder.
    pub chunk_tokens: usize,
    pub counter: Arc<TokenCounter>,
    /// Writes that run off the request path. Shared by every clone of `Core`,
    /// so draining one drains them all.
    pub background: Arc<Background>,
    /// Where this feature reads the time. `System` in the binary; the
    /// recommendation tests set a fixed one so a seventh Friday at 14:52
    /// exists on demand. Nothing else in the tree reads it.
    pub clock: crate::core::context::Clock,
    /// Shared by every clone of `Core`, like the background queue, and across
    /// the tenant registry's evictions — see `Working`.
    pub query_cache: Arc<std::sync::Mutex<QueryCache>>,
    /// Thresholds and budgets for duplicate hygiene. Read on the capture path
    /// and by the sweep, so it lives here rather than being passed down.
    pub consolidate: crate::config::ConsolidateConfig,
    /// The floor under the weak line. See `VectorConfig::weak_below` and
    /// `Core::weak_below`.
    pub weak_floor: f32,
    /// Where "unrelated" stops under this base's embedder, as `f32` bits;
    /// zero until `calibrate` has measured it or `adopt_measured_line` has
    /// read the last measurement back. Shared by every clone of `Core` *and*
    /// by every `Core` built for this subject — it lives on `Working`, which
    /// the registry hands back on reopen. See `Working::line` for why that
    /// matters and `core::gaps::unrelated_line` for what is measured.
    pub line: Arc<std::sync::atomic::AtomicU32>,
    /// The recency decay's half-life and the pinned tag's boost — the two
    /// terms the vector store folds into one score and never reports back.
    /// Held here so the explanation reconstructs them from the same
    /// configuration the store was built from, rather than from a second
    /// reading that could drift. See `core::explain::scoring_terms`.
    pub recency_half_life_days: u32,
    pub pinned_boost: f32,
    /// The one switch over everything learned from what happens here. Read on
    /// the search path and by every sweep downstream of it, so it lives here
    /// rather than being threaded down. See `LearnConfig`.
    pub learn: crate::config::LearnConfig,
    /// How real searches are recorded for later judging. Read on the search
    /// path, so it lives here rather than being threaded down.
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
    pub time: crate::config::TimeConfig,
    pub reap: crate::config::ReapConfig,
    /// Whether and how the area under the search box is filled. Read by the
    /// sweep and on the page-view path, so it lives here rather than being
    /// threaded down.
    pub recommend: crate::config::RecommendConfig,
    /// The vector background behind the pages. Read by the sample endpoint,
    /// so it lives here rather than being threaded down.
    pub ui: crate::config::UiConfig,
    /// Every live sitting, keyed by web session. Shared by every clone of
    /// `Core`, like the background queue — a per-clone map would be a per-clone
    /// working memory, which is no working memory at all. Shared across the
    /// tenant registry's evictions too, for the same reason and by the same
    /// argument: see `Working`.
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
    /// The two scoring knobs a tuning sweep may move. Shared by every clone,
    /// like the background queue: applying a recommendation has to change the
    /// search the next request runs, not the one this handler holds.
    pub ranking: Arc<std::sync::RwLock<crate::core::ranking::RankingParams>>,
    /// Whether a sweep is in flight, so a run of verdicts starts one and not
    /// one each.
    pub tuning: Arc<std::sync::atomic::AtomicBool>,
}

impl Core {
    /// The measured line, or zero while unmeasured.
    fn measured_line(&self) -> f32 {
        f32::from_bits(self.line.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Cosine similarity below which a result is unrelated to what was asked:
    /// the measured line when there is one, the configured floor otherwise.
    ///
    /// Read by the search path to label weak hits, by the gap store to decide
    /// what is a gap, by `gaps::cover` to decide what answers one, and by the
    /// pursuit sweep to decide what was engaged. All one line, because they
    /// are all the one question — is this near that — asked of one embedder.
    pub fn weak_below(&self) -> f32 {
        crate::core::gaps::line_above(self.weak_floor, self.measured_line())
    }

    /// Cosine similarity at or above which two queries are about one thing.
    /// The same measurement as `weak_below`, over a higher floor: a match has
    /// to reach further than "not unrelated".
    pub fn link_at(&self) -> f32 {
        crate::core::gaps::line_above(crate::core::gaps::GAP_LINK_AT, self.measured_line())
    }

    /// Pin the weak line, for tests that assert on the labelling: the floor is
    /// set and the measurement dropped, so `weak_below` returns exactly this.
    pub fn set_weak_below(&mut self, at: f32) {
        self.weak_floor = at;
        self.line = Arc::new(std::sync::atomic::AtomicU32::new(0));
    }

    /// Take up the last measurement this base recorded, for a core that has
    /// not been calibrated in this process yet.
    ///
    /// `calibrate` writes the line to `meta` on every pass and nothing read it
    /// back at startup: `line` began at zero, which `weak_below` reads as
    /// "unmeasured" and answers with `weak_floor` — the configured 0.35 —
    /// however many times this base had measured 0.60 before the restart. That
    /// is not a conservative fallback. Every reader of the line treats it as
    /// "below this, two things are unrelated", so a line an eighth of the way
    /// down from where it belongs makes unrelated things related: a capture
    /// closes gaps it does not answer, a search calls weak hits good, and the
    /// first repair pass after the restart deletes the lot for being under the
    /// line it should have been holding all along.
    ///
    /// Never overwrites a measurement already in hand — a second core opened
    /// for a subject the registry is already serving adopts nothing, because
    /// what `Working::line` holds is at least as fresh as what is in `meta`.
    ///
    /// A base that has never been calibrated has no key and keeps the floor,
    /// which is what the floor is for.
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

    /// Measure where "unrelated" stops from what this base holds — every
    /// recorded query against the others and against a sample of stored
    /// artifacts — and remember it, in memory for the readers above and in
    /// the meta table for the next boot. Runs on the repair pass, so the line
    /// follows the base as it grows and the embedder if it changes; a base
    /// with too little recorded keeps the floors.
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

    /// Build the running core from configuration. Lives here rather than in
    /// `main`, so the evaluation harness drives exactly the `Core` the binary
    /// does — a benchmark against a differently wired core measures the wrong
    /// program.
    pub fn from_config(cfg: &Config, vectors: Arc<dyn VectorStore>, store: Store) -> Core {
        Core::from_config_with(cfg, vectors, store, Working::default())
    }

    /// `from_config`, over working memory that already exists.
    ///
    /// The registry's door. See `Working`: a reopened tenant is the same user
    /// mid-sitting, and building this state fresh is what made the cap visible
    /// to them.
    pub fn from_config_with(
        cfg: &Config,
        vectors: Arc<dyn VectorStore>,
        store: Store,
        working: Working,
    ) -> Core {
        // Chunk size is capped by what the embedder accepts, with headroom for
        // token-count estimation error.
        let max_artifact_tokens = (cfg.infer.embed.max_input_tokens as f32 * 0.8) as usize;

        let synth = &cfg.infer.synthesize;
        // Built before the roles, not inside the struct literal, because the
        // request side has to budget with the same counter the windowing side
        // sizes with. Two counters that disagree is a prompt sized to N real
        // tokens and estimated at 0.6N — room the endpoint does not have, and a
        // non-retryable 400 on every sweep. See `HttpSynthesizer::counter`.
        let counter = Arc::new(TokenCounter::load(
            cfg.infer.tokenizer.as_deref(),
            std::path::Path::new(&cfg.store.dir),
        ));
        Core {
            store,
            vectors,
            synthesizer: Arc::new(
                HttpSynthesizer::new(synth)
                    .with_max_artifact_tokens(max_artifact_tokens)
                    .with_counter(counter.clone()),
            ),
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
            judge: Some(Arc::new(
                HttpCompleter::for_judging(synth).with_counter(counter.clone()),
            )),
            link_judge: Some(Arc::new(
                HttpCompleter::for_link_judging(synth).with_counter(counter.clone()),
            )),
            gap_namer: Some(Arc::new(
                HttpCompleter::for_gap_naming(synth).with_counter(counter.clone()),
            )),
            reaper: Some(Arc::new(
                HttpCompleter::for_reaping(synth).with_counter(counter.clone()),
            )),
            generator: Some(Arc::new(
                HttpCompleter::for_generating(synth).with_counter(counter.clone()),
            )),
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
                .map(|v| Arc::new(HttpDescriber::new(v, Some(synth))) as Arc<dyn Describer>),
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
                crate::core::ranking::RankingParams::from_vector(&cfg.vector),
            )),
            tuning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            weak_floor: cfg.vector.weak_below,
            // From the subject's working memory, not a fresh atomic: see
            // `Working::line`. A core built for one pass and dropped after it
            // still hands its measurement to whichever core is serving.
            line: working.line,
            recency_half_life_days: cfg.vector.recency_half_life_days.max(1),
            pinned_boost: cfg.vector.pinned_boost,
            learn: cfg.learn.clone(),
            feedback: cfg.feedback.clone(),
            capture: cfg.capture.clone(),
            associate: cfg.associate.clone(),
            activation: cfg.activation.clone(),
            promote: cfg.promote.clone(),
            pursuit: cfg.pursuit.clone(),
            schedule: cfg.schedule.clone(),
            sitting: cfg.sitting.clone(),
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
    /// Links are learned from recorded searches (`search_events`), and nothing
    /// is recorded while `[learn]` is off. Kept as a named predicate rather
    /// than inlined: every site that primes, associates, bumps activation from
    /// a search, or renders "seen together" asks this one question, and the
    /// name says which question it is.
    pub fn associating(&self) -> bool {
        self.learn.enabled
    }

    /// Is there an ask model to call? `false` means no `[infer.ask]`: no ask
    /// page, no nav entry, no MCP tool, no `/api/ask`.
    pub fn asks(&self) -> bool {
        self.completer.is_some()
    }

    /// Does a reranker serve `place` at all? One is configured and the scope
    /// covers it. The one spelling of that question: a guard added here — a
    /// health check, a kill switch, a new scope — holds for every door.
    fn reranks(&self, place: crate::config::RerankApply) -> bool {
        self.reranker.is_some() && self.rerank_apply.contains(&place)
    }

    /// Does a reranker serve the search path? `false` means the UI never fires
    /// a refining pass and never claims one happened: with no reranker — or
    /// one scoped to ask alone — a `rerank=true` request answers in vector
    /// order, and saying "refined" over it would assert a confirmation that
    /// never took place.
    pub fn reranks_search(&self) -> bool {
        self.reranks(crate::config::RerankApply::Search)
    }

    /// Does a reranker serve the ask path? The retrieval behind an answer is
    /// ordered by it before the citations are chosen.
    pub fn reranks_ask(&self) -> bool {
        self.reranks(crate::config::RerankApply::Ask)
    }

    /// Is the area under the search box filled? `false` means the placeholder
    /// is not rendered, the endpoint records nothing, and the sweep does not
    /// run — one question, asked in one place.
    ///
    /// `[learn]` as well as `[recommend]`, and not as a second gate over the
    /// faculty: the situations it clusters are opens recorded in the same log
    /// everything else here reads, so with the log unwritten the sweep profiles
    /// nothing and the ladder falls to its floor for ever. Saying so here is
    /// what stops that being a silent state.
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

    async fn build(synthesizer: Arc<dyn Synthesizer>, reranker: Option<Arc<dyn Reranker>>) -> Core {
        let store = Store::memory().await.unwrap();
        Core {
            store,
            vectors: Arc::new(MemoryVectors::new()),
            synthesizer,
            embedder: Arc::new(FakeEmbedder::new(TEST_DIM)),
            reranker,
            // The shipped default: a configured reranker serves both places.
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
            // Off, unlike the shipped default: a test that wants a fan-out puts
            // a completer here, and every other test gets one round and no
            // extra call to account for.
            planner: None,
            describer: Some(Arc::new(FakeDescriber::default())),
            // Off, like the shipped default: the tests that want a microphone
            // put one here, and every other test renders the page without one.
            transcriber: None,
            chunk_tokens: crate::config::DEFAULT_CHUNK_TOKENS,
            counter: Arc::new(TokenCounter::default()),
            background: Arc::new(Background::default()),
            clock: crate::core::context::Clock::System,
            query_cache: Arc::new(std::sync::Mutex::new(QueryCache::new(QUERY_CACHE_CAPACITY))),
            consolidate: crate::config::ConsolidateConfig::default(),
            // The shipped cap, so a test ranks what the binary ranks; recency
            // off, because the fake embedder's ordering is the only thing a
            // search test can assert against and an age boost over it is noise.
            ranking: Arc::new(std::sync::RwLock::new(
                crate::core::ranking::RankingParams {
                    recency_weight: 0.0,
                    per_source_cap: Some(crate::core::search::MAX_PER_CORPUS),
                },
            )),
            tuning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // The fake embedder's vectors are not a semantic space, so a
            // realistic threshold would mark arbitrary results weak and every
            // search test would be asserting against noise. Tests that care
            // about the labelling set it themselves.
            weak_floor: 0.0,
            line: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            recency_half_life_days: 180,
            pinned_boost: 0.15,
            // Off in tests, whatever ships: the tests that need a log switch
            // it on and the rest assert nothing is recorded.
            learn: crate::config::LearnConfig {
                enabled: false,
                ..Default::default()
            },
            feedback: crate::config::FeedbackConfig::default(),
            capture: crate::config::CaptureConfig::default(),
            // Inert in most tests whatever it says, because `learn` is off and
            // nothing has learned a link yet. The association tests seed one.
            associate: crate::config::AssociateConfig::default(),
            activation: crate::config::ActivationConfig::default(),
            promote: crate::config::PromoteConfig::default(),
            pursuit: crate::config::PursuitConfig::default(),
            schedule: crate::config::ScheduleConfig::default(),
            sitting: crate::config::SittingConfig::default(),
            // The fake embedder hashes text into eight dimensions, where two
            // unrelated strings clear 0.80 by chance and the classifier fires on
            // noise. Tests of the classifier hand it vectors directly.
            time: crate::config::TimeConfig::default(),
            reap: crate::config::ReapConfig::default(),
            // Off, unlike the shipped default: `recommends()` is two flags and
            // a test that leaves both alone must offer nothing. The
            // recommendation tests switch both on.
            recommend: crate::config::RecommendConfig {
                enabled: false,
                ..Default::default()
            },
            ui: Default::default(),
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

    /// Deterministic stand-in for an embedder that puts everything close
    /// together, the way bge-m3 does.
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

    /// The weak line is measured, not configured: with enough recorded
    /// queries the base says where unrelated stops, every clone of `Core`
    /// reads the one measurement, and the next boot finds it in meta. With
    /// too few, the configured floor stands.
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

    /// A core built over a base that has already measured its line starts
    /// with that line, rather than at the floor until some later pass gets
    /// round to it.
    ///
    /// The bug this is against: `calibrate` runs on the hourly pass, which
    /// opens a *transient* core to run it on, so the measurement was written
    /// to meta and then dropped with the core that took it. Whatever core was
    /// serving requests went on reading `weak_below` as the 0.35 floor, and a
    /// capture landing in that window closed every gap it came within 0.40 of
    /// — all of which the next pass then deleted as being under the line.
    #[tokio::test]
    async fn a_core_starts_from_the_line_the_base_last_measured() {
        let mut core = test_support::test_core().await;
        core.weak_floor = 0.35;
        // Nothing recorded: no key, the floor stands, and adopting is a no-op
        // rather than a zero written over it.
        core.adopt_measured_line().await.unwrap();
        assert_eq!(core.weak_below(), 0.35);

        core.store.meta_set(CALIBRATION_KEY, "0.6").await.unwrap();
        core.adopt_measured_line().await.unwrap();
        assert_eq!(core.weak_below(), 0.6);

        // A measurement in hand is never overwritten by a stored one: what the
        // running process measured is at least as fresh as what is on disk.
        core.store.meta_set(CALIBRATION_KEY, "0.4").await.unwrap();
        core.adopt_measured_line().await.unwrap();
        assert_eq!(core.weak_below(), 0.6);
    }

    /// Two cores built for one subject read one line, whichever of them
    /// measured it. `Working` is what carries it across the two, and the
    /// hourly pass's transient core is the one that matters: see
    /// `background::open_for_pass`.
    #[tokio::test]
    async fn one_subjects_cores_share_the_measurement_whichever_took_it() {
        let store = crate::store::Store::memory().await.unwrap();
        let vectors = Arc::new(crate::vector::memory::MemoryVectors::new());
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        let working = Working::default();
        // What the registry does on two cache misses for one subject — and
        // what `open_for_pass` does beside whichever core is serving.
        let serving =
            Core::from_config_with(&cfg, vectors.clone(), store.clone(), working.clone());
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
    async fn the_layer_is_one_switch_and_it_ships_on() {
        // Recording searches, learning links and writing pursuits were three
        // flags that only ever meant something together — two of their
        // combinations were refused at startup and the third was a warning.
        // One switch now, on by default: promotion reads activation, and
        // activation only moves while the log is being written, so off is the
        // deliberate act. The test core keeps it off so every other test
        // starts from nothing recorded.
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
        // The situations it clusters are opens recorded in the same log
        // everything else reads. `[recommend]` alone over an unwritten log is
        // a sweep that profiles nothing and a ladder that falls to its floor
        // for ever — the inert state this predicate exists to make impossible.
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
        // `synthesis = "off"` was a complete product state once. Since the
        // 2026-09 capture reshape it is a removed key, and the refusal has to
        // say what changed rather than parse silently into something else.
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
