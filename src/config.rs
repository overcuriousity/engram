use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    /// Defaulted: every field under it has a default, so an empty table is no reason to refuse.
    #[serde(default)]
    pub store: StoreConfig,
    pub vector: VectorConfig,
    pub infer: InferConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub consolidate: ConsolidateConfig,
    #[serde(default)]
    pub learn: LearnConfig,
    #[serde(default)]
    pub feedback: FeedbackConfig,
    #[serde(default)]
    pub pacing: PacingConfig,
    #[serde(default)]
    pub evolve: EvolveConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub associate: AssociateConfig,
    #[serde(default)]
    pub activation: ActivationConfig,
    #[serde(default)]
    pub promote: PromoteConfig,
    #[serde(default)]
    pub pursuit: PursuitConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub sitting: SittingConfig,
    #[serde(default)]
    pub time: TimeConfig,
    #[serde(default)]
    pub reap: ReapConfig,
    #[serde(default)]
    pub recommend: RecommendConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

/// What the two supplied-from-outside capture paths may cost; apart from `MAX_BODY_BYTES`,
/// which bounds what a client sends, not what we fetch.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CaptureConfig {
    /// Ceiling on a server-side GET, in seconds.
    pub fetch_timeout_secs: u64,
    /// Bytes read from a fetched URL before the transfer is abandoned.
    pub fetch_max_bytes: usize,
    /// Characters an extraction must yield to count; less is boilerplate, and nothing is stored.
    pub min_extracted_chars: usize,
    /// Bytes an uploaded image may weigh (the image door's own ceiling).
    pub image_max_bytes: usize,
    /// Longest edge in pixels of the preview the vision model and UI see; the original is kept.
    pub image_preview_edge: u32,
    /// Bytes an uploaded PDF may weigh (the upload door's own ceiling); no page cap.
    pub pdf_max_bytes: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fetch_timeout_secs: 30,
            fetch_max_bytes: 8 * 1024 * 1024,
            min_extracted_chars: 200,
            image_max_bytes: 25 * 1024 * 1024,
            image_preview_edge: 2048,
            pdf_max_bytes: 50 * 1024 * 1024,
        }
    }
}

/// Pacing for every generating inference call: the roles share one GPU, so one gap fronts them
/// all. Embedding takes its turn but neither serves nor starts the gap.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct PacingConfig {
    /// Minimum seconds between background generations; zero disables. Ask and embedding ignore it.
    pub cooldown_secs: u64,
}

/// The one switch over everything learned from use: recording searches, links, pursuits. On by
/// default; off keeps no query wording at all.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LearnConfig {
    /// The named bundle; a key written in the file wins over what the mode says.
    pub mode: LearnMode,
    pub enabled: bool,
    /// What the mode decided, for `--print-config`; filled by `load`, never read from the file.
    #[serde(skip)]
    pub resolved: Vec<(&'static str, String)>,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            mode: LearnMode::default(),
            enabled: true,
            resolved: Vec::new(),
        }
    }
}

/// The dial: one word that decides the keys the file leaves unset, so a mode cannot disagree
/// with itself. `learning` records everything and reads none of it, for an honest sweep.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LearnMode {
    /// Record, learn, prime, promote nothing; ranking holds. Capture, search, ask, curation stay.
    Off,
    /// Record and accumulate, read none of it on the query path, adopt nothing; curation runs.
    Learning,
    /// Today's defaults, unchanged.
    #[default]
    Full,
}

impl LearnMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            LearnMode::Off => "off",
            LearnMode::Learning => "learning",
            LearnMode::Full => "full",
        }
    }
}

/// The one UI concern with settings of its own: the vector background.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct UiConfig {
    pub background: BackgroundConfig,
}

/// The rotating point cloud behind the pages; a page load costs a count, a changed store a scroll.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BackgroundConfig {
    pub enabled: bool,
    /// Vectors sampled per snapshot: far fewer read as noise, far more cost the phone drawing them.
    pub sample_size: usize,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_size: 2000,
        }
    }
}

/// Recording real searches so they can be judged later: only an earnest search is worth scoring.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FeedbackConfig {
    /// Candidates stored per event; wider than the answer so a buried hit can be confirmed later.
    pub candidates: usize,
    /// Window in which the same searcher's next query replaces the previous event; `0` is off.
    /// Sized for a typing burst: a longer one erases finished searches.
    pub coalesce_secs: i64,
    pub retain_days: i64,
    /// The share of searches that lift the top unshown candidate into the last row; `0.0` is off.
    /// Without it the tuner only ever learns about what the ranking already showed.
    pub explore: f32,
    /// How often the retention sweep runs, in hours.
    pub sweep_hours: u64,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            candidates: 20,
            coalesce_secs: 5,
            retain_days: 0,
            explore: 0.1,
            sweep_hours: 6,
        }
    }
}

/// Links learned from co-retrieval: co-appearance `+1`, confirmation `+2`, 30-day half-life.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AssociateConfig {
    pub interval_mins: u64,
    pub half_life_days: f64,
    pub prune_below: f64,
    pub show_min: f64,
    pub judge_min: f64,
    /// Distinct binding questions a link needs before it is judged.
    pub judge_min_queries: i64,
    pub judge_per_sweep: i64,
    pub spread_from: usize,
    pub spread_max: usize,
    /// How much more activated a hit must be than the one above to pass it: a fraction of the list.
    pub prime_margin: f64,
    /// Positions a hit may climb; `0` turns priming off. The starting rung the idle pass moves.
    pub prime_lift: usize,
}

impl Default for AssociateConfig {
    fn default() -> Self {
        Self {
            interval_mins: 30,
            half_life_days: 30.0,
            prune_below: 0.5,
            show_min: 2.0,
            judge_min: 4.0,
            judge_min_queries: 3,
            judge_per_sweep: 10,
            spread_from: 3,
            spread_max: default_spread_max(),
            prime_margin: 0.5,
            prime_lift: default_prime_lift(),
        }
    }
}

/// When a passage has earned a synthesis call, and an eager artifact a second one.
/// `activation_above` is above the decayed capture baseline: `3.0` is one confirmation.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PromoteConfig {
    pub activation_above: f64,
    /// Cosine at or above which an artifact counts as traceable to a passage it covers by span but
    /// copied no line of. Not the dedupe threshold; above 1.0 switches it off.
    pub traceable_min: f32,
}

impl Default for PromoteConfig {
    fn default() -> Self {
        Self {
            activation_above: 3.0,
            traceable_min: 0.75,
        }
    }
}

/// What a pursuit (a coherent run of searches) may earn: one generated artifact. Behind `[learn]`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PursuitConfig {
    pub idle_secs: u64,
    pub min_sources: usize,
    pub min_engagement: f64,
}

impl Default for PursuitConfig {
    fn default() -> Self {
        Self {
            idle_secs: 900,
            min_sources: 2,
            min_engagement: 3.0,
        }
    }
}

/// How accessible an artifact is. Being surfaced because of activation raises nothing, so no
/// loop reinforces itself.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ActivationConfig {
    pub half_life_days: f64,
    /// Returned by a search marked as seen. Zero: being listed is exposure, not use.
    pub retrieved: f64,
    /// Opened in the detail pane; the unit the others are measured in.
    pub opened: f64,
    /// Judged the answer to a real question; the strong signal.
    pub confirmed: f64,
    /// Cited by an answer: a model's use, unverified, so it weighs what an open weighs.
    pub cited: f64,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            half_life_days: 14.0,
            retrieved: 0.0,
            opened: 1.0,
            confirmed: 3.0,
            cited: 0.5,
        }
    }
}

/// The live sitting: what this session has touched, carried between the doors.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SittingConfig {
    /// Let what this sitting touched lift a result; the starting rung, after which the live value
    /// is the generation's. Off when unset so upgrades move nothing; the example file ships it on.
    pub prime: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for SittingConfig {
    fn default() -> Self {
        // Spelled out, not derived: `false` is a decision, documented on `prime`.
        Self { prime: false }
    }
}

/// How much a quiet base may do on its own: ranking generations revert exactly, corpus writes
/// only have an undo, hence stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Autonomy {
    /// The idle pass does not run. Integration still files and writes probes.
    Off,
    /// Integrate, rehearse, watch; adopt and revert ranking generations.
    Ranking,
    /// Everything above, plus the corpus rules; `Config::normalize` always sets this.
    #[default]
    Full,
}

impl Autonomy {
    pub fn as_str(self) -> &'static str {
        match self {
            Autonomy::Off => "off",
            Autonomy::Ranking => "ranking",
            Autonomy::Full => "full",
        }
    }
    pub fn moves_ranking(self) -> bool {
        matches!(self, Autonomy::Ranking | Autonomy::Full)
    }
    /// The corpus rules may act: retract, interference, condense.
    pub fn acts_on_corpus(self) -> bool {
        matches!(self, Autonomy::Full)
    }
}

impl<'de> Deserialize<'de> for Autonomy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Autonomy;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#"true, false, "off", "ranking" or "full""#)
            }
            // A file from before the stages: `true` was the whole loop.
            fn visit_bool<E: serde::de::Error>(self, b: bool) -> std::result::Result<Autonomy, E> {
                Ok(if b { Autonomy::Full } else { Autonomy::Off })
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Autonomy, E> {
                match s {
                    "off" => Ok(Autonomy::Off),
                    "ranking" => Ok(Autonomy::Ranking),
                    "full" => Ok(Autonomy::Full),
                    // Env values arrive as strings: `ENGRAM__EVOLVE__AUTONOMOUS=false` lands here.
                    "true" => Ok(Autonomy::Full),
                    "false" => Ok(Autonomy::Off),
                    other => Err(E::unknown_variant(other, &["off", "ranking", "full"])),
                }
            }
        }
        d.deserialize_any(V)
    }
}

/// Recording what use leaves behind; nothing here changes what a search or ask returns.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct EvolveConfig {
    /// Seconds after an unopened search within which another search is a weak negative; `0` is off.
    /// Far above `feedback.coalesce_secs`: this is a person reading and asking again.
    pub give_up_window_secs: i64,
    /// How much a quiet base may do alone; always `"full"`. Older values still load, and
    /// `Config::normalize` logs and overrides them.
    pub autonomous: Autonomy,
    /// Corpus actions each job may take per 24 hours; a pace, not a cap. Undos are not counted;
    /// `0` stops the corpus jobs acting.
    pub max_actions_per_day: u32,
    #[serde(default)]
    max_actions_per_week: Option<u32>,
    /// How long a base must be quiet (no search, no question) before the idle pass runs.
    pub idle_secs: i64,
}

impl Default for EvolveConfig {
    fn default() -> Self {
        Self {
            give_up_window_secs: 300,
            autonomous: Autonomy::Full,
            max_actions_per_day: 20,
            max_actions_per_week: None,
            idle_secs: 1800,
        }
    }
}

/// A sense of time: what counts as due, what the front page calls coming up.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TimeConfig {
    /// A due reminder inside this many hours is shown on the front page and may lift a hit.
    pub horizon_hours: u64,
    pub coming_up_days: u64,
    pub lift: bool,
    pub default_tz: String,
}

impl Default for TimeConfig {
    fn default() -> Self {
        Self {
            horizon_hours: 48,
            coming_up_days: 7,
            lift: true,
            default_tz: String::new(),
        }
    }
}

/// The reap sweep: bury long-retired artifacts the live base already states, rewrite the rest.
/// Runs only in a declared `[reap]` block with a judge; see `Config::disable_undeclared_reap`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ReapConfig {
    pub enabled: bool,
    pub interval_mins: u64,
    pub min_age_days: u64,
    /// Model calls per run: the sweep's whole inference budget.
    pub max_judged_per_run: u64,
    /// Rescue rewrites per run, so one bad batch cannot flood the base with model-written text.
    pub max_rescues_per_run: u64,
}

impl Default for ReapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_mins: 1440,
            min_age_days: 90,
            max_judged_per_run: 20,
            max_rescues_per_run: 3,
        }
    }
}

/// What each named block of the context vector is worth: blocks are normalised, then scaled, so
/// a block contributes its weight whatever its dimensions.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BlockWeights {
    /// The hour, as an angle. See `core::context::encode`.
    pub time_of_day: f32,
    pub weekday: f32,
    /// The gradual part of the weekday, kept apart from the one-hot and kept weak.
    pub weekend: f32,
    pub device: f32,
    pub viewport: f32,
    pub locale: f32,
    pub network: f32,
    pub power: f32,
    pub environment: f32,
    /// Off: no monthly rhythm has shown up yet, and a zero block costs only two dimensions.
    pub month_cycle: f32,
}

impl Default for BlockWeights {
    fn default() -> Self {
        Self {
            time_of_day: 1.0,
            weekday: 1.0,
            weekend: 0.3,
            device: 0.8,
            viewport: 0.4,
            locale: 0.3,
            network: 0.6,
            power: 0.2,
            environment: 0.2,
            month_cycle: 0.0,
        }
    }
}

impl BlockWeights {
    /// The weight of a block by name; an unknown name is worth zero, so a typo adds no weight.
    pub fn of(&self, block: &str) -> f32 {
        match block {
            "time_of_day" => self.time_of_day,
            "weekday" => self.weekday,
            "weekend" => self.weekend,
            "device" => self.device,
            "viewport" => self.viewport,
            "locale" => self.locale,
            "network" => self.network,
            "power" => self.power,
            "environment" => self.environment,
            "month_cycle" => self.month_cycle,
            _ => 0.0,
        }
    }
}

/// Offering an artifact before it is asked for, from the situation the page was opened in.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RecommendConfig {
    /// On; a new base sees the random card until weekly patterns form. Needs `learn.enabled`.
    pub enabled: bool,
    pub cluster_merge_at: f32,
    /// Clusters per (scope, artifact): two situations averaged are a situation that never happened.
    pub max_clusters: usize,
    pub half_life_days: f64,
    /// A cluster below this is dropped; low enough that a single recent event survives.
    pub min_weight: f64,
    /// Weight at or above which a cluster is called established; below it the offer counts instead.
    pub firm_at: f64,
    /// Context score at or above which the offer is called a pattern.
    pub strong_at: f32,
    /// ...and above which a resemblance; below it the ladder falls to the sitting, then forgotten.
    pub weak_at: f32,
    /// What an open of an offered artifact feeds back. Zero, or a lucky guess grows into a habit.
    pub self_weight: f64,
    pub weights: BlockWeights,
}

impl Default for RecommendConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cluster_merge_at: 0.82,
            max_clusters: 5,
            half_life_days: 45.0,
            min_weight: 0.9,
            firm_at: 2.5,
            strong_at: 0.75,
            weak_at: 0.45,
            self_weight: 0.0,
            weights: BlockWeights::default(),
        }
    }
}

/// What the queue does with work nobody is waiting on.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ScheduleConfig {
    /// A background unit waiting longer than this becomes foreground, so priority cannot starve it.
    pub age_after_mins: i64,
    /// The ceiling on the doubling wait of a sweep that keeps finding nothing; new data re-arms it.
    pub backoff_max_hours: u64,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            age_after_mins: 60,
            backoff_max_hours: 24,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ConsolidateConfig {
    /// Whether the background sweep runs; capture-time near-duplicate detection always does.
    pub enabled: bool,
    /// Estimated shingle Jaccard above which a capture is parked as a near-duplicate. Measured: the
    /// real duplicate scored 0.789 and the next pair 0.036 over 3916 corpus pairs.
    pub near_dupe_min: f64,
    pub review_min: f32,
    /// Cosine at or above which a pair goes to the dedupe judge first; never a hide on its own,
    /// since embeddings barely see negation. Must exceed `review_min`.
    pub auto_supersede: f32,
    pub per_point: usize,
    pub interval_hours: u64,
    /// How often the dedupe ticker arms units, in minutes: the budget is a rate the hardware sets.
    pub dedupe_interval_mins: u64,
    /// Units armed per tick; zero switches the model off, while pairs are still found and recorded.
    pub max_dedupe_per_tick: usize,
    /// Days unverified (`last_verified_at`) before an active artifact is a deprecation candidate.
    pub stale_after_days: u32,
    /// ...and retrieved at most this many times since. Never fed into search scoring.
    pub stale_max_hits: i64,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            near_dupe_min: 0.60,
            review_min: default_review_min(),
            auto_supersede: 0.95,
            per_point: 5,
            interval_hours: 24,
            dedupe_interval_mins: 15,
            max_dedupe_per_tick: 5,
            stale_after_days: 365,
            stale_max_hits: 0,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// SHA-256 of the certificate's SubjectPublicKeyInfo, base64. Unset, the app pins on first
    /// use while pairing; set, the QR carries it. Config: TLS terminates outside this process.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<String>,
    #[serde(default)]
    pub allow_plain_http: bool,
}

fn default_trusted_proxies() -> Vec<String> {
    vec!["127.0.0.1/32".into(), "::1/128".into()]
}

impl ServerConfig {
    /// The fingerprint as the QR carries it: base64url, unpadded, 43 characters.
    pub fn fingerprint(&self) -> std::result::Result<Option<String>, String> {
        use base64::Engine;
        let Some(raw) = self.tls_fingerprint.as_deref().map(str::trim) else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Ok(None);
        }
        let cleaned: String = raw.chars().filter(|c| *c != '=').collect();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&cleaned)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
            .map_err(|_| {
                format!(
                    "server.tls_fingerprint {raw:?} is not base64: it should be the SHA-256 \
                     of the certificate's SubjectPublicKeyInfo"
                )
            })?;
        if bytes.len() != 32 {
            return Err(format!(
                "server.tls_fingerprint decodes to {} bytes, and a SHA-256 is 32",
                bytes.len()
            ));
        }
        Ok(Some(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        ))
    }
}
fn default_workers() -> usize {
    2
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct StoreConfig {
    pub control_path: String,
    pub dir: String,
    /// How many tenants may be open at once; eviction is transparent, the sitting surviving it.
    pub max_open_tenants: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            control_path: "engram-control.db".into(),
            dir: "data/users".into(),
            max_open_tenants: 32,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct VectorConfig {
    pub url: String,
    pub collection: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// How much a result's age counts against it: breaks near-ties without overturning a better
    /// match. `0.0` turns recency off.
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f32,
    #[serde(default = "default_recency_half_life_days")]
    pub recency_half_life_days: u32,
    /// Extra score for a chunk tagged `pinned`, so a deliberate choice outranks the decay curve.
    #[serde(default = "default_pinned_boost")]
    pub pinned_boost: f32,
    /// Cosine below which a result is labelled loosely related. A floor: the line is measured
    /// per base by `Core::calibrate`, since where noise lands depends on the embedder.
    #[serde(default = "default_weak_below")]
    pub weak_below: f32,
    /// Chunks one document may contribute to a result list; `0` lets one document fill it.
    #[serde(default = "default_per_source_cap")]
    pub per_source_cap: usize,
    /// Fetch multiple of the answer size when the cap or reranker will narrow it.
    #[serde(default = "default_candidate_multiplier")]
    pub candidate_multiplier: usize,
}
pub(crate) fn default_recency_weight() -> f32 {
    0.05
}
pub(crate) fn default_per_source_cap() -> usize {
    crate::core::search::MAX_PER_CORPUS
}
pub(crate) fn default_recency_half_life_days() -> u32 {
    180
}
pub(crate) fn default_candidate_multiplier() -> usize {
    crate::core::search::CANDIDATE_MULTIPLIER
}
/// The shipped `associate.prime_lift`: one place, as in `config.example.toml`.
pub(crate) fn default_prime_lift() -> usize {
    1
}
/// What a generation row from before the knob ran under: priming off.
pub(crate) fn legacy_prime_lift() -> usize {
    0
}
pub(crate) fn default_spread_max() -> usize {
    3
}
/// The shipped `sitting.prime`: off, so an older generation row decodes as what it ran under.
pub(crate) fn default_sitting_prime() -> bool {
    false
}
/// A generation row from before the rerank knob ran with whatever reranker the file named.
pub(crate) fn default_rerank_knob() -> bool {
    true
}
pub(crate) fn default_review_min() -> f32 {
    0.88
}
fn default_pinned_boost() -> f32 {
    0.15
}
fn default_weak_below() -> f32 {
    0.35
}

/// A named endpoint and its defaults, so roles point at one instead of repeating it.
#[derive(Debug, Deserialize, Clone)]
pub struct TierConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    #[serde(default = "default_true")]
    pub structured_output: bool,
}

/// The retrieval unit, a target fixed rather than derived from the embedder's capacity.
pub const DEFAULT_CHUNK_TOKENS: usize = 384;

fn default_chunk_tokens() -> usize {
    DEFAULT_CHUNK_TOKENS
}

/// The resolved roles, deserialised through [`RawInferConfig`] so tiers are flattened away.
#[derive(Debug, Deserialize, Clone)]
#[serde(try_from = "RawInferConfig")]
pub struct InferConfig {
    /// Path or http(s) URL of a HF-format tokenizer.json; unset uses the bundled Qwen default.
    pub tokenizer: Option<String>,
    /// `None` is the embed-only floor: verbatim passages, and chat-model surfaces say so.
    pub synthesize: Option<SynthesizeRole>,
    pub embed: EmbedRole,
    /// `None` closes the ask door: no page, no nav entry, no tool.
    pub ask: Option<AskRole>,
    pub rerank: Option<RerankRole>,
    pub vision: Option<VisionRole>,
    /// The speech model behind the search-box microphone; `None` removes the button.
    pub transcribe: Option<TranscribeRole>,
}

/// The file's shape, before tiers fold into the roles; endpoint fields default from the tier.
#[derive(Debug, Deserialize)]
pub struct RawInferConfig {
    #[serde(default)]
    tiers: HashMap<String, TierConfig>,
    #[serde(default)]
    tokenizer: Option<String>,
    #[serde(default)]
    synthesize: Option<RawSynthesizeRole>,
    embed: EmbedRole,
    #[serde(default)]
    ask: Option<RawAskRole>,
    #[serde(default)]
    rerank: Option<RerankRole>,
    #[serde(default)]
    vision: Option<RawVisionRole>,
    #[serde(default)]
    transcribe: Option<TranscribeRole>,
}

#[derive(Debug, Deserialize)]
struct RawSynthesizeRole {
    tier: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    context_tokens: Option<usize>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    ceiling_param: Option<CeilingParam>,
    #[serde(default)]
    structured_output: Option<bool>,
    #[serde(default = "default_output_ratio")]
    output_ratio: f32,
    #[serde(default = "default_context_opening_tokens")]
    context_opening_tokens: usize,
    #[serde(default = "default_context_overlap_tokens")]
    context_overlap_tokens: usize,
    #[serde(default = "default_context_neighbor_tokens")]
    context_neighbor_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct RawAskRole {
    tier: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    context_tokens: Option<usize>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    ceiling_param: Option<CeilingParam>,
    /// Formerly `follow_up`, still accepted so a renamed key does not silently revert.
    #[serde(default = "default_plan", alias = "follow_up")]
    plan: bool,
    #[serde(default, alias = "follow_up_tier")]
    plan_tier: Option<String>,
}

/// On: a question spanning several subjects is retrieved for each, at one cheap call.
fn default_plan() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawVisionRole {
    model: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    ceiling_param: Option<CeilingParam>,
}

/// The endpoint a role runs on: the tier it names. An unknown name is refused, not defaulted.
fn resolve_endpoint(
    role: &str,
    tier_name: &str,
    tiers: &HashMap<String, TierConfig>,
) -> Result<TierConfig, String> {
    tiers.get(tier_name).cloned().ok_or_else(|| {
        let mut known: Vec<&str> = tiers.keys().map(String::as_str).collect();
        known.sort_unstable();
        format!(
            "[infer.{role}] points at tier `{tier_name}`, which is not defined. \
             Known tiers: {}. Define it under [infer.tiers.{tier_name}].",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )
    })
}

impl TryFrom<RawInferConfig> for InferConfig {
    type Error = String;

    fn try_from(raw: RawInferConfig) -> Result<Self, Self::Error> {
        let tiers = &raw.tiers;

        let synthesize = match raw.synthesize {
            None => None,
            Some(s) => {
                let st = resolve_endpoint("synthesize", &s.tier, tiers)?;
                Some(SynthesizeRole {
                    base_url: s.base_url.unwrap_or(st.base_url),
                    model: s.model.unwrap_or(st.model),
                    api_key: s.api_key.or(st.api_key),
                    context_tokens: s.context_tokens.unwrap_or(st.context_tokens),
                    max_output_tokens: s.max_output_tokens.unwrap_or(st.max_output_tokens),
                    output_ratio: s.output_ratio,
                    reasoning_effort: s.reasoning_effort.or(st.reasoning_effort),
                    ceiling_param: s.ceiling_param.or(st.ceiling_param),
                    timeout_secs: s.timeout_secs.unwrap_or(st.timeout_secs),
                    structured_output: s.structured_output.unwrap_or(st.structured_output),
                    context_opening_tokens: s.context_opening_tokens,
                    context_overlap_tokens: s.context_overlap_tokens,
                    context_neighbor_tokens: s.context_neighbor_tokens,
                })
            }
        };

        let ask = match raw.ask {
            None => None,
            Some(a) => {
                let at = resolve_endpoint("ask", &a.tier, tiers)?;
                // Resolved here, so a typo fails startup rather than the first question.
                let plan_endpoint = match a.plan_tier.as_deref() {
                    Some(name) => Some(resolve_endpoint("ask.plan_tier", name, tiers)?),
                    None => None,
                };
                Some(AskRole {
                    base_url: a.base_url.unwrap_or(at.base_url),
                    model: a.model.unwrap_or(at.model),
                    api_key: a.api_key.or(at.api_key),
                    context_tokens: a.context_tokens.unwrap_or(at.context_tokens),
                    max_output_tokens: a.max_output_tokens.unwrap_or(at.max_output_tokens),
                    timeout_secs: a.timeout_secs.unwrap_or(at.timeout_secs),
                    reasoning_effort: a.reasoning_effort.or(at.reasoning_effort),
                    ceiling_param: a.ceiling_param.or(at.ceiling_param),
                    structured_output: at.structured_output,
                    plan: a.plan,
                    plan_endpoint,
                })
            }
        };

        // Vision may lack an endpoint (it borrows synthesize's), so it is folded by hand.
        let vision = match raw.vision {
            None => None,
            Some(v) => {
                let vt = match v.tier.as_deref() {
                    Some(name) => Some(resolve_endpoint("vision", name, tiers)?),
                    None => None,
                };
                Some(VisionRole {
                    model: v.model,
                    base_url: v
                        .base_url
                        .or_else(|| vt.as_ref().map(|t| t.base_url.clone())),
                    api_key: v
                        .api_key
                        .or_else(|| vt.as_ref().and_then(|t| t.api_key.clone())),
                    // The tier's timeout only where there is a tier; the role's suits one image.
                    timeout_secs: v
                        .timeout_secs
                        .or_else(|| vt.as_ref().map(|t| t.timeout_secs))
                        .unwrap_or_else(default_vision_timeout_secs),
                    // Not inherited: a tier's ceiling fits its role, and a description is stored.
                    max_output_tokens: v
                        .max_output_tokens
                        .unwrap_or_else(default_vision_max_output_tokens),
                    ceiling_param: v
                        .ceiling_param
                        .or_else(|| vt.as_ref().and_then(|t| t.ceiling_param)),
                })
            }
        };

        // With no synthesize role there is nothing to borrow, and vision would call "".
        if let Some(v) = &vision
            && v.base_url.is_none()
            && synthesize.is_none()
        {
            return Err(
                "[infer.vision] has no `base_url` or `tier` of its own and there is no \
                 [infer.synthesize] for it to borrow one from. Give it an address, or \
                 configure a synthesize role."
                    .to_string(),
            );
        }

        Ok(InferConfig {
            tokenizer: raw.tokenizer,
            synthesize,
            embed: raw.embed,
            ask,
            rerank: raw.rerank,
            vision,
            // Never borrows synthesize's endpoint: speech and chat are different servers.
            transcribe: raw.transcribe,
        })
    }
}

/// Seconds an inference request may take: one segmentation window on a consumer GPU measured
/// seven minutes, and a timeout there retries forever. Hosted endpoints should lower it.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

#[derive(Debug, Deserialize, Clone)]
pub struct SynthesizeRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
    /// Sent as `reasoning_effort` when set; thinking spends the output budget the JSON must fit in.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Which name this endpoint takes the output ceiling under; unset infers it, a 400 corrects.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Constrain decoding with the reply's JSON Schema: a 9B model's malformed JSON fails windows
    /// for good. On by default; also governs the dedupe judge.
    #[serde(default = "default_true")]
    pub structured_output: bool,
    /// Tokens of the document's opening prepended to every window, for context; zero disables.
    #[serde(default = "default_context_opening_tokens")]
    pub context_opening_tokens: usize,
    /// Tokens of each neighbouring window carried on both sides; zero disables.
    #[serde(default = "default_context_overlap_tokens")]
    pub context_overlap_tokens: usize,
    /// Tokens of nearest-neighbour artifacts shown to a judged capture call; zero disables.
    #[serde(default = "default_context_neighbor_tokens")]
    pub context_neighbor_tokens: usize,
}

fn default_true() -> bool {
    true
}

/// The example file's number, so a minimal config starts; sized for a small local model.
fn default_output_ratio() -> f32 {
    8.0
}

fn default_context_opening_tokens() -> usize {
    200
}

fn default_context_overlap_tokens() -> usize {
    150
}

fn default_context_neighbor_tokens() -> usize {
    1024
}

#[derive(Debug, Deserialize, Clone)]
pub struct EmbedRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub dim: usize,
    pub max_input_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// See `EmbedTemplates`; flat on the role so the TOML reads beside `model`.
    #[serde(default = "default_query_template")]
    pub query_template: String,
    #[serde(default = "default_document_template")]
    pub document_template: String,
    #[serde(default = "default_document_template_untitled")]
    pub document_template_untitled: String,
    /// Passage size at `synthesis = "off"`/`"earned"` in estimator tokens, clamped to the embedder.
    #[serde(default = "default_chunk_tokens")]
    pub chunk_tokens: usize,
}

impl EmbedRole {
    /// What a stored vector means: model, dimension and the three templates, one identity.
    /// Recorded at boot so a change can be said; the answer to one is a re-capture.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(
            format!(
                "{}\n{}\n{}\n{}\n{}",
                self.model,
                self.dim,
                self.query_template,
                self.document_template,
                self.document_template_untitled,
            )
            .as_bytes(),
        ))
    }

    /// `chunk_tokens`, never above `max_input_tokens * 0.8`. The rendered envelope is not counted;
    /// `split_oversize` cuts against it.
    pub fn effective_chunk_tokens(&self) -> usize {
        let ceiling = (self.max_input_tokens as f32 * 0.8) as usize;
        self.chunk_tokens.min(ceiling).max(1)
    }

    pub fn templates(&self) -> EmbedTemplates {
        EmbedTemplates {
            query_template: self.query_template.clone(),
            document_template: self.document_template.clone(),
            document_template_untitled: self.document_template_untitled.clone(),
        }
    }
}

/// The three strings that, with `model`, fix what a stored vector means (EmbeddingGemma is
/// asymmetric). Changing one invalidates every vector; `legacy()` is the symmetric block.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedTemplates {
    pub query_template: String,
    pub document_template: String,
    /// `{text}` only; used when the document has no title.
    pub document_template_untitled: String,
}

pub(crate) fn default_query_template() -> String {
    "task: search result | query: {text}".into()
}
pub(crate) fn default_document_template() -> String {
    "title: {title} | text: {text}".into()
}
pub(crate) fn default_document_template_untitled() -> String {
    "title: none | text: {text}".into()
}

impl Default for EmbedTemplates {
    fn default() -> Self {
        Self {
            query_template: default_query_template(),
            document_template: default_document_template(),
            document_template_untitled: default_document_template_untitled(),
        }
    }
}

impl EmbedTemplates {
    /// Title line above the body, query as typed: for a symmetric embedder and every test double.
    pub fn legacy() -> Self {
        Self {
            query_template: "{text}".into(),
            document_template: "{title}\n{text}".into(),
            document_template_untitled: "{text}".into(),
        }
    }

    pub fn render_query(&self, query: &str) -> String {
        substitute(&self.query_template, &[("text", query)])
    }

    pub fn render_document(&self, title: Option<&str>, text: &str) -> String {
        match title {
            Some(t) => substitute(&self.document_template, &[("title", t), ("text", text)]),
            None => substitute(&self.document_template_untitled, &[("text", text)]),
        }
    }

    /// Every template must carry what it is for: without `{text}` every document embeds alike.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.query_template.contains("{text}") {
            return Err("infer.embed.query_template must contain {text}".into());
        }
        if !self.document_template.contains("{text}") {
            return Err("infer.embed.document_template must contain {text}".into());
        }
        if !self.document_template.contains("{title}") {
            return Err("infer.embed.document_template must contain {title}; \
                        use document_template_untitled for the case with no title"
                .into());
        }
        if !self.document_template_untitled.contains("{text}") {
            return Err("infer.embed.document_template_untitled must contain {text}".into());
        }
        Ok(())
    }
}

/// Fill `{name}` placeholders in one pass, so a value containing a placeholder lands verbatim.
/// An unknown `{name}` is left as written.
pub(crate) fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Deserialize, Clone)]
pub struct AskRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    /// Hard cap on output tokens per answer; sent because an endpoint with none applies its own.
    #[serde(default = "default_ask_max_output_tokens")]
    pub max_output_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    /// See `TierConfig::structured_output`: the planning call sends a response format.
    pub structured_output: bool,
    /// Whether one round of planned, fanned-out retrieval follows the first.
    pub plan: bool,
    /// The resolved endpoint for the planning call, from `plan_tier`; `None` uses this role's.
    pub plan_endpoint: Option<TierConfig>,
}

impl AskRole {
    /// The endpoint the planning call runs on, falling back at config time to this one as its tier
    /// described it, `structured_output` included.
    pub fn plan_on(&self) -> TierConfig {
        self.plan_endpoint.clone().unwrap_or_else(|| TierConfig {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            context_tokens: self.context_tokens,
            max_output_tokens: self.max_output_tokens,
            timeout_secs: self.timeout_secs,
            reasoning_effort: self.reasoning_effort.clone(),
            ceiling_param: self.ceiling_param,
            structured_output: self.structured_output,
        })
    }
}

fn default_ask_max_output_tokens() -> usize {
    4096
}

/// The request field an endpoint takes the output ceiling under. llama.cpp and vLLM silently
/// ignore an unknown one, leaving no ceiling; unset infers it from `reasoning_effort`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CeilingParam {
    MaxTokens,
    MaxCompletionTokens,
}

impl CeilingParam {
    /// The ceiling name when the operator has not said. Ambiguous `"none"` reads as `max_tokens`:
    /// a hosted endpoint corrects it with a 400, a local one would silently run unbounded.
    pub fn inferred_from(effort: Option<&str>) -> Self {
        match effort.map(str::trim) {
            None => Self::MaxTokens,
            Some(e) if e.eq_ignore_ascii_case("none") => Self::MaxTokens,
            Some(_) => Self::MaxCompletionTokens,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    /// The other name: what a rejected ceiling is retried under.
    pub fn flipped(self) -> Self {
        match self {
            Self::MaxTokens => Self::MaxCompletionTokens,
            Self::MaxCompletionTokens => Self::MaxTokens,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RerankRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub style: RerankStyle,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Where the reranker is consulted; both by default, `apply = ["ask"]` spares search latency.
    #[serde(default = "default_rerank_apply")]
    pub apply: Vec<RerankApply>,
}

impl RerankRole {
    pub fn applies_to(&self, place: RerankApply) -> bool {
        self.apply.contains(&place)
    }
}

fn default_rerank_apply() -> Vec<RerankApply> {
    vec![RerankApply::Ask, RerankApply::Search]
}

/// A place the reranker runs: `Ask` behind a question, `Search` for every list a caller sees.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RerankApply {
    Ask,
    Search,
}

/// The vision model that reads a captured image; absent closes the image door. Endpoint fields
/// default to the synthesize role's.
#[derive(Debug, Deserialize, Clone)]
pub struct VisionRole {
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_vision_timeout_secs")]
    pub timeout_secs: u64,
    /// Hard cap on output tokens per description, sent every call; the reply is stored as a corpus.
    #[serde(default = "default_vision_max_output_tokens")]
    pub max_output_tokens: usize,
    /// See `SynthesizeRole::ceiling_param`; inherited when borrowing the synthesize endpoint.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
}

fn default_vision_timeout_secs() -> u64 {
    120
}

fn default_vision_max_output_tokens() -> usize {
    4096
}

impl VisionRole {
    /// The endpoint and key this role calls: its own where given, the synthesize role's otherwise.
    pub fn resolve(&self, synth: Option<&SynthesizeRole>) -> (String, Option<String>) {
        (
            self.base_url
                .clone()
                .or_else(|| synth.map(|s| s.base_url.clone()))
                // Unreachable after `Config::load`, which refuses a vision role with neither.
                .unwrap_or_default(),
            self.api_key
                .clone()
                .or_else(|| synth.and_then(|s| s.api_key.clone())),
        )
    }

    /// The ceiling name, inherited only when this role borrows the synthesize endpoint.
    pub fn ceiling_param(&self, synth: Option<&SynthesizeRole>) -> Option<CeilingParam> {
        self.ceiling_param.or_else(|| {
            self.base_url
                .is_none()
                .then(|| synth.and_then(|s| s.ceiling_param))
                .flatten()
        })
    }

    /// The effort the ceiling name is inferred from, inherited as [`Self::ceiling_param`] is,
    /// so two roles on one server guess alike. Never sent by this role.
    pub fn inherited_reasoning_effort<'a>(
        &self,
        synth: Option<&'a SynthesizeRole>,
    ) -> Option<&'a str> {
        self.base_url
            .is_none()
            .then(|| synth.and_then(|s| s.reasoning_effort.as_deref()))
            .flatten()
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RerankStyle {
    Tei,
    Cohere,
    Vllm,
}

impl RerankStyle {
    /// Where the startup probe checks: the same prefix as the style's request path (vLLM
    /// `v1/models`, Cohere `models`, TEI `info`).
    pub fn probe_path(self) -> &'static str {
        match self {
            RerankStyle::Tei => "info",
            RerankStyle::Cohere => "models",
            RerankStyle::Vllm => "v1/models",
        }
    }
}

/// The speech-to-text model behind the search-box microphone (OpenAI `audio/transcriptions`);
/// both endpoint fields are required, as there is nothing to borrow.
#[derive(Debug, Deserialize, Clone)]
pub struct TranscribeRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// A minute: a person is holding a button.
    #[serde(default = "default_transcribe_timeout_secs")]
    pub timeout_secs: u64,
    /// An ISO-639-1 code sent with every recording; whisper misdetects language on short queries.
    #[serde(default)]
    pub language: Option<String>,
}

fn default_transcribe_timeout_secs() -> u64 {
    60
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub mode: AuthMode,
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
    #[serde(default)]
    pub local: Option<LocalConfig>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    Oidc,
    Local,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Admit everyone the identity provider authenticates. Off: each new subject provisions a
    /// tenant, so open self-registration is unbounded. Ignored when any list below is set.
    #[serde(default)]
    pub open_registration: bool,
    /// Subjects admitted by name; see [`crate::auth::oidc::is_allowed`].
    #[serde(default)]
    pub allowed_subs: Vec<String>,
    /// Group names from the provider's `groups` claim (Nextcloud sends it with group provisioning).
    #[serde(default)]
    pub allowed_groups: Vec<String>,
}
fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

#[derive(Debug, Deserialize, Clone)]
pub struct LocalConfig {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config: {0}")]
    Load(#[from] config::ConfigError),
    #[error("config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut builder = config::Config::builder();
        if let Some(p) = path {
            builder = builder.add_source(config::File::from(p).required(true));
        } else {
            builder = builder.add_source(config::File::with_name("config").required(false));
        }
        let raw = builder
            .add_source(
                config::Environment::with_prefix("ENGRAM")
                    .separator("__")
                    .list_separator(","),
            )
            .build()?;
        let raw = Self::read_secret_files(raw)?;
        Self::refuse_removed_keys(&raw)?;
        let mut cfg: Config = raw.clone().try_deserialize()?;
        cfg.apply_learn_mode(&raw);
        cfg.normalize();
        cfg.validate()?;
        cfg.warn_on_file_secrets(path);
        cfg.warn_on_defaulted_store(&raw);
        cfg.disable_undeclared_reap(&raw);
        cfg.warn_on_inert_settings();
        cfg.warn_on_inferred_ceiling_param();
        cfg.warn_on_unplaced_plan_cost();
        Ok(cfg)
    }

    /// `ENGRAM__A__B_FILE=/path` sets `a.b` to that file's contents, and any
    /// string value `file:///path` is replaced by the contents of `/path`.
    fn read_secret_files(raw: config::Config) -> Result<config::Config, ConfigError> {
        fn read(path: &str, key: &str) -> Result<String, ConfigError> {
            std::fs::read_to_string(path)
                .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
                .map_err(|e| ConfigError::Invalid(format!("cannot read {key} from {path}: {e}")))
        }
        fn walk(
            prefix: &str,
            v: &serde_json::Value,
            out: &mut Vec<(String, String)>,
        ) -> Result<(), ConfigError> {
            match v {
                serde_json::Value::Object(m) => {
                    for (k, v) in m {
                        let key = if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        };
                        walk(&key, v, out)?;
                    }
                }
                serde_json::Value::String(s) => {
                    if let Some(path) = s.strip_prefix("file://") {
                        out.push((prefix.to_string(), read(path, prefix)?));
                    }
                }
                _ => {}
            }
            Ok(())
        }
        let mut overrides = Vec::new();
        for (name, path) in std::env::vars() {
            let Some(rest) = name.strip_prefix("ENGRAM__") else {
                continue;
            };
            let Some(key) = rest.strip_suffix("_FILE") else {
                continue;
            };
            let key = key.to_ascii_lowercase().replace("__", ".");
            overrides.push((key.clone(), read(&path, &key)?));
        }
        let tree: serde_json::Value = raw.clone().try_deserialize()?;
        walk("", &tree, &mut overrides)?;
        if overrides.is_empty() {
            return Ok(raw);
        }
        let mut b = config::Config::builder().add_source(raw);
        for (k, v) in overrides {
            b = b.set_override(k, v)?;
        }
        Ok(b.build()?)
    }

    pub fn deployment(&self) -> Result<crate::web::security::Deployment, ConfigError> {
        use std::net::ToSocketAddrs;
        let bind = self
            .server
            .bind
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "server.bind {:?} is not an address",
                    self.server.bind
                ))
            })?;
        let trusted_proxies = self
            .server
            .trusted_proxies
            .iter()
            .map(|p| {
                p.parse().map_err(|_| {
                    ConfigError::Invalid(format!("server.trusted_proxies: {p:?} is not a network"))
                })
            })
            .collect::<Result<Vec<ipnet::IpNet>, _>>()?;
        let public_url = match self.server.public_url.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(u) => Some(url::Url::parse(u).map_err(|_| {
                ConfigError::Invalid(format!("server.public_url {u:?} is not a URL"))
            })?),
        };
        let dep = crate::web::security::Deployment {
            public_url,
            trusted_proxies,
            allow_plain_http: self.server.allow_plain_http,
            bind,
        };
        match &dep.public_url {
            None if !bind.ip().is_loopback() => Err(ConfigError::Invalid(
                "server.public_url is required when server.bind is not loopback.".into(),
            )),
            Some(u) if !matches!(u.scheme(), "https" | "http") => Err(ConfigError::Invalid(
                "server.public_url must be an http(s) URL.".into(),
            )),
            Some(_) if dep.plain_http_remote() && !dep.allow_plain_http => {
                Err(ConfigError::Invalid(
                    "server.public_url is plain http; use https or set server.allow_plain_http."
                        .into(),
                ))
            }
            _ => Ok(dep),
        }
    }

    /// Keys that no longer exist, refused rather than ignored: a dropped `[feedback] enabled =
    /// false` would silently record again, and the old flags map to no single `[learn]` value.
    fn refuse_removed_keys(raw: &config::Config) -> Result<(), ConfigError> {
        // `promote.resynthesize_after_unconfirmed` is merely ignored: it shipped disabled.
        const RESHAPED: [&str; 2] = ["infer.synthesis", "infer.segment_tokens"];
        let found: Vec<String> = RESHAPED
            .iter()
            .filter(|key| raw.get::<config::Value>(key).is_ok())
            .map(|key| format!("  {key}"))
            .collect();
        if !found.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "this config sets keys removed in the 2026-09 capture reshape:\n{}\nThere are \
                 no synthesis modes any more: capture stores verbatim passages first and, \
                 with [infer.synthesize] configured, synthesizes what fits one call while \
                 larger corpora earn synthesis through use. Delete the keys.\n\nA base that \
                 ran `synthesis = \"off\"` leaves [infer.synthesize] out instead: every \
                 capture then stays verbatim. config.example.toml has both shapes.",
                found.join("\n")
            )));
        }

        if raw.get::<config::Value>("auth.oidc.allowed_emails").is_ok() {
            return Err(ConfigError::Invalid(
                "auth.oidc.allowed_emails is gone; list subjects in allowed_subs or use allowed_groups."
                    .into(),
            ));
        }
        if raw.get::<config::Value>("auth.oidc.redirect_url").is_ok() {
            return Err(ConfigError::Invalid(
                "auth.oidc.redirect_url is gone; set server.public_url.".into(),
            ));
        }

        const REMOVED: [(&str, &str); 3] = [
            ("feedback.enabled", "[learn] enabled"),
            ("associate.enabled", "[learn] enabled"),
            ("pursuit.enabled", "[learn] enabled"),
        ];
        let found: Vec<String> = REMOVED
            .iter()
            .filter(|(key, _)| raw.get::<config::Value>(key).is_ok())
            .map(|(key, now)| format!("  {key} — see {now}"))
            .collect();
        if found.is_empty() {
            return Ok(());
        }
        Err(ConfigError::Invalid(format!(
            "this config sets keys that no longer exist:\n{}\nRemove them, or set [learn] to \
             what you mean. They were three switches over one faculty and are one switch now; \
             an upgrade cannot guess which of them you meant, and ignoring them would turn a \
             setting that says \"keep none of this\" into a setting that says nothing.",
            found.join("\n")
        )))
    }

    /// The dial, resolved to the keys it stands for, after deserialization and before validation.
    /// A key set in the file or environment is never touched; `full` decides nothing.
    fn apply_learn_mode(&mut self, raw: &config::Config) {
        let mut resolved: Vec<(&'static str, String)> = Vec::new();
        macro_rules! resolve {
            ($key:literal, $field:expr, $value:expr) => {
                if raw.get::<config::Value>($key).is_err() {
                    $field = $value;
                    resolved.push(($key, format!("{}", $field)));
                }
            };
        }
        match self.learn.mode {
            LearnMode::Full => {}
            LearnMode::Off | LearnMode::Learning => {
                // Nothing reads the log on the query path: no priming, spread or sitting lift.
                resolve!("associate.spread_max", self.associate.spread_max, 0);
                resolve!("associate.prime_lift", self.associate.prime_lift, 0);
                resolve!("sitting.prime", self.sitting.prime, false);
                // Promotion is gated on a threshold, so "never" is one no activation reaches.
                resolve!(
                    "promote.activation_above",
                    self.promote.activation_above,
                    f64::INFINITY
                );
                // Neither mode may grow the corpus from what was recorded.
                resolve!(
                    "pursuit.min_engagement",
                    self.pursuit.min_engagement,
                    f64::INFINITY
                );
            }
        }
        match self.learn.mode {
            LearnMode::Off => {
                resolve!("learn.enabled", self.learn.enabled, false);
                // Capture-time near-duplicate detection stays on; the background sweep stops.
                resolve!("consolidate.enabled", self.consolidate.enabled, false);
            }
            LearnMode::Learning => {
                resolve!("learn.enabled", self.learn.enabled, true);
                // Not shared with `off`: there it arms the retention sweep, and `recommends()`
                // already requires `learn.enabled`.
                resolve!("recommend.enabled", self.recommend.enabled, false);
            }
            LearnMode::Full => {}
        }
        self.learn.resolved = resolved;
    }

    /// Values that would make a feature quietly useless, put back with a log line, not refused:
    /// `feedback.candidates = 0`, and a pool wider than the widest legal search.
    fn normalize(&mut self) {
        if self.evolve.autonomous != Autonomy::Full {
            tracing::warn!(
                configured = self.evolve.autonomous.as_str(),
                "evolve.autonomous is no longer read: the base curates itself, and every \
                 action it takes has an undo on Insights"
            );
            self.evolve.autonomous = Autonomy::Full;
        }
        if self.evolve.max_actions_per_week.take().is_some() {
            tracing::warn!(
                "evolve.max_actions_per_week is no longer read; see max_actions_per_day"
            );
        }
        if self.feedback.candidates == 0 {
            let d = FeedbackConfig::default().candidates;
            self.feedback.candidates = d;
            tracing::warn!(
                using = d,
                "feedback.candidates = 0 would store an empty pool for every captured search; \
                 using the default"
            );
        }
        if self.vector.candidate_multiplier == 0 {
            let d = default_candidate_multiplier();
            self.vector.candidate_multiplier = d;
            tracing::warn!(
                using = d,
                "vector.candidate_multiplier = 0 would fetch nothing to cap; using the default"
            );
        }
        // The top rung of the multiplier ladder, not the shipped one: the pass may adopt it.
        let ceiling = crate::core::search::MAX_LIMIT
            * crate::core::ranking::MULTIPLIERS[crate::core::ranking::MULTIPLIERS.len() - 1];
        if self.feedback.candidates > ceiling {
            tracing::warn!(
                configured = self.feedback.candidates,
                using = ceiling,
                "feedback.candidates is the fetch width of every captured search; \
                 capping it at the widest ordinary search"
            );
            self.feedback.candidates = ceiling;
        }
        // These widths multiply into one SQL `LIMIT` and `interval_mins` into a `Duration`;
        // past these bounds a value is a typo, so it is clamped.
        const MAX_SPREAD_FROM: usize = 64;
        const MAX_SPREAD_MAX: usize = 64;
        const MAX_PRIME_LIFT: usize = 64;
        // A year: longer and the ticker effectively never fires again.
        const MAX_INTERVAL_MINS: u64 = 525_600;
        for (name, value, ceiling) in [
            (
                "associate.spread_from",
                &mut self.associate.spread_from,
                MAX_SPREAD_FROM,
            ),
            (
                "associate.spread_max",
                &mut self.associate.spread_max,
                MAX_SPREAD_MAX,
            ),
            (
                "associate.prime_lift",
                &mut self.associate.prime_lift,
                MAX_PRIME_LIFT,
            ),
        ] {
            if *value > ceiling {
                tracing::warn!(
                    setting = name,
                    configured = *value,
                    using = ceiling,
                    "association width is far past anything a result list can use; capping it"
                );
                *value = ceiling;
            }
        }
        if self.associate.interval_mins > MAX_INTERVAL_MINS {
            tracing::warn!(
                configured = self.associate.interval_mins,
                using = MAX_INTERVAL_MINS,
                "associate.interval_mins is longer than a year; capping it"
            );
            self.associate.interval_mins = MAX_INTERVAL_MINS;
        }
        // `sample_size` reaches a Qdrant scroll limit and a `dim × n` loop: a typo scrolls it all.
        const MAX_SAMPLE_SIZE: usize = 20_000;
        if self.ui.background.sample_size > MAX_SAMPLE_SIZE {
            tracing::warn!(
                configured = self.ui.background.sample_size,
                using = MAX_SAMPLE_SIZE,
                "ui.background.sample_size is far past what a decorative cloud can draw; capping it"
            );
            self.ui.background.sample_size = MAX_SAMPLE_SIZE;
        }
    }

    /// Rules a config can satisfy syntactically and still be wrong: `auto_supersede` at or below
    /// `review_min` names a lane no pair can be in.
    fn validate(&self) -> Result<(), ConfigError> {
        let c = &self.consolidate;
        if c.auto_supersede <= c.review_min {
            return Err(ConfigError::Invalid(format!(
                "consolidate.auto_supersede ({}) must be above consolidate.review_min ({}): \
                 it marks the pairs asked about first, and cannot sit below the score at \
                 which a pair is worth asking about",
                c.auto_supersede, c.review_min
            )));
        }
        self.infer
            .embed
            .templates()
            .validate()
            .map_err(ConfigError::Invalid)?;
        let tz = self.time.default_tz.trim();
        if !tz.is_empty() && tz.parse::<chrono_tz::Tz>().is_err() {
            return Err(ConfigError::Invalid(format!(
                "time.default_tz {tz:?} is not an IANA zone name (e.g. \"Europe/Berlin\"): \
                 left standing, every capture from a door that sends no zone would be \
                 silently read and stamped in UTC"
            )));
        }
        self.server.fingerprint().map_err(ConfigError::Invalid)?;
        Ok(())
    }

    /// The sweep that destroys captured text runs only where the file names `[reap]`: an upgraded
    /// base without the block never chose it. The default inside the block stays on.
    fn disable_undeclared_reap(&mut self, raw: &config::Config) {
        if !self.reap.enabled || raw.get::<config::Value>("reap").is_ok() {
            return;
        }
        self.reap.enabled = false;
        tracing::info!(
            min_age_days = self.reap.min_age_days,
            interval_mins = self.reap.interval_mins,
            "no [reap] section: the reap sweep stays off. It is the one stage that \
             destroys captured text, so it runs only where a file asks for it. Write \
             `[reap] enabled = true`, or the whole block from config.example.toml, to \
             turn it on."
        );
    }

    fn warn_on_defaulted_store(&self, raw: &config::Config) {
        if raw.get::<config::Value>("store").is_ok() {
            return;
        }
        tracing::info!(
            control_path = %self.store.control_path,
            dir = %self.store.dir,
            "no [store] section: using the default paths, resolved against the working \
             directory. If this base has data elsewhere, the section is missing or misspelt."
        );
    }

    fn warn_on_inert_settings(&self) {
        if !self.learn.enabled {
            tracing::warn!(
                "learn.enabled = false: activation never moves, so a large corpus's windows \
                 are never promoted — only captures small enough to synthesize at capture \
                 are ever rewritten."
            );
        }
        for key in self.inert_priming_keys() {
            tracing::warn!(
                key,
                "{key} is on but associate.prime_lift is 0, so it does nothing: both lifts \
                 share the one budget prime_lift bounds, and priming returns the list \
                 untouched at zero. Raise prime_lift, or turn this off to say what you mean."
            );
        }
    }

    /// Keys that are on but inert because `prime_lift` is zero; only under `full`, since `off` and
    /// `learning` zero it themselves.
    fn inert_priming_keys(&self) -> Vec<&'static str> {
        if self.learn.mode != LearnMode::Full || self.associate.prime_lift > 0 {
            return Vec::new();
        }
        let mut keys = Vec::new();
        if self.sitting.prime {
            keys.push("sitting.prime");
        }
        if self.time.lift {
            keys.push("time.lift");
        }
        keys
    }

    /// Say at startup which ceiling name was guessed: an endpoint ignoring one errs silently.
    fn warn_on_inferred_ceiling_param(&self) {
        for (role, effort) in self.inferred_ceiling_params() {
            tracing::warn!(
                role,
                reasoning_effort = effort,
                guessing = CeilingParam::inferred_from(Some(effort)).as_str(),
                "{role}.ceiling_param is unset, so the output ceiling's name is inferred from \
                 reasoning_effort. If this endpoint takes the other name and ignores unknown \
                 fields — llama.cpp and older vLLM builds do — replies run with no ceiling at \
                 all and nothing reports it. Set {role}.ceiling_param to say."
            );
        }
    }

    /// The roles whose ceiling name is a guess, each with the effort it is guessed from.
    fn inferred_ceiling_params(&self) -> Vec<(&'static str, &str)> {
        let s = self.infer.synthesize.as_ref();
        let mut roles: Vec<(&str, Option<&str>, Option<CeilingParam>)> = Vec::new();
        if let Some(s) = s {
            roles.push((
                "infer.synthesize",
                s.reasoning_effort.as_deref(),
                s.ceiling_param,
            ));
        }
        if let Some(a) = &self.infer.ask {
            roles.push(("infer.ask", a.reasoning_effort.as_deref(), a.ceiling_param));
        }
        // Vision may have inherited from synthesize, and its output is stored as a corpus.
        if let Some(v) = &self.infer.vision {
            roles.push((
                "infer.vision",
                v.inherited_reasoning_effort(s),
                v.ceiling_param(s),
            ));
        }
        // The planning call guesses off its own tier; without one, the ask line speaks for it.
        if let Some(f) = self
            .infer
            .ask
            .as_ref()
            .and_then(|a| a.plan_endpoint.as_ref())
        {
            roles.push((
                "infer.ask.plan_tier",
                f.reasoning_effort.as_deref(),
                f.ceiling_param,
            ));
        }
        roles
            .into_iter()
            .filter_map(|(role, effort, configured)| match (effort, configured) {
                (Some(effort), None) => Some((role, effort)),
                _ => None,
            })
            .collect()
    }

    /// Warn when `plan` is on with no `plan_tier`: every ask then pays a deep-model call first.
    fn warn_on_unplaced_plan_cost(&self) {
        let Some(a) = self.infer.ask.as_ref() else {
            return;
        };
        if a.plan && a.plan_endpoint.is_none() {
            tracing::warn!(
                model = %a.model,
                "infer.ask.plan is on with no infer.ask.plan_tier: the planning call \
                 runs on the ask endpoint, one extra completion per question. Name a \
                 cheaper tier there, or set plan = false"
            );
        }
    }

    /// A secret in the config file gets committed: warn loudly rather than refuse a working config.
    fn warn_on_file_secrets(&self, path: Option<&Path>) {
        let Some(p) = path else { return };
        let Ok(body) = std::fs::read_to_string(p) else {
            return;
        };
        for key in ["client_secret", "api_key", "password_hash"] {
            if body.contains(key) {
                tracing::warn!(
                    key,
                    file = %p.display(),
                    "secret found in config file; prefer the ENGRAM__ environment variable"
                );
            }
        }
    }

    pub fn redacted(&self) -> String {
        let mut c = self.clone();
        const R: &str = "REDACTED";
        c.vector.api_key = c.vector.api_key.map(|_| R.into());
        if let Some(s) = c.infer.synthesize.as_mut() {
            s.api_key = s.api_key.as_ref().map(|_| R.into());
        }
        c.infer.embed.api_key = c.infer.embed.api_key.map(|_| R.into());
        if let Some(a) = c.infer.ask.as_mut() {
            a.api_key = a.api_key.as_ref().map(|_| R.into());
            if let Some(f) = a.plan_endpoint.as_mut() {
                f.api_key = f.api_key.as_ref().map(|_| R.into());
            }
        }
        if let Some(r) = c.infer.rerank.as_mut() {
            r.api_key = r.api_key.as_ref().map(|_| R.into());
        }
        if let Some(v) = c.infer.vision.as_mut() {
            v.api_key = v.api_key.as_ref().map(|_| R.into());
        }
        if let Some(t) = c.infer.transcribe.as_mut() {
            t.api_key = t.api_key.as_ref().map(|_| R.into());
        }
        if let Some(o) = c.auth.oidc.as_mut() {
            o.client_secret = o.client_secret.as_ref().map(|_| R.into());
        }
        if let Some(l) = c.auth.local.as_mut() {
            l.password_hash = R.into();
        }
        // The dial first, in the file's spelling: what the mode decided, at a glance.
        let mut head = format!("# learn.mode = \"{}\"\n", c.learn.mode.as_str());
        head.push_str(&format!(
            "# evolve.autonomous = \"{}\"\n",
            c.evolve.autonomous.as_str()
        ));
        if c.learn.resolved.is_empty() {
            head.push_str("# nothing was resolved from it: every key it stands for is set\n");
        } else {
            head.push_str("# resolved from it, because the file did not say:\n");
            for (key, value) in &c.learn.resolved {
                head.push_str(&format!("#   {key} = {value}\n"));
            }
        }
        format!("{head}\n{c:#?}")
    }
}

impl Config {
    /// A `Config` with every role configured and nothing reachable; not `cfg(test)` because
    /// integration tests compile against the crate.
    #[doc(hidden)]
    pub fn test_default() -> Config {
        Config {
            evolve: EvolveConfig::default(),
            server: ServerConfig {
                bind: "127.0.0.1:8080".into(),
                workers: 2,
                tls_fingerprint: None,
                public_url: None,
                trusted_proxies: default_trusted_proxies(),
                allow_plain_http: false,
            },
            store: StoreConfig::default(),
            vector: VectorConfig {
                url: "http://localhost:6333".into(),
                collection: "chunks".into(),
                api_key: None,
                recency_weight: 0.05,
                recency_half_life_days: 180,
                pinned_boost: 0.15,
                weak_below: 0.35,
                per_source_cap: 3,
                candidate_multiplier: 3,
            },
            infer: InferConfig {
                tokenizer: None,
                synthesize: Some(SynthesizeRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    max_output_tokens: 8192,
                    output_ratio: 1.4,
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    ceiling_param: None,
                    structured_output: true,
                    context_opening_tokens: 200,
                    context_overlap_tokens: 150,
                    context_neighbor_tokens: 1024,
                }),
                embed: EmbedRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "e".into(),
                    api_key: None,
                    dim: 1024,
                    max_input_tokens: 8192,
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    query_template: EmbedTemplates::default().query_template,
                    document_template: EmbedTemplates::default().document_template,
                    document_template_untitled: EmbedTemplates::default()
                        .document_template_untitled,
                    chunk_tokens: DEFAULT_CHUNK_TOKENS,
                },
                ask: Some(AskRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    max_output_tokens: 4096,
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    ceiling_param: None,
                    plan: false,
                    structured_output: true,
                    plan_endpoint: None,
                }),
                rerank: None,
                vision: None,
                transcribe: None,
            },
            auth: AuthConfig {
                mode: AuthMode::Local,
                oidc: None,
                local: Some(LocalConfig {
                    username: "dev".into(),
                    password_hash: "$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aaaa".into(),
                }),
            },
            consolidate: ConsolidateConfig::default(),
            learn: LearnConfig::default(),
            feedback: FeedbackConfig::default(),
            capture: CaptureConfig::default(),
            pacing: PacingConfig::default(),
            associate: AssociateConfig::default(),
            activation: ActivationConfig::default(),
            promote: PromoteConfig::default(),
            pursuit: PursuitConfig::default(),
            schedule: ScheduleConfig::default(),
            sitting: SittingConfig::default(),
            time: TimeConfig::default(),
            reap: ReapConfig::default(),
            recommend: RecommendConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_dupe_min_catches_a_recapture_and_not_a_related_document() {
        let c = ConsolidateConfig::default();
        assert!(
            c.near_dupe_min <= 0.78,
            "a re-capture through another door scored 0.789; {} is above it",
            c.near_dupe_min
        );
        assert!(
            c.near_dupe_min > 0.10,
            "the highest false candidate scored 0.036; {} is not clear of it",
            c.near_dupe_min
        );
    }

    #[test]
    fn autonomy_reads_the_old_bool_and_the_three_words() {
        #[derive(Deserialize)]
        struct Probe {
            autonomous: Autonomy,
        }
        let parse = |s: &str| {
            config::Config::builder()
                .add_source(config::File::from_str(s, config::FileFormat::Toml))
                .build()
                .unwrap()
                .try_deserialize::<Probe>()
        };
        let read = |s: &str| parse(s).unwrap().autonomous;
        assert_eq!(read("autonomous = false"), Autonomy::Off);
        assert_eq!(
            read("autonomous = true"),
            Autonomy::Full,
            "true meant the whole loop"
        );
        assert_eq!(read(r#"autonomous = "off""#), Autonomy::Off);
        assert_eq!(read(r#"autonomous = "ranking""#), Autonomy::Ranking);
        assert_eq!(read(r#"autonomous = "full""#), Autonomy::Full);
        assert!(parse(r#"autonomous = "sometimes""#).is_err());
        // The same words as strings, the only way an environment variable can spell them.
        assert_eq!(read(r#"autonomous = "false""#), Autonomy::Off);
        assert_eq!(read(r#"autonomous = "true""#), Autonomy::Full);
    }

    #[test]
    fn an_evolve_section_naming_one_key_leaves_the_other_four_at_their_defaults() {
        let parse = |s: &str| {
            config::Config::builder()
                .add_source(config::File::from_str(s, config::FileFormat::Toml))
                .build()
                .unwrap()
                .try_deserialize::<EvolveConfig>()
                .unwrap()
        };
        let one = parse(r#"autonomous = "off""#);
        assert_eq!(one.autonomous, Autonomy::Off);
        assert_eq!(
            one.give_up_window_secs,
            EvolveConfig::default().give_up_window_secs,
        );
        assert_eq!(
            one.max_actions_per_day,
            EvolveConfig::default().max_actions_per_day,
        );
        assert_eq!(one.idle_secs, EvolveConfig::default().idle_secs);
    }

    fn parse_with(body: &str) -> Config {
        config::Config::builder()
            .add_source(config::File::from_str(
                &format!("{MINIMAL}\n{body}\n"),
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize::<Config>()
            .unwrap()
    }

    #[test]
    fn autonomy_is_full_whatever_the_file_says() {
        for body in [
            r#"autonomous = "off""#,
            r#"autonomous = "ranking""#,
            "autonomous = false",
            "",
        ] {
            let mut cfg = parse_with(&format!("[evolve]\n{body}"));
            cfg.normalize();
            assert_eq!(cfg.evolve.autonomous, Autonomy::Full, "{body}");
        }
    }

    #[test]
    fn the_pace_is_per_day_and_the_week_key_is_ignored() {
        let mut cfg = parse_with("[evolve]\nmax_actions_per_week = 3");
        cfg.normalize();
        assert_eq!(cfg.evolve.max_actions_per_day, 20);
        let cfg = parse_with("[evolve]\nmax_actions_per_day = 0");
        assert_eq!(cfg.evolve.max_actions_per_day, 0);
    }

    #[test]
    fn the_ranking_stage_moves_ranking_and_touches_no_corpus() {
        assert!(!Autonomy::Off.moves_ranking());
        assert!(Autonomy::Ranking.moves_ranking());
        assert!(!Autonomy::Ranking.acts_on_corpus());
        assert!(Autonomy::Full.acts_on_corpus());
        assert_eq!(EvolveConfig::default().max_actions_per_day, 20);
        assert_eq!(EvolveConfig::default().autonomous, Autonomy::Full);
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_capture_defaults_are_the_documented_ones() {
        let c = CaptureConfig::default();
        assert_eq!(c.fetch_timeout_secs, 30);
        assert_eq!(c.fetch_max_bytes, 8 * 1024 * 1024);
        assert_eq!(c.min_extracted_chars, 200);
    }

    #[test]
    fn the_background_ships_on() {
        let b = UiConfig::default().background;
        assert!(b.enabled);
        assert_eq!(b.sample_size, 2000);
    }

    #[test]
    fn the_example_config_carries_the_background_block() {
        // Read as text first: `#[serde(default)]` would pass a compare with the block deleted.
        let raw = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            raw.contains("\n[ui.background]\n"),
            "the block is documented"
        );
    }

    #[test]
    fn the_recommender_ships_on_with_its_weights_named() {
        let r = RecommendConfig::default();
        assert!(r.enabled);
        assert_eq!(r.weights.of("weekday"), 1.0);
        // Isolation is a database and collection per user, never a direction in a vector.
        assert_eq!(r.weights.of("scope"), 0.0);
        assert_eq!(r.weights.of("month_cycle"), 0.0, "off by default");
        assert_eq!(r.weights.of("phase_of_the_moon"), 0.0);
        assert!(r.strong_at > r.weak_at + 0.2);
        assert_eq!(r.self_weight, 0.0, "the offer does not teach itself");
    }

    #[test]
    fn the_example_config_carries_the_recommend_block() {
        // Read as text first: `#[serde(default)]` would pass a compare with the block deleted.
        let path = std::path::Path::new("config.example.toml");
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("\n[recommend]\n"), "the block is documented");
        assert!(
            raw.contains("\n[recommend.weights]\n"),
            "and so is every weight the offer rests on"
        );
        for block in [
            "time_of_day",
            "weekday",
            "weekend",
            "device",
            "viewport",
            "locale",
            "network",
            "power",
            "environment",
            "month_cycle",
        ] {
            assert!(raw.contains(&format!("\n{block} = ")), "{block} is unnamed");
        }

        let cfg = Config::load(Some(path)).unwrap();
        assert!(cfg.recommend.enabled);
        assert!(raw.contains("\n[learn]\n"), "the example names no [learn]");
        assert!(cfg.learn.enabled);
        assert_eq!(cfg.recommend.max_clusters, 5);
        assert_eq!(cfg.recommend.weights.of("network"), 0.6);
    }

    #[test]
    fn the_example_config_carries_the_reap_block() {
        // Read as text first, key by key: this sweep destroys text, so documented is the point.
        let path = std::path::Path::new("config.example.toml");
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("\n[reap]\n"), "the block is documented");
        for key in [
            "enabled",
            "interval_mins",
            "min_age_days",
            "max_judged_per_run",
            "max_rescues_per_run",
        ] {
            assert!(raw.contains(&format!("\n{key} = ")), "{key} is unnamed");
        }
        let cfg = Config::load(Some(path)).unwrap();
        assert!(cfg.reap.enabled);
        assert_eq!(cfg.reap.min_age_days, 90);
        assert_eq!(cfg.reap.max_rescues_per_run, 3);
    }

    #[test]
    fn the_example_config_carries_the_capture_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.capture.min_extracted_chars, 200);
    }

    #[test]
    fn the_ceiling_parameter_is_named_in_config_the_way_it_is_named_on_the_wire() {
        for (typed, want) in [
            ("max_tokens", CeilingParam::MaxTokens),
            ("max_completion_tokens", CeilingParam::MaxCompletionTokens),
        ] {
            let p: CeilingParam = serde_json::from_str(&format!("\"{typed}\"")).unwrap();
            assert_eq!(p, want);
            assert_eq!(p.as_str(), typed);
            assert_eq!(p.flipped().flipped(), p);
            assert_ne!(p.flipped(), p);
        }
        // No shared substring, so a rejection naming one is unambiguous.
        assert!(
            !CeilingParam::MaxCompletionTokens
                .as_str()
                .contains(CeilingParam::MaxTokens.as_str())
        );

        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(
            cfg.infer.ask.as_ref().unwrap().ceiling_param.is_none()
                && cfg
                    .infer
                    .synthesize
                    .as_ref()
                    .unwrap()
                    .ceiling_param
                    .is_none(),
            "the example config pins a ceiling name it should be leaving to the guess"
        );
    }

    #[test]
    fn the_default_timeout_survives_a_slow_local_model() {
        // A segmentation window on a 9B model on one consumer GPU measured seven minutes.
        const {
            assert!(
                DEFAULT_TIMEOUT_SECS >= 600,
                "the default must outlast a local reasoning model's slowest window"
            )
        };
    }

    #[test]
    fn a_config_without_timeouts_still_gets_them() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().timeout_secs,
            DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(cfg.infer.embed.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().timeout_secs,
            DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().reasoning_effort,
            None
        );
    }

    #[test]
    fn a_key_that_no_longer_exists_stops_the_start_rather_than_being_ignored() {
        // Unknown keys are dropped: `[feedback] enabled = false` would silently re-enable.
        let dir = tempfile::tempdir().unwrap();
        for key in ["[feedback]\nenabled = false", "[pursuit]\nenabled = false"] {
            let p = write(&dir, &format!("{MINIMAL}\n{key}\n"));
            let err = Config::load(Some(&p)).unwrap_err().to_string();
            assert!(
                err.contains("no longer exist") && err.contains("[learn]"),
                "a removed key loaded without saying so: {err}"
            );
        }
        // `true` is refused as well: a reader would take it for the live switch.
        let p = write(&dir, &format!("{MINIMAL}\n[associate]\nenabled = true\n"));
        assert!(Config::load(Some(&p)).is_err());
        let p = write(&dir, &format!("{MINIMAL}\n[learn]\nenabled = false\n"));
        assert!(!Config::load(Some(&p)).unwrap().learn.enabled);
    }

    #[test]
    fn a_zero_candidate_pool_is_put_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 0\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.feedback.candidates,
            FeedbackConfig::default().candidates
        );
        assert!(cfg.learn.enabled, "the rest of the section was dropped");
    }

    #[test]
    fn an_oversized_candidate_pool_is_capped_at_the_widest_ordinary_search() {
        // The candidate pool is the width of every captured search, not only the stored depth.
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 2000\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.feedback.candidates,
            crate::core::search::MAX_LIMIT
                * crate::core::ranking::MULTIPLIERS[crate::core::ranking::MULTIPLIERS.len() - 1]
        );
    }

    #[test]
    fn an_oversized_background_sample_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[ui.background]\nsample_size = 2000000\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.ui.background.sample_size, 20_000);
    }

    #[test]
    fn a_deliberate_background_sample_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[ui.background]\nsample_size = 500\n"),
        );
        assert_eq!(
            Config::load(Some(&p)).unwrap().ui.background.sample_size,
            500
        );
    }

    #[test]
    fn a_deliberate_candidate_count_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 5\n"));
        assert_eq!(Config::load(Some(&p)).unwrap().feedback.candidates, 5);
    }

    #[test]
    fn a_lift_of_zero_makes_the_knobs_that_share_its_budget_inert() {
        let mut cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(
            cfg.inert_priming_keys().is_empty(),
            "the shipped file lifts, so nothing on it is inert"
        );

        cfg.associate.prime_lift = 0;
        assert_eq!(
            cfg.inert_priming_keys(),
            vec!["sitting.prime", "time.lift"],
            "both share the budget, and both are on in the shipped file"
        );

        cfg.sitting.prime = false;
        cfg.time.lift = false;
        assert!(cfg.inert_priming_keys().is_empty());
    }

    #[test]
    fn the_learn_modes_do_not_warn_about_the_lift_they_zeroed_themselves() {
        // `time.lift` ships on, so a warning here would fire on every such base.
        let mut cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        cfg.associate.prime_lift = 0;
        for mode in [LearnMode::Off, LearnMode::Learning] {
            cfg.learn.mode = mode;
            assert!(cfg.inert_priming_keys().is_empty(), "{mode:?}");
        }
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("config.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn a_config_that_never_mentions_reap_does_not_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(Some(&write(&dir, MINIMAL))).unwrap();
        assert!(!cfg.reap.enabled, "an undeclared sweep stays off");

        // Writing the block is the act that turns the sweep on.
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(Some(&write(&dir, &format!("{MINIMAL}\n[reap]\n")))).unwrap();
        assert!(cfg.reap.enabled);
        assert_eq!(cfg.reap.min_age_days, ReapConfig::default().min_age_days);

        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(Some(&write(
            &dir,
            &format!("{MINIMAL}\n[reap]\nenabled = false\n"),
        )))
        .unwrap();
        assert!(!cfg.reap.enabled);
    }

    const MINIMAL: &str = r#"
[server]
bind = "127.0.0.1:8080"

[store]
path = "engram.db"

[vector]
url = "http://localhost:6334"
collection = "chunks"

[infer.tiers.efficient]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 8192

[infer.synthesize]
tier = "efficient"
output_ratio = 1.4

[infer.embed]
base_url = "http://localhost:8000/v1"
model = "bge-m3"
dim = 1024
max_input_tokens = 8192

[infer.ask]
tier = "efficient"

[auth]
mode = "local"

[auth.local]
username = "dev"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"
"#;

    const FIVE_SECTIONS: &str = r#"
[server]
bind = "127.0.0.1:8080"

[vector]
url = "http://localhost:6334"
collection = "chunks"

[infer.tiers.efficient]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 8192

[infer.synthesize]
tier = "efficient"

[infer.embed]
base_url = "http://localhost:8000/v1"
model = "bge-m3"
dim = 1024
max_input_tokens = 8192

[auth]
mode = "local"

[auth.local]
username = "dev"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"

[learn]
mode = "off"
"#;

    #[test]
    fn five_sections_and_one_mode_start() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, FIVE_SECTIONS);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.learn.mode, LearnMode::Off);
        assert!(!cfg.learn.enabled);
        assert!(!cfg.consolidate.enabled);
        assert!(!cfg.sitting.prime);
        assert_eq!(cfg.associate.spread_max, 0);
        assert_eq!(cfg.associate.prime_lift, 0);
        assert!(cfg.promote.activation_above.is_infinite());
        assert!(cfg.pursuit.min_engagement.is_infinite());
        // Left alone: this key arms the retention sweep that ages recorded rows out.
        assert!(cfg.recommend.enabled);
    }

    #[test]
    fn off_leaves_the_key_that_ages_recorded_rows_out_alone() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[learn]\nmode = \"off\"\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        // `periodic_units` arms retention on `retain_days > 0 || learn || recommend` enabled.
        assert!(!cfg.learn.enabled);
        assert_eq!(cfg.feedback.retain_days, 0);
        assert!(cfg.recommend.enabled);
        assert!(
            !cfg.learn
                .resolved
                .iter()
                .any(|(k, _)| *k == "recommend.enabled")
        );
    }

    #[test]
    fn learning_records_everything_and_reads_none_of_it() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[learn]\nmode = \"learning\"\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.learn.enabled);
        assert_eq!(cfg.associate.spread_max, 0);
        assert_eq!(cfg.associate.prime_lift, 0);
        assert!(!cfg.sitting.prime);
        assert!(!cfg.recommend.enabled);
        assert!(cfg.promote.activation_above.is_infinite());
        assert!(cfg.pursuit.min_engagement.is_infinite());
        assert!(cfg.consolidate.enabled);
        // Ranking is held by the mode at the pass (`Core::moves_ranking`), not by `autonomous`.
        assert_eq!(cfg.evolve.autonomous, Autonomy::Full);
        assert!(
            !cfg.learn
                .resolved
                .iter()
                .any(|(k, _)| *k == "evolve.autonomous"),
            "{:?}",
            cfg.learn.resolved
        );
    }

    #[test]
    fn a_learning_base_told_full_by_name_is_full() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[learn]\nmode = \"learning\"\n\
                 [evolve]\nautonomous = \"full\"\n"
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.evolve.autonomous, Autonomy::Full);
        assert!(
            !cfg.learn
                .resolved
                .iter()
                .any(|(k, _)| *k == "evolve.autonomous")
        );
    }

    #[test]
    fn full_is_todays_defaults_and_resolves_nothing() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let with = write(&dir, &format!("{MINIMAL}\n[learn]\nmode = \"full\"\n"));
        let cfg = Config::load(Some(&with)).unwrap();
        assert!(cfg.learn.resolved.is_empty(), "{:?}", cfg.learn.resolved);
        let plain = Config::load(Some(&write(&tempfile::tempdir().unwrap(), MINIMAL))).unwrap();
        assert_eq!(cfg.learn.mode, plain.learn.mode);
        assert_eq!(cfg.learn.enabled, plain.learn.enabled);
        assert_eq!(cfg.recommend.enabled, plain.recommend.enabled);
        assert_eq!(cfg.associate.spread_max, plain.associate.spread_max);
        assert_eq!(cfg.promote.activation_above, plain.promote.activation_above);
    }

    #[test]
    fn a_key_in_the_file_beats_the_mode() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[learn]\nmode = \"off\"\nenabled = true\n\
                 [recommend]\nenabled = true\n[associate]\nspread_max = 2\n"
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.learn.enabled);
        assert!(cfg.recommend.enabled);
        assert_eq!(cfg.associate.spread_max, 2);
        assert!(!cfg.consolidate.enabled);
        assert!(cfg.promote.activation_above.is_infinite());
        let named: Vec<&str> = cfg.learn.resolved.iter().map(|(k, _)| *k).collect();
        assert!(!named.contains(&"learn.enabled"), "{named:?}");
        assert!(!named.contains(&"associate.spread_max"), "{named:?}");
        assert!(named.contains(&"consolidate.enabled"), "{named:?}");
    }

    #[test]
    fn print_config_names_what_the_mode_decided() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, FIVE_SECTIONS);
        let dump = Config::load(Some(&p)).unwrap().redacted();
        assert!(dump.contains("learn.mode = \"off\""), "{dump}");
        assert!(dump.contains("consolidate.enabled = false"), "{dump}");
        assert!(dump.contains("promote.activation_above = inf"), "{dump}");
    }

    #[test]
    fn loads_minimal_config() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.infer.embed.dim, 1024);
        assert_eq!(cfg.vector.collection, "chunks");
        assert!(
            cfg.infer.rerank.is_none(),
            "rerank must default to disabled"
        );
    }

    #[test]
    fn transcription_is_off_until_an_endpoint_is_named() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        assert!(
            Config::load(Some(&p)).unwrap().infer.transcribe.is_none(),
            "speech-to-text must default to unconfigured"
        );

        let stt = "\n[infer.transcribe]\nbase_url = \"http://localhost:8090/v1\"\nmodel = \"whisper-1\"\n";
        let p = write(&dir, &format!("{MINIMAL}{stt}"));
        let role = Config::load(Some(&p)).unwrap().infer.transcribe.unwrap();
        assert_eq!(role.base_url, "http://localhost:8090/v1");
        assert_eq!(role.model, "whisper-1");
        assert_eq!(role.timeout_secs, 60, "a person is holding the button");
        assert!(
            role.language.is_none(),
            "detection unless a language is named"
        );

        let p = write(&dir, &format!("{MINIMAL}{stt}language = \"de\"\n"));
        let role = Config::load(Some(&p)).unwrap().infer.transcribe.unwrap();
        assert_eq!(role.language.as_deref(), Some("de"));
    }

    #[test]
    fn the_transcription_key_is_redacted_in_the_dump() {
        // `--print-config` is a thing operators paste into issues.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let stt = "\n[infer.transcribe]\nbase_url = \"http://localhost:8090/v1\"\nmodel = \"whisper-1\"\napi_key = \"sk-not-in-the-dump\"\n";
        let p = write(&dir, &format!("{MINIMAL}{stt}"));
        let dump = Config::load(Some(&p)).unwrap().redacted();
        assert!(!dump.contains("sk-not-in-the-dump"), "{dump}");
        assert!(dump.contains("REDACTED"), "{dump}");
    }

    #[test]
    fn rerank_applies_everywhere_unless_narrowed() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let rerank = "\n[infer.rerank]\nbase_url = \"http://localhost:8081\"\nmodel = \"bge-reranker-v2-m3\"\nstyle = \"tei\"\n";
        let p = write(&dir, &format!("{MINIMAL}{rerank}"));
        let role = Config::load(Some(&p)).unwrap().infer.rerank.unwrap();
        assert!(role.applies_to(RerankApply::Ask));
        assert!(role.applies_to(RerankApply::Search));

        let p = write(&dir, &format!("{MINIMAL}{rerank}apply = [\"ask\"]\n"));
        let role = Config::load(Some(&p)).unwrap().infer.rerank.unwrap();
        assert!(role.applies_to(RerankApply::Ask));
        assert!(
            !role.applies_to(RerankApply::Search),
            "apply = [\"ask\"] must switch the reranker off for search"
        );
    }

    #[test]
    fn a_public_url_is_required_off_loopback() {
        let mut cfg = Config::test_default();
        cfg.server.bind = "0.0.0.0:8080".into();
        let err = cfg.deployment().unwrap_err().to_string();
        assert!(err.contains("server.public_url"), "{err}");
        cfg.server.public_url = Some("https://engram.example".into());
        assert!(cfg.deployment().is_ok());
        let loopback = Config::test_default();
        assert!(loopback.deployment().unwrap().public_url.is_none());
    }

    #[test]
    fn plain_http_off_loopback_needs_allow_plain_http() {
        let mut cfg = Config::test_default();
        cfg.server.public_url = Some("http://engram.lan".into());
        let err = cfg.deployment().unwrap_err().to_string();
        assert!(err.contains("allow_plain_http"), "{err}");
        cfg.server.allow_plain_http = true;
        let dep = cfg.deployment().unwrap();
        assert!(!dep.secure_cookies());
        cfg.server.allow_plain_http = false;
        cfg.server.public_url = Some("http://localhost:8080".into());
        assert!(cfg.deployment().is_ok(), "loopback http needs no opt-in");
    }

    #[test]
    fn a_config_with_redirect_url_is_refused_and_names_public_url() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[auth.oidc]\nissuer_url = \"https://idp.example\"\nclient_id = \"engram\"\nredirect_url = \"https://engram.example/auth/callback\"\n"
            ),
        );
        let err = Config::load(Some(&p)).unwrap_err().to_string();
        assert!(
            err.contains("auth.oidc.redirect_url is gone; set server.public_url."),
            "{err}"
        );
    }

    #[test]
    fn a_secret_can_come_from_a_file_env_or_a_file_url() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("key");
        std::fs::write(&secret, "s3cret\n").unwrap();
        let p = write(&dir, MINIMAL);
        temp_env::with_var(
            "ENGRAM__VECTOR__API_KEY_FILE",
            Some(secret.to_str().unwrap()),
            || {
                let cfg = Config::load(Some(&p)).unwrap();
                assert_eq!(cfg.vector.api_key.as_deref(), Some("s3cret"));
            },
        );
        let p = write(
            &dir,
            &MINIMAL.replace(
                "collection = \"chunks\"",
                &format!(
                    "collection = \"chunks\"\napi_key = \"file://{}\"",
                    secret.display()
                ),
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.vector.api_key.as_deref(), Some("s3cret"));
    }

    #[test]
    fn the_oidc_callback_is_derived_from_public_url() {
        let mut cfg = Config::test_default();
        cfg.server.public_url = Some("https://engram.example/".into());
        let dep = cfg.deployment().unwrap();
        assert_eq!(
            crate::auth::oidc::callback_url(&dep.base()),
            "https://engram.example/auth/callback"
        );
    }

    #[test]
    fn env_overrides_file() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        temp_env::with_var("ENGRAM__INFER__EMBED__DIM", Some("768"), || {
            let cfg = Config::load(Some(&p)).unwrap();
            assert_eq!(cfg.infer.embed.dim, 768);
        });
    }

    #[test]
    fn an_unknown_key_does_not_stop_a_config_that_otherwise_works() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[consolidate]\nnot_a_setting = 20\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.consolidate.max_dedupe_per_tick,
            ConsolidateConfig::default().max_dedupe_per_tick
        );
    }

    #[test]
    fn thresholds_that_leave_no_review_band_are_refused() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[consolidate]\nreview_min = 0.88\nauto_supersede = 0.85\n"),
        );
        assert!(matches!(
            Config::load(Some(&p)),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn a_fingerprint_that_is_not_32_bytes_is_refused_at_startup() {
        // A typo here is a phone that refuses every connection with nothing saying why.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let body = MINIMAL.replace(
            "bind = \"127.0.0.1:8080\"",
            "bind = \"127.0.0.1:8080\"\ntls_fingerprint = \"not-a-digest\"",
        );
        let p = write(&dir, &body);
        match Config::load(Some(&p)) {
            Err(ConfigError::Invalid(msg)) => {
                assert!(msg.contains("server.tls_fingerprint"), "{msg}")
            }
            other => panic!("expected an Invalid error, got {other:?}"),
        }
    }

    #[test]
    fn a_fingerprint_is_normalised_to_unpadded_base64url() {
        // `openssl` prints padded standard base64; the QR wants unpadded base64url.
        use base64::Engine;
        let padded = base64::engine::general_purpose::STANDARD.encode([0xABu8; 32]);
        let cfg = ServerConfig {
            bind: "127.0.0.1:8080".into(),
            workers: 1,
            tls_fingerprint: Some(padded),
            public_url: None,
            trusted_proxies: default_trusted_proxies(),
            allow_plain_http: false,
        };
        let got = cfg.fingerprint().unwrap().unwrap();
        assert_eq!(got.len(), 43);
        assert!(!got.contains('='));
        assert!(!got.contains('+') && !got.contains('/'));

        let unset = ServerConfig {
            bind: "127.0.0.1:8080".into(),
            workers: 1,
            tls_fingerprint: None,
            public_url: None,
            trusted_proxies: default_trusted_proxies(),
            allow_plain_http: false,
        };
        assert_eq!(unset.fingerprint().unwrap(), None);
    }

    #[test]
    fn a_misspelt_default_zone_is_refused_at_startup() {
        // A misspelt zone parses as none and would fall back to UTC silently.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[time]\ndefault_tz = \"Europe/Berlim\"\n"),
        );
        assert!(matches!(
            Config::load(Some(&p)),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn redacted_hides_secrets() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        let dump = cfg.redacted();
        assert!(!dump.contains("$argon2id$"), "password hash leaked: {dump}");
        assert!(dump.contains("REDACTED"));
    }

    #[test]
    fn vision_is_off_unless_configured() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(
            cfg.infer.vision.is_none(),
            "vision must default to disabled"
        );
        assert_eq!(cfg.capture.image_max_bytes, 25 * 1024 * 1024);
        assert_eq!(cfg.capture.pdf_max_bytes, 50 * 1024 * 1024);
        assert_eq!(cfg.capture.image_preview_edge, 2048);
    }

    #[test]
    fn a_vision_role_without_its_own_endpoint_inherits_synthesize() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(v.model, "qwen-vl");
        assert_eq!(v.timeout_secs, 120);
        assert_eq!(v.max_output_tokens, 4096);
        let (url, key) = v.resolve(cfg.infer.synthesize.as_ref());
        assert_eq!(url, cfg.infer.synthesize.as_ref().unwrap().base_url);
        assert_eq!(key, cfg.infer.synthesize.as_ref().unwrap().api_key);
    }

    #[test]
    fn the_ceiling_name_is_inherited_only_by_a_vision_role_sharing_the_endpoint() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let mut cfg = Config::load(Some(&p)).unwrap();
        cfg.infer.synthesize.as_mut().unwrap().ceiling_param = Some(CeilingParam::MaxTokens);
        let synth = cfg.infer.synthesize.clone().unwrap();
        let v = cfg.infer.vision.as_mut().expect("configured");

        assert_eq!(v.ceiling_param(Some(&synth)), Some(CeilingParam::MaxTokens));

        v.base_url = Some("http://vision:9000/v1".into());
        assert_eq!(v.ceiling_param(Some(&synth)), None);

        v.ceiling_param = Some(CeilingParam::MaxCompletionTokens);
        assert_eq!(
            v.ceiling_param(Some(&synth)),
            Some(CeilingParam::MaxCompletionTokens)
        );
    }

    #[test]
    fn the_effort_the_name_is_guessed_from_travels_with_the_borrowed_endpoint() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let mut cfg = Config::load(Some(&p)).unwrap();
        cfg.infer.synthesize.as_mut().unwrap().reasoning_effort = Some("high".into());
        let synth = cfg.infer.synthesize.clone().unwrap();
        let v = cfg.infer.vision.as_mut().expect("configured");

        assert_eq!(
            v.ceiling_param(Some(&synth)),
            None,
            "nothing explicit to inherit"
        );
        assert_eq!(v.inherited_reasoning_effort(Some(&synth)), Some("high"));

        v.base_url = Some("http://vision:9000/v1".into());
        assert_eq!(v.inherited_reasoning_effort(Some(&synth)), None);
    }

    #[test]
    fn the_ambiguous_effort_is_guessed_the_way_that_corrects_itself() {
        assert_eq!(CeilingParam::inferred_from(None), CeilingParam::MaxTokens);
        for ambiguous in ["none", "None", " none "] {
            assert_eq!(
                CeilingParam::inferred_from(Some(ambiguous)),
                CeilingParam::MaxTokens,
                "{ambiguous} was read as a hosted reasoning endpoint"
            );
        }
        for hosted in ["minimal", "low", "medium", "high"] {
            assert_eq!(
                CeilingParam::inferred_from(Some(hosted)),
                CeilingParam::MaxCompletionTokens,
                "{hosted}"
            );
        }
    }

    #[test]
    fn a_dedicated_vision_endpoint_wins_over_synthesize() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\nbase_url = \"http://vision:9000/v1\"\napi_key = \"vk\"\n"
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        let (url, key) = cfg
            .infer
            .vision
            .as_ref()
            .unwrap()
            .resolve(cfg.infer.synthesize.as_ref());
        assert_eq!(url, "http://vision:9000/v1");
        assert_eq!(key.as_deref(), Some("vk"));
        assert!(!cfg.redacted().contains("\"vk\""), "vision key leaked");
    }

    #[test]
    fn the_example_config_documents_the_vision_role() {
        let text = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            text.contains("[infer.vision]"),
            "example config must show the vision block"
        );
        assert!(text.contains("image_max_bytes"));
        assert!(text.contains("pdf_max_bytes"));
    }

    #[test]
    fn the_association_defaults_are_the_documented_ones() {
        let a = AssociateConfig::default();
        assert_eq!(a.interval_mins, 30);
        assert_eq!(a.half_life_days, 30.0);
        assert_eq!((a.show_min, a.judge_min, a.prune_below), (2.0, 4.0, 0.5));
        assert_eq!((a.spread_from, a.spread_max), (3, 3));
        assert_eq!((a.prime_margin, a.prime_lift), (0.5, 1));
        let v = ActivationConfig::default();
        assert_eq!(v.half_life_days, 14.0);
        assert_eq!((v.retrieved, v.opened, v.confirmed), (0.0, 1.0, 3.0));
    }

    #[test]
    fn a_config_with_no_association_block_still_gets_one() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.learn.enabled);
        // Promotion reads activation, which moves only while searches are recorded.
        assert!(cfg.learn.enabled);
    }

    #[test]
    fn the_example_config_carries_the_association_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.associate.spread_max, 3);
    }
    const AUTH_TAIL: &str = r#"
        [auth]
        mode = "local"
        [auth.local]
        username = "dev"
        password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"
    "#;

    fn load_infer(body: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        Config::load(Some(&write(&dir, &format!("{body}{AUTH_TAIL}"))))
    }

    #[test]
    fn a_role_resolves_its_endpoint_from_its_tier() {
        let _guard = env_guard();
        let cfg = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficient"
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
        "#,
        )
        .expect("tiered config parses");

        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().output_ratio, 8.0);

        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().base_url,
            "http://localhost:8000/v1"
        );
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().model, "qwen");
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().context_tokens, 32768);
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().max_output_tokens,
            16384
        );
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().base_url,
            "http://localhost:8000/v1"
        );
        assert_eq!(cfg.infer.ask.as_ref().unwrap().model, "qwen");
    }

    #[test]
    fn a_role_field_overrides_the_tier_it_points_at() {
        let _guard = env_guard();
        let cfg = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.deep]
        base_url = "http://localhost:8000/v1"
        model = "big"
        context_tokens = 131072
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "deep"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "deep"
        max_output_tokens = 4096
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().max_output_tokens,
            4096,
            "the role's value wins"
        );
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().context_tokens,
            131072,
            "unset fields come from the tier"
        );
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().max_output_tokens,
            16384
        );
    }

    #[test]
    fn a_role_pointing_at_a_tier_that_does_not_exist_is_refused() {
        let _guard = env_guard();
        let err = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficent"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("efficent"),
            "the error must name the typo: {err}"
        );
        assert!(err.contains("efficient"), "and what was available: {err}");
    }

    #[test]
    fn the_example_config_reaches_its_endpoints_through_tiers() {
        let text = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            text.contains("[infer.tiers."),
            "the example must show a tier"
        );
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().context_tokens, 32768);
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().max_output_tokens,
            16384
        );
        assert_eq!(cfg.infer.ask.as_ref().unwrap().context_tokens, 32768);
        assert_eq!(cfg.infer.ask.as_ref().unwrap().max_output_tokens, 4096);
    }
    const TIERED: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        api_key = "tier-key"
        context_tokens = 32768
        max_output_tokens = 16384
        timeout_secs = 600
        ceiling_param = "max_completion_tokens"
        [infer.synthesize]
        tier = "efficient"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
    "#;

    #[test]
    fn a_vision_role_takes_its_endpoint_from_a_tier_but_not_its_output_ceiling() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\ntier = \"efficient\"\n"
        ))
        .unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(v.base_url.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(v.api_key.as_deref(), Some("tier-key"));
        assert_eq!(
            v.timeout_secs, 600,
            "a timeout is a property of the server, so it comes from the tier"
        );
        assert_eq!(
            v.ceiling_param,
            Some(CeilingParam::MaxCompletionTokens),
            "so is the name the ceiling is sent under"
        );
        assert_eq!(
            v.max_output_tokens,
            default_vision_max_output_tokens(),
            "the tier's 16384 is sized for the segmenter; a description is \
             stored as a corpus and keeps its own bound"
        );
        assert!(!cfg.redacted().contains("tier-key"), "tier key leaked");
    }

    #[test]
    fn a_vision_role_borrowing_the_synthesize_endpoint_is_the_common_case() {
        let _guard = env_guard();
        let cfg = load_infer(&format!("{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\n")).unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert!(
            v.base_url.is_none(),
            "`None` is what `resolve` reads as: use the synthesize endpoint"
        );
        assert_eq!(
            v.timeout_secs,
            default_vision_timeout_secs(),
            "with no tier to take one from, the role keeps its own two minutes"
        );
        assert_eq!(
            v.resolve(cfg.infer.synthesize.as_ref()).0,
            cfg.infer.synthesize.as_ref().unwrap().base_url
        );
    }

    #[test]
    fn a_vision_role_may_carry_its_own_endpoint() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\nbase_url = \"http://vision:9000/v1\"\n"
        ))
        .unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(
            v.resolve(cfg.infer.synthesize.as_ref()).0,
            "http://vision:9000/v1"
        );
    }

    #[test]
    fn a_plan_tier_resolves_to_a_complete_endpoint() {
        let _guard = env_guard();
        let cfg = load_infer(&format!("{TIERED}\nplan_tier = \"efficient\"\n")).unwrap();
        let f = cfg
            .infer
            .ask
            .as_ref()
            .unwrap()
            .plan_endpoint
            .as_ref()
            .expect("the named tier resolved");
        assert_eq!(f.base_url, "http://localhost:8000/v1");
        assert_eq!(f.model, "qwen");
        assert_eq!(f.max_output_tokens, 16384);
        assert_eq!(f.api_key.as_deref(), Some("tier-key"));
        assert!(
            !cfg.redacted().contains("tier-key"),
            "the planning endpoint's key leaked"
        );
    }

    #[test]
    fn the_plan_fallback_carries_the_ask_tiers_structured_output_flag() {
        let _guard = env_guard();
        let toml = TIERED.replace(
            "ceiling_param = \"max_completion_tokens\"",
            "ceiling_param = \"max_completion_tokens\"\nstructured_output = false",
        );
        let cfg = load_infer(&toml).unwrap();
        assert!(cfg.infer.ask.as_ref().unwrap().plan_endpoint.is_none());
        assert!(
            !cfg.infer.ask.as_ref().unwrap().plan_on().structured_output,
            "the tier said no response format; the fallback endpoint said yes"
        );
        let cfg = load_infer(TIERED).unwrap();
        assert!(cfg.infer.ask.as_ref().unwrap().plan_on().structured_output);
    }

    #[test]
    fn planning_ships_on_and_the_old_key_still_speaks() {
        let _guard = env_guard();
        let on = |toml: &str| load_infer(toml).unwrap().infer.ask.unwrap().plan;
        assert!(on(TIERED), "the fan-out has to be the default");
        assert!(!on(&format!("{TIERED}\nplan = false\n")));
        assert!(
            !on(&format!("{TIERED}\nfollow_up = false\n")),
            "the old key stopped being read"
        );
    }

    #[test]
    fn a_plan_tier_guessing_its_ceiling_name_is_warned_about() {
        let _guard = env_guard();
        let toml = format!(
            "{TIERED}\nplan_tier = \"quick\"\n\
             [infer.tiers.quick]\nbase_url = \"http://localhost:8001/v1\"\nmodel = \"small\"\n\
             context_tokens = 8192\nmax_output_tokens = 512\nreasoning_effort = \"none\"\n"
        );
        let cfg = load_infer(&toml).unwrap();
        let guessed = cfg.inferred_ceiling_params();
        assert!(
            guessed.contains(&("infer.ask.plan_tier", "none")),
            "the planning endpoint's guess went unreported: {guessed:?}"
        );
        assert!(
            !guessed.iter().any(|(r, _)| *r == "infer.ask"),
            "{guessed:?}"
        );
    }

    #[test]
    fn a_plan_tier_that_does_not_exist_is_refused_like_any_other() {
        let _guard = env_guard();
        let err = load_infer(&format!("{TIERED}\nplan_tier = \"efficent\"\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("plan_tier"), "name the key: {err}");
        assert!(
            err.contains("efficent"),
            "the error must name the typo: {err}"
        );
        assert!(err.contains("efficient"), "and what was available: {err}");
    }

    #[test]
    fn query_and_document_render_differently_for_the_same_text() {
        let t = EmbedTemplates::default();
        let q = t.render_query("how do I recover deleted entries");
        let d = t.render_document(None, "how do I recover deleted entries");
        assert_ne!(q, d);
        assert_eq!(
            q,
            "task: search result | query: how do I recover deleted entries"
        );
        assert_eq!(d, "title: none | text: how do I recover deleted entries");
    }

    #[test]
    fn a_titled_and_an_untitled_document_take_different_templates() {
        let t = EmbedTemplates::default();
        assert_eq!(
            t.render_document(Some("Recovering deleted entries"), "run fsck first"),
            "title: Recovering deleted entries | text: run fsck first"
        );
        assert_eq!(
            t.render_document(None, "run fsck first"),
            "title: none | text: run fsck first"
        );
    }

    #[test]
    fn the_legacy_templates_reproduce_the_old_join() {
        let t = EmbedTemplates::legacy();
        assert_eq!(t.render_query("t0\nalpha"), "t0\nalpha");
        assert_eq!(t.render_document(Some("t0"), "alpha"), "t0\nalpha");
        assert_eq!(t.render_document(None, "alpha"), "alpha");
    }

    #[test]
    fn substitution_is_one_pass_so_a_value_cannot_be_substituted_again() {
        let t = EmbedTemplates::default();
        assert_eq!(
            t.render_document(Some("about {text}"), "body"),
            "title: about {text} | text: body"
        );
        assert_eq!(
            substitute("a {x} b {y} c {x}", &[("x", "1"), ("y", "2")]),
            "a 1 b 2 c 1"
        );
        assert_eq!(substitute("{nope} {x}", &[("x", "1")]), "{nope} 1");
    }

    #[test]
    fn a_template_without_its_placeholder_is_rejected() {
        assert!(EmbedTemplates::default().validate().is_ok());
        let t = EmbedTemplates {
            query_template: "task: search result | query: ".into(),
            ..EmbedTemplates::default()
        };
        assert!(t.validate().unwrap_err().contains("query_template"));
        let t = EmbedTemplates {
            document_template: "text: {text}".into(),
            ..EmbedTemplates::default()
        };
        assert!(t.validate().unwrap_err().contains("{title}"));
        let t = EmbedTemplates {
            document_template_untitled: "title: none | text: ".into(),
            ..EmbedTemplates::default()
        };
        assert!(
            t.validate()
                .unwrap_err()
                .contains("document_template_untitled")
        );
    }

    const EMBED_PREAMBLE: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficient"
        output_ratio = 8.0
        [infer.ask]
        tier = "efficient"
    "#;

    #[test]
    fn embed_templates_default_to_embeddinggemma_when_unset() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"embeddinggemma\"
            dim = 768
            max_input_tokens = 2048
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.templates(), EmbedTemplates::default());
    }

    #[test]
    fn embed_templates_are_read_from_config() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"bge-m3\"
            dim = 1024
            max_input_tokens = 1024
            query_template = \"{{text}}\"
            document_template = \"{{title}}\\n{{text}}\"
            document_template_untitled = \"{{text}}\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.templates(), EmbedTemplates::legacy());
    }

    #[test]
    fn a_template_missing_its_placeholder_fails_at_load() {
        let _guard = env_guard();
        let err = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"embeddinggemma\"
            dim = 768
            max_input_tokens = 2048
            query_template = \"task: search result | query: \"
            "
        ))
        .unwrap_err();
        assert!(err.to_string().contains("query_template"), "got: {err}");
    }

    const BARE_PREAMBLE: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "embeddinggemma"
        dim = 768
        max_input_tokens = 2048
    "#;

    const BARE_PREAMBLE_NO_EMBED: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
    "#;

    #[test]
    fn a_config_without_a_synthesizer_is_the_embed_only_floor() {
        let _guard = env_guard();
        let cfg = load_infer(BARE_PREAMBLE).unwrap();
        assert!(cfg.infer.synthesize.is_none());
        assert!(cfg.infer.ask.is_none());
        // The redacted dump, which every boot logs, survives the absence.
        assert!(cfg.redacted().contains("synthesize: None"));
    }

    #[test]
    fn vision_with_no_address_and_nothing_to_borrow_is_refused() {
        let _guard = env_guard();
        let err = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.vision]
            model = \"llava\"
            "
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing") || err.contains("borrow"), "{err}");
    }

    #[test]
    fn the_reshaped_keys_are_refused_by_name() {
        let _guard = env_guard();
        for key in ["synthesis = \"earned\"", "segment_tokens = 2048"] {
            let err = load_infer(&format!(
                "{BARE_PREAMBLE}
                [infer]
                {key}
                [infer.synthesize]
                tier = \"efficient\"
                "
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("2026-09 capture reshape"), "{key}: {err}");
        }
    }

    #[test]
    fn vision_borrows_the_synthesizer_or_stands_on_its_own_address() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            [infer.vision]
            model = \"llava\"
            "
        ))
        .unwrap();
        assert!(cfg.infer.vision.is_some());
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            [infer.vision]
            model = \"llava\"
            base_url = \"http://localhost:9000/v1\"
            "
        ))
        .unwrap();
        let v = cfg.infer.vision.as_ref().unwrap();
        assert_eq!(v.resolve(None).0, "http://localhost:9000/v1");
    }

    #[test]
    fn chunk_tokens_defaults_to_384_and_is_clamped_to_the_embedder() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.chunk_tokens, DEFAULT_CHUNK_TOKENS);
        assert_eq!(cfg.infer.embed.effective_chunk_tokens(), 384);
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE_NO_EMBED}
            [infer.synthesize]
            tier = \"efficient\"
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"small\"
            dim = 384
            max_input_tokens = 256
            chunk_tokens = 1000
            "
        ))
        .unwrap();
        // 256 * 0.8 = 204: what the embedder will take wins over what was asked.
        assert_eq!(cfg.infer.embed.effective_chunk_tokens(), 204);
    }

    #[test]
    fn promote_defaults_and_feedback_ships_on() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.promote.activation_above, 3.0);
        assert!(cfg.learn.enabled);
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            [promote]
            activation_above = 2.5
            [learn]
            enabled = false
            "
        ))
        .unwrap();
        assert_eq!(cfg.promote.activation_above, 2.5);
        assert!(!cfg.learn.enabled);
    }

    #[test]
    fn the_learning_layer_is_one_switch_that_nothing_can_contradict() {
        let _guard = env_guard();
        let roles = "[infer.synthesize]\ntier = \"efficient\"\noutput_ratio = 8.0\n[infer.ask]\ntier = \"efficient\"\n";
        let cfg = load_infer(&format!("{BARE_PREAMBLE}\n{roles}")).unwrap();
        assert!(cfg.learn.enabled);
        assert_eq!(cfg.pursuit.idle_secs, 900);
        assert_eq!(cfg.pursuit.min_sources, 2);
        assert_eq!(cfg.pursuit.min_engagement, 3.0);
        assert_eq!(cfg.associate.interval_mins, 30);
        assert_eq!(cfg.feedback.candidates, 20);

        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}\n{roles}[learn]\nenabled = false\n"
        ))
        .unwrap();
        assert!(!cfg.learn.enabled);
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}\n{roles}[learn]\nenabled = false\n[pursuit]\nidle_secs = 60\n"
        ))
        .unwrap();
        assert_eq!(cfg.pursuit.idle_secs, 60);
        assert!(!cfg.learn.enabled);
    }

    #[test]
    fn the_rerank_probe_shares_the_prefix_of_its_style_request_path() {
        // Cohere's `base_url` already ends in `/v1`, so the probe must not add one.
        assert_eq!(RerankStyle::Cohere.probe_path(), "models");
        assert_eq!(RerankStyle::Vllm.probe_path(), "v1/models");
        assert_eq!(RerankStyle::Tei.probe_path(), "info");
    }
}
