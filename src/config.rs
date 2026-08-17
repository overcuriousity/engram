use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub store: StoreConfig,
    pub vector: VectorConfig,
    pub infer: InferConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub consolidate: ConsolidateConfig,
    #[serde(default)]
    pub feedback: FeedbackConfig,
    #[serde(default)]
    pub pacing: PacingConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub associate: AssociateConfig,
    #[serde(default)]
    pub activation: ActivationConfig,
}

/// What the two supplied-from-outside capture paths are allowed to cost.
///
/// The fetch limits are deliberately separate from `MAX_BODY_BYTES`: that one
/// bounds what a client may send us, and says nothing about what we go and
/// retrieve on their behalf.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CaptureConfig {
    /// Ceiling on a server-side GET. Generous, but it is a network fetch and
    /// not a local model call, so it is not measured in minutes.
    pub fetch_timeout_secs: u64,
    /// Bytes read from a fetched URL before the transfer is abandoned.
    pub fetch_max_bytes: usize,
    /// Characters an extraction must yield to count as a capture. Below this,
    /// the page reduced to navigation and boilerplate: report it, store
    /// nothing. A corpus that silently holds a cookie banner instead of the
    /// document is the failure this whole path is shaped to prevent.
    pub min_extracted_chars: usize,
    /// Bytes an uploaded image may weigh. A phone photo is 3–8 MB; this is the
    /// per-route ceiling for the image door only, the global body limit stays.
    pub image_max_bytes: usize,
    /// Longest edge, in pixels, of the preview the vision model is shown and
    /// the UI displays. The original is stored untouched regardless.
    pub image_preview_edge: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fetch_timeout_secs: 30,
            fetch_max_bytes: 8 * 1024 * 1024,
            min_extracted_chars: 200,
            image_max_bytes: 25 * 1024 * 1024,
            image_preview_edge: 2048,
        }
    }
}

/// Pacing for every inference call, not just synthesis.
///
/// The roles share one GPU, so a per-role gap could not bound total load: three
/// roles each honouring their own cooldown still interleave into unbroken work.
/// One gap in front of all of them is the only version of this setting that
/// means what it says.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct PacingConfig {
    /// Minimum seconds between the end of one background call and the start of
    /// the next. Zero disables pacing. `ask` ignores it: a person is waiting,
    /// and the pacer exists to protect the GPU from batch work, not from them.
    pub cooldown_secs: u64,
    /// Retired. The turn serialises calls and the job queue backs off.
    pub breaker_after: Option<usize>,
    /// Retired.
    pub breaker_probe_secs: Option<u64>,
}

/// Recording real searches so they can be judged later.
///
/// The queries a benchmark needs cannot be written from memory: phrased while
/// looking at an artifact, they reuse its vocabulary, and every retrieval system
/// passes such a pair. Only a search made in earnest, before anything came back,
/// is worth scoring against.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FeedbackConfig {
    /// Whether real searches are recorded at all. Off by default: the wording of
    /// a query is personal, and nothing here is useful to anyone but the
    /// operator.
    pub enabled: bool,
    /// Candidates stored per event. Wider than the answer on purpose — search
    /// over-fetches anyway, so the extra rows are free, and they are what lets a
    /// buried hit be confirmed later.
    pub candidates: usize,
    /// Window in which a query that extends the previous one replaces it
    /// instead of starting a new event. `0` turns folding off.
    pub coalesce_secs: i64,
    /// Days captured searches are kept. `0` keeps them forever.
    pub retain_days: i64,
    /// How often the retention sweep runs. Hours rather than minutes because
    /// `retain_days` is the only thing it enforces: a window measured in days
    /// does not need checking more than a few times a day.
    pub sweep_hours: u64,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            candidates: 20,
            coalesce_secs: 15,
            retain_days: 0,
            sweep_hours: 6,
        }
    }
}

/// Links learned from co-retrieval, and what they are allowed to do.
///
/// Every threshold here is a weight in the same units: one co-appearance is
/// `+1`, one confirmed answer is `+2`, and a half-life of thirty days is what
/// makes those numbers mean "lately" rather than "ever".
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AssociateConfig {
    /// Requires `feedback.enabled`. Without recorded searches there is nothing
    /// to learn from, and that combination is a warning at startup.
    pub enabled: bool,
    pub interval_mins: u64,
    pub half_life_days: f64,
    /// Decayed weight under which a `learning` link is deleted.
    pub prune_below: f64,
    /// Decayed weight at which a link is worth showing.
    pub show_min: f64,
    /// ...and at which it is worth one model call.
    pub judge_min: f64,
    /// Distinct binding questions a link needs before it is judged. One question
    /// asked six times is one question.
    pub judge_min_queries: i64,
    pub judge_per_sweep: i64,
    /// How many of the top ranked hits are asked what they are linked to.
    pub spread_from: usize,
    /// How many associated hits may be appended, outside `limit`.
    pub spread_max: usize,
    /// How much more activated a hit must be than the one above it to pass it.
    /// Normalised within one result list, so this is a fraction, not a weight.
    pub prime_margin: f64,
    /// Positions a hit may climb. `0` turns priming off.
    pub prime_lift: usize,
}

impl Default for AssociateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_mins: 30,
            half_life_days: 30.0,
            prune_below: 0.5,
            show_min: 2.0,
            judge_min: 4.0,
            judge_min_queries: 3,
            judge_per_sweep: 10,
            spread_from: 3,
            spread_max: 3,
            prime_margin: 0.5,
            prime_lift: 2,
        }
    }
}

/// How accessible an artifact is, and what raises it.
///
/// Being surfaced *because* of activation raises nothing: `resurface` and
/// association both leave it alone. Loops that reinforce themselves are the
/// failure mode of this whole idea, and they are closed by construction.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ActivationConfig {
    pub half_life_days: f64,
    /// Returned by a search the caller marked as seen.
    pub retrieved: f64,
    /// Opened in the detail pane.
    pub opened: f64,
    /// Judged the answer to a real question. The strong signal.
    pub confirmed: f64,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            half_life_days: 14.0,
            retrieved: 1.0,
            opened: 0.5,
            confirmed: 3.0,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ConsolidateConfig {
    /// Whether the background sweep runs at all. Capture-time near-duplicate
    /// detection is separate and always on: it costs a hash, not a query.
    pub enabled: bool,
    /// Estimated Jaccard over word shingles above which a capture is parked as
    /// a near-duplicate of an existing corpus.
    pub near_dupe_min: f64,
    /// Cosine at or above which a pair is worth an operator's attention.
    pub review_min: f32,
    /// Cosine at or above which the older artifact is superseded without
    /// asking. Deliberately far above `review_min`: two genuinely distinct
    /// artifacts about one subsystem sit around 0.88 routinely, and superseding
    /// at that score destroys knowledge rather than duplication.
    pub auto_supersede: f32,
    /// Neighbours considered per artifact when it looks for duplicates.
    pub per_point: usize,
    /// How often the sweep is queued.
    pub interval_hours: u64,
    /// How often the dedupe ticker arms units, in minutes.
    ///
    /// Its own ticker rather than a passenger on the sweep. `max_judgements`
    /// bounded what *one sweep* armed, which was right while the sweep was the
    /// only producer of pairs; the relate units file them continuously now, so a
    /// number per 24-hour tick is not a budget but a queue that only grows.
    ///
    /// The fixed quantity in this system is neither the base nor the sweep — it
    /// is what the hardware can get through. So the budget is a rate.
    pub dedupe_interval_mins: u64,
    /// Units armed per tick. With the default interval that is a ceiling of
    /// twenty calls an hour, whatever the base has grown to.
    ///
    /// Zero switches the model off entirely: pairs are still found, recorded and
    /// clustered — all of which is free — and nothing is ever asked about.
    pub max_dedupe_per_tick: usize,
    /// How many captured roots one merged artifact may be written from.
    ///
    /// Above this the component is left alone and surfaced instead. A merge of
    /// forty sources is no longer one atomic piece of knowledge, which is what
    /// `schema.sql` defines an artifact to be — so past the cap the honest
    /// answer is to stop rather than to write something nobody asked for.
    pub merge_max_roots: usize,
    /// An active artifact not confirmed accurate (`last_verified_at`) in this
    /// many days becomes a deprecation-review candidate — never anything more
    /// automatic than that. See `stale_max_hits`.
    pub stale_after_days: u32,
    /// ...and retrieved at most this many times since. Both conditions must
    /// hold: staleness alone is not suspicious for a rare topic, and
    /// popularity alone says nothing about accuracy. This is read-only input
    /// to the candidate list — it never feeds search scoring, or a frequently
    /// shown result would keep boosting its own visibility.
    pub stale_max_hits: i64,

    /// Retired. Read only so that `judge = false` can be carried across rather
    /// than ignored, and so an operator is told where the setting went.
    ///
    /// It gated whether the model was asked at all, which is now
    /// `max_dedupe_per_tick = 0`. Left unread it would parse without complaint,
    /// and the operator who had switched the only inference-costing stage off
    /// would be given an autonomous one instead — a setting that hides
    /// artifacts, arriving by upgrade, from a file that says the opposite.
    pub judge: Option<bool>,
    /// Retired. A number per 24-hour tick was a budget only while the sweep was
    /// the only producer of pairs; see `max_dedupe_per_tick`, which is a rate.
    pub max_judgements: Option<usize>,
    /// Retired. Detection is per artifact now, not a sampled sweep.
    pub sample: Option<usize>,
    /// Retired. Every verdict is acted on; every merge and supersede has undo.
    pub autonomous: Option<bool>,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            near_dupe_min: 0.90,
            review_min: 0.88,
            auto_supersede: 0.95,
            per_point: 5,
            interval_hours: 24,
            dedupe_interval_mins: 15,
            max_dedupe_per_tick: 5,
            merge_max_roots: 8,
            stale_after_days: 365,
            stale_max_hits: 0,
            judge: None,
            max_judgements: None,
            sample: None,
            autonomous: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default = "default_workers")]
    pub workers: usize,
}
fn default_workers() -> usize {
    2
}

#[derive(Debug, Deserialize, Clone)]
pub struct StoreConfig {
    pub path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct VectorConfig {
    pub url: String,
    pub collection: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// How much a result's age counts against it. Fused ranks land between
    /// roughly 0.1 and 1.0, so the default breaks near-ties in favour of the
    /// newer note without ever overturning a clearly better match. `0.0` turns
    /// recency off entirely.
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f32,
    /// Age at which a chunk has lost half of that boost.
    #[serde(default = "default_recency_half_life_days")]
    pub recency_half_life_days: u32,
    /// Extra score for a chunk carrying the `pinned` tag, so something you
    /// decided matters can outrank the decay curve.
    #[serde(default = "default_pinned_boost")]
    pub pinned_boost: f32,
    /// Cosine similarity below which a result is only loosely related to the
    /// query, and is labelled as such rather than presented like a real answer.
    ///
    /// This is a similarity, not a rank: hybrid retrieval returns reciprocal
    /// rank fusion values, which say where a result placed and nothing about
    /// how close it was, so the top hit for a typo scores exactly like the top
    /// hit for a perfect match. The similarity is read separately — see
    /// `VectorStore::search` — and compared here.
    ///
    /// Normalised embeddings put unrelated text around 0.0–0.2 and genuinely
    /// related text well above 0.4, so the default sits between them. Raise it
    /// to be told more often that nothing really matched; `0.0` turns the
    /// labelling off.
    #[serde(default = "default_weak_below")]
    pub weak_below: f32,
}
fn default_recency_weight() -> f32 {
    0.05
}
fn default_recency_half_life_days() -> u32 {
    180
}
fn default_pinned_boost() -> f32 {
    0.15
}
fn default_weak_below() -> f32 {
    0.35
}

/// A named endpoint and its defaults. Roles point at one instead of each
/// carrying its own, so "which model is this call worth" is a decision made
/// once rather than repeated per role.
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

/// The resolved roles. Deserialised through [`RawInferConfig`] so that tiers
/// are flattened away before anything downstream sees a role: `HttpCompleter`
/// and friends keep taking a struct whose every field is concrete, and a tier
/// stays a spelling of the config file rather than a concept the call path has
/// to know about.
#[derive(Debug, Deserialize, Clone)]
#[serde(try_from = "RawInferConfig")]
pub struct InferConfig {
    pub synthesize: SynthesizeRole,
    pub embed: EmbedRole,
    pub ask: AskRole,
    pub rerank: Option<RerankRole>,
    pub vision: Option<VisionRole>,
    /// Emitted by `normalize`. Collected here rather than logged during
    /// deserialization because a `TryFrom` runs before the subscriber is up,
    /// and a warning written to nowhere is the same as no warning.
    pub legacy_warnings: Vec<String>,
}

/// The file's shape, before tiers are folded into the roles. Every endpoint
/// field on a role is optional here: it comes from the tier unless the role
/// overrides it, and in the legacy shape the role carries it directly.
#[derive(Debug, Deserialize)]
pub struct RawInferConfig {
    #[serde(default)]
    tiers: HashMap<String, TierConfig>,
    synthesize: RawSynthesizeRole,
    embed: EmbedRole,
    ask: RawAskRole,
    #[serde(default)]
    rerank: Option<RerankRole>,
    #[serde(default)]
    vision: Option<RawVisionRole>,
}

#[derive(Debug, Deserialize)]
struct RawSynthesizeRole {
    #[serde(default)]
    tier: Option<String>,
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
    // Role-only, unchanged.
    output_ratio: f32,
    #[serde(default)]
    tokenizer_path: Option<String>,
    #[serde(default)]
    cooldown_secs: Option<u64>,
    #[serde(default = "default_context_opening_tokens")]
    context_opening_tokens: usize,
    #[serde(default = "default_context_overlap_tokens")]
    context_overlap_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct RawAskRole {
    #[serde(default)]
    tier: Option<String>,
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
    // Role-only.
    #[serde(default)]
    follow_up: bool,
    #[serde(default)]
    follow_up_tier: Option<String>,
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

/// What `[infer.synthesize]` and `[infer.ask]` can hand to a tier: every
/// endpoint field they have.
const ENDPOINT_KEYS: &str = "base_url, model, api_key, context_tokens and max_output_tokens";

/// What `[infer.vision]` can hand to a tier, which is much less. `model` is
/// required on the role and is never inherited — a vision model is the point of
/// the block, not a property of the server — and `max_output_tokens` is
/// deliberately not inherited either, so both have to stay where they are.
const VISION_ENDPOINT_KEYS: &str = "base_url and api_key (model stays here: it is required on the role and \
     never comes from a tier)";

/// The endpoint a role runs on: the tier it names, or `None` when it carries
/// one inline and the caller builds an anonymous tier from the role's own
/// fields.
///
/// A name that matches nothing is refused rather than defaulted. What that
/// prevents is a typo running every call of one role against a different model
/// than the operator wrote down — a divergence no later stage could notice,
/// let alone report.
///
/// `movable_keys` is the caller's because the roles do not share one list. A
/// migration warning is an instruction someone follows literally, and telling
/// `[infer.vision]` to move its `model` into a tier would delete the one key it
/// requires and cannot inherit — the block would stop loading. A shim that
/// warns instead of refusing has nothing to offer but the accuracy of the
/// sentence.
fn resolve_endpoint(
    role: &str,
    tier_name: Option<&str>,
    tiers: &HashMap<String, TierConfig>,
    inline_base_url: Option<&str>,
    movable_keys: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<TierConfig>, String> {
    if let Some(name) = tier_name {
        return tiers.get(name).cloned().map(Some).ok_or_else(|| {
            let mut known: Vec<&str> = tiers.keys().map(String::as_str).collect();
            known.sort_unstable();
            format!(
                "[infer.{role}] points at tier `{name}`, which is not defined. \
                 Known tiers: {}. Define it under [infer.tiers.{name}].",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )
        });
    }
    if inline_base_url.is_some() {
        warnings.push(format!(
            "[infer.{role}] carries its endpoint inline. Move {movable_keys} into an \
             [infer.tiers.<name>] block and write `tier = \"<name>\"` here. The inline form \
             still works and will be removed."
        ));
        return Ok(None);
    }
    Err(format!(
        "[infer.{role}] has neither `tier` nor `base_url`. Point it at an \
         [infer.tiers.<name>] block."
    ))
}

/// A field the inline shape has always required. Named here rather than left
/// to serde's `missing field`, because the same key is legal to omit in the
/// tiered shape and the message has to say which of the two is short.
fn required<T>(v: Option<T>, role: &str, field: &str) -> Result<T, String> {
    v.ok_or_else(|| {
        format!(
            "[infer.{role}] carries its endpoint inline but is missing `{field}`. \
             Add it, or point the role at an [infer.tiers.<name>] block that has it."
        )
    })
}

impl TryFrom<RawInferConfig> for InferConfig {
    type Error = String;

    fn try_from(raw: RawInferConfig) -> Result<Self, Self::Error> {
        let mut legacy_warnings = Vec::new();
        let tiers = &raw.tiers;

        let s = raw.synthesize;
        let st = match resolve_endpoint(
            "synthesize",
            s.tier.as_deref(),
            tiers,
            s.base_url.as_deref(),
            ENDPOINT_KEYS,
            &mut legacy_warnings,
        )? {
            Some(t) => t,
            None => TierConfig {
                base_url: required(s.base_url.clone(), "synthesize", "base_url")?,
                model: required(s.model.clone(), "synthesize", "model")?,
                api_key: s.api_key.clone(),
                context_tokens: required(s.context_tokens, "synthesize", "context_tokens")?,
                max_output_tokens: required(
                    s.max_output_tokens,
                    "synthesize",
                    "max_output_tokens",
                )?,
                timeout_secs: default_timeout_secs(),
                reasoning_effort: None,
                ceiling_param: None,
                structured_output: default_true(),
            },
        };
        let synthesize = SynthesizeRole {
            base_url: s.base_url.unwrap_or(st.base_url),
            model: s.model.unwrap_or(st.model),
            api_key: s.api_key.or(st.api_key),
            context_tokens: s.context_tokens.unwrap_or(st.context_tokens),
            max_output_tokens: s.max_output_tokens.unwrap_or(st.max_output_tokens),
            output_ratio: s.output_ratio,
            tokenizer_path: s.tokenizer_path,
            reasoning_effort: s.reasoning_effort.or(st.reasoning_effort),
            ceiling_param: s.ceiling_param.or(st.ceiling_param),
            timeout_secs: s.timeout_secs.unwrap_or(st.timeout_secs),
            structured_output: s.structured_output.unwrap_or(st.structured_output),
            cooldown_secs: s.cooldown_secs,
            context_opening_tokens: s.context_opening_tokens,
            context_overlap_tokens: s.context_overlap_tokens,
        };

        let a = raw.ask;
        let at = match resolve_endpoint(
            "ask",
            a.tier.as_deref(),
            tiers,
            a.base_url.as_deref(),
            ENDPOINT_KEYS,
            &mut legacy_warnings,
        )? {
            Some(t) => t,
            None => TierConfig {
                base_url: required(a.base_url.clone(), "ask", "base_url")?,
                model: required(a.model.clone(), "ask", "model")?,
                api_key: a.api_key.clone(),
                context_tokens: required(a.context_tokens, "ask", "context_tokens")?,
                // The inline shape has always defaulted this rather than
                // requiring it, and a refactor is the wrong place to stop.
                max_output_tokens: default_ask_max_output_tokens(),
                timeout_secs: default_timeout_secs(),
                reasoning_effort: None,
                ceiling_param: None,
                structured_output: default_true(),
            },
        };
        // Resolved here rather than where it is used, so a typo in the name is
        // a startup failure like every other tier name instead of a surprise on
        // the first question someone asks.
        let follow_up_endpoint = match a.follow_up_tier.as_deref() {
            Some(name) => resolve_endpoint(
                "ask.follow_up_tier",
                Some(name),
                tiers,
                None,
                ENDPOINT_KEYS,
                &mut legacy_warnings,
            )?,
            None => None,
        };
        let ask = AskRole {
            base_url: a.base_url.unwrap_or(at.base_url),
            model: a.model.unwrap_or(at.model),
            api_key: a.api_key.or(at.api_key),
            context_tokens: a.context_tokens.unwrap_or(at.context_tokens),
            max_output_tokens: a.max_output_tokens.unwrap_or(at.max_output_tokens),
            timeout_secs: a.timeout_secs.unwrap_or(at.timeout_secs),
            reasoning_effort: a.reasoning_effort.or(at.reasoning_effort),
            ceiling_param: a.ceiling_param.or(at.ceiling_param),
            follow_up: a.follow_up,
            follow_up_endpoint,
        };

        // Vision is the one role whose endpoint may legitimately be absent:
        // `None` there means the synthesize endpoint, which `VisionRole::resolve`
        // reads later. So it is folded by hand rather than through
        // `resolve_endpoint`, which would refuse that as underspecified.
        let vision = match raw.vision {
            None => None,
            Some(v) => {
                let vt = match v.tier.as_deref() {
                    Some(name) => resolve_endpoint(
                        "vision",
                        Some(name),
                        tiers,
                        None,
                        VISION_ENDPOINT_KEYS,
                        &mut legacy_warnings,
                    )?,
                    None => {
                        if v.base_url.is_some() {
                            resolve_endpoint(
                                "vision",
                                None,
                                tiers,
                                v.base_url.as_deref(),
                                VISION_ENDPOINT_KEYS,
                                &mut legacy_warnings,
                            )?;
                        }
                        None
                    }
                };
                Some(VisionRole {
                    model: v.model,
                    base_url: v
                        .base_url
                        .or_else(|| vt.as_ref().map(|t| t.base_url.clone())),
                    api_key: v
                        .api_key
                        .or_else(|| vt.as_ref().and_then(|t| t.api_key.clone())),
                    // The tier's timeout only where there is a tier: this
                    // role's own default is two minutes, which is about one
                    // image and not about whatever endpoint serves it.
                    timeout_secs: v
                        .timeout_secs
                        .or_else(|| vt.as_ref().map(|t| t.timeout_secs))
                        .unwrap_or_else(default_vision_timeout_secs),
                    // Not inherited: a tier's ceiling is sized for the role it
                    // was written for, and a description is stored as a corpus.
                    max_output_tokens: v
                        .max_output_tokens
                        .unwrap_or_else(default_vision_max_output_tokens),
                    ceiling_param: v
                        .ceiling_param
                        .or_else(|| vt.as_ref().and_then(|t| t.ceiling_param)),
                })
            }
        };

        Ok(InferConfig {
            synthesize,
            embed: raw.embed,
            ask,
            rerank: raw.rerank,
            vision,
            legacy_warnings,
        })
    }
}

/// Seconds an inference request may take before the client gives up.
///
/// Fifteen minutes, which is absurd for a hosted API and about right for the
/// case engram is built for: a small reasoning model on one consumer GPU,
/// where a single segmentation window has been measured at seven minutes and
/// 8000 output tokens. A timeout there is indistinguishable from a dead
/// endpoint to the job runner — the call fails, the job retries, and it fails
/// again at the same wall, forever.
///
/// The cost of setting it too high is a stuck job holding a worker until it
/// gives up. The cost of setting it too low is a corpus that never finishes
/// segmenting, which is worse, so the default errs long. Hosted endpoints
/// should lower it per role.
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
    /// Retired: budgets use the character estimate.
    #[serde(default)]
    pub tokenizer_path: Option<String>,
    /// Sent as `reasoning_effort` when set. A reasoning model spends output
    /// budget thinking before it writes any JSON, and that budget is the same
    /// one the chunk list has to fit in — on a small local model the thinking
    /// is what truncates the answer.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Which name this endpoint takes the output ceiling under. See
    /// [`CeilingParam`]. Unset means: infer it, then correct the guess from the
    /// endpoint's own 400.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    /// Seconds to wait on one call before giving up on it.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether to send the reply's JSON Schema as an OpenAI `json_schema`
    /// response format, so the endpoint constrains decoding to it.
    ///
    /// On by default, because unconstrained is not a safe default here: a 9B
    /// model asked for JSON will close an array with a brace, or omit a
    /// required field, often enough that windows fail permanently — and the
    /// only symptom is a parse error that reads exactly like a truncated reply.
    /// llama.cpp, vLLM and the hosted APIs all honour it. Turn it off for an
    /// endpoint that rejects the field outright.
    ///
    /// Governs the dedupe judge as well, which runs on this endpoint.
    #[serde(default = "default_true")]
    pub structured_output: bool,
    /// Moved to `[pacing]`, and kept here only to be complained about.
    ///
    /// Pacing is one queue in front of one endpoint now, so a cooldown per role
    /// could never bound the total load — several roles each honouring their own
    /// still interleave into unbroken work. Nothing reads this, and without the
    /// field the operator's thermal pacing would parse cleanly and silently stop
    /// happening: unknown keys are ignored, which is right for forward
    /// compatibility and wrong for a setting someone chose on purpose.
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
    /// Tokens of the document's verbatim opening prepended to every window, so
    /// an artifact from deep in a long document still knows what product and
    /// version it belongs to. Zero disables it.
    #[serde(default = "default_context_opening_tokens")]
    pub context_opening_tokens: usize,
    /// Tokens of each neighbouring window carried on both sides, so a window
    /// that opens mid-procedure can still resolve what its pronouns point at.
    /// Zero disables it.
    #[serde(default = "default_context_overlap_tokens")]
    pub context_overlap_tokens: usize,
}

fn default_true() -> bool {
    true
}

fn default_context_opening_tokens() -> usize {
    200
}

fn default_context_overlap_tokens() -> usize {
    150
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
}

#[derive(Debug, Deserialize, Clone)]
pub struct AskRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    /// Hard cap on output tokens per answer. An answer is prose for a person to
    /// read, so this is a generous bound on the longest one worth waiting for
    /// rather than a tuning knob — but it must be sent, because an endpoint
    /// asked for no ceiling applies its own, and the model's own stopping is
    /// the only thing between the two.
    #[serde(default = "default_ask_max_output_tokens")]
    pub max_output_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// See `SynthesizeRole::reasoning_effort`.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// See `SynthesizeRole::ceiling_param`.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    /// One bounded extra retrieval round. Off by default: it costs a call, and
    /// a default moves only after the harness has run.
    pub follow_up: bool,
    /// The resolved endpoint the "what do I still need" call runs on, from
    /// `follow_up_tier`. `None` falls back to this role's own endpoint.
    ///
    /// A `TierConfig` rather than a role, because that is honestly what it is:
    /// an endpoint and its ceilings, handed straight to a completer. That call
    /// is a cheap classification and belongs on the efficient model even when
    /// the answer it feeds belongs on the deep one — which is the capability
    /// the tier names exist to express.
    pub follow_up_endpoint: Option<TierConfig>,
}

fn default_ask_max_output_tokens() -> usize {
    4096
}

/// The request field an endpoint takes the output ceiling under.
///
/// The two names mean the same ceiling and no endpoint accepts both: OpenAI's
/// reasoning models answer a `max_tokens` with a 400 naming
/// `max_completion_tokens`, while a llama.cpp or vLLM build reads `max_tokens`
/// and silently ignores anything it does not recognise — which is the dangerous
/// half, because an unrecognised ceiling is no ceiling at all and the reply is
/// bounded only by the server's own limit.
///
/// Left unset, the name is inferred from `reasoning_effort` and then corrected
/// from the endpoint's 400 if the guess was wrong. That inference is a guess and
/// not a fact: `reasoning_effort = "none"` is exactly what the local-endpoint
/// documentation recommends, and a local endpoint wants `max_tokens`. Set this
/// when the guess is wrong and the endpoint is one of the silent ones, which
/// have no 400 for the probe to learn from.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CeilingParam {
    MaxTokens,
    MaxCompletionTokens,
}

impl CeilingParam {
    /// The name to send the ceiling under when the operator has not said.
    ///
    /// A reasoning model refuses `max_tokens` outright — a 400 naming
    /// `max_completion_tokens` — and `reasoning_effort` is the only signal
    /// available that one is on the other end.
    ///
    /// The two ways of guessing wrong do not cost the same, which is what
    /// decides the ambiguous value. Guess `max_completion_tokens` at a local
    /// endpoint and the field is silently ignored: no ceiling at all, no 400 to
    /// learn from, and a reply bounded only by the server's own limit. Guess
    /// `max_tokens` at a reasoning endpoint and it says so, in a 400 the
    /// transport reads once and remembers. Where the signal is ambiguous, the
    /// safe guess is the one that self-corrects.
    ///
    /// `"none"` is exactly that case. It is what the local-endpoint
    /// documentation — and `config.example.toml` — recommend for suppressing a
    /// local model's thinking, and it is *also* a value hosted reasoning models
    /// accept, so it says nothing either way. Reading it as `max_tokens` costs
    /// a hosted endpoint one self-correcting 400 and saves a silent local one
    /// from running with no ceiling at all.
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

    /// The other name — what a rejected ceiling is retried under.
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
}

/// The vision model that reads a captured image into text. Optional: absent
/// means the image door is closed. `base_url` and `api_key` are optional
/// because the common case is the synthesize endpoint serving a multimodal
/// model too — then only `model` needs saying.
#[derive(Debug, Deserialize, Clone)]
pub struct VisionRole {
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_vision_timeout_secs")]
    pub timeout_secs: u64,
    /// Hard cap on output tokens per description, sent on every call.
    ///
    /// A description is markdown for a person and for the segmenter that reads
    /// it next, so this is a generous bound on the longest one worth waiting
    /// for — but it must be sent, for the reason every other role sends one: an
    /// endpoint asked for no ceiling applies its own, which is far larger, and
    /// a vision model handed a dense screenshot is exactly the kind of caller
    /// that keeps writing. What it produces is stored as a corpus, so an
    /// unbounded reply is not merely a slow call.
    #[serde(default = "default_vision_max_output_tokens")]
    pub max_output_tokens: usize,
    /// See `SynthesizeRole::ceiling_param`. Inherited from the synthesize role
    /// when this one has no endpoint of its own, since then it is that endpoint.
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
    /// The endpoint and key this role actually calls: its own where given,
    /// the synthesize role's otherwise.
    pub fn resolve(&self, synth: &SynthesizeRole) -> (String, Option<String>) {
        (
            self.base_url
                .clone()
                .unwrap_or_else(|| synth.base_url.clone()),
            self.api_key.clone().or_else(|| synth.api_key.clone()),
        )
    }

    /// Which name to send the output ceiling under. Inherited only when this
    /// role borrows the synthesize endpoint: a setting about how one server
    /// reads a request says nothing about a different server.
    pub fn ceiling_param(&self, synth: &SynthesizeRole) -> Option<CeilingParam> {
        self.ceiling_param.or_else(|| {
            self.base_url
                .is_none()
                .then_some(synth.ceiling_param)
                .flatten()
        })
    }

    /// The effort the ceiling name is inferred from when neither role names it,
    /// inherited on the same condition and for the same reason as
    /// [`Self::ceiling_param`].
    ///
    /// Without this, a synthesize role that sets `reasoning_effort` and leaves
    /// `ceiling_param` unset has nothing to inherit, and the two roles guess
    /// different names for the one server they share. This role never sends
    /// `reasoning_effort` itself — it is read here only as the signal about how
    /// that server reads a request.
    pub fn inherited_reasoning_effort<'a>(&self, synth: &'a SynthesizeRole) -> Option<&'a str> {
        self.base_url
            .is_none()
            .then_some(synth.reasoning_effort.as_deref())
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
    pub redirect_url: String,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub allowed_subs: Vec<String>,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// Group names from the provider's `groups` claim. Nextcloud's OIDC
    /// provider app only sends this when the admin has turned on group
    /// provisioning for the client; without it the claim is simply absent; and
    /// a subject in a listed group is admitted the same as one listed by
    /// subject or email.
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
        let mut cfg: Config = raw.try_deserialize()?;
        cfg.carry_retired_keys();
        cfg.normalize();
        cfg.validate()?;
        cfg.warn_on_file_secrets(path);
        cfg.warn_on_moved_keys();
        cfg.warn_on_inferred_ceiling_param();
        Ok(cfg)
    }

    /// A retired key still says what its operator wanted, and is honoured where
    /// something current can carry it.
    ///
    /// `judge` gated whether the dedupe pass was asked anything at all. That
    /// file is the record of an operator declining the one stage that spends
    /// inference and hides artifacts, so it is carried to the setting that
    /// means the same thing now.
    ///
    /// `max_judgements` has no successor to carry to — a count per tick and a
    /// rate are not the same quantity — so it is only named.
    fn carry_retired_keys(&mut self) {
        match self.consolidate.judge {
            Some(false) => {
                self.consolidate.max_dedupe_per_tick = 0;
                tracing::warn!(
                    "consolidate.judge has been retired; reading judge = false as \
                     max_dedupe_per_tick = 0, which is what stops the dedupe pass \
                     asking anything now."
                );
            }
            Some(true) => tracing::warn!(
                "consolidate.judge has been retired and is being ignored; \
                 max_dedupe_per_tick decides how many groups are asked about per tick"
            ),
            None => {}
        }
        if let Some(n) = self.consolidate.max_judgements {
            tracing::warn!(
                was = n,
                interval_mins = self.consolidate.dedupe_interval_mins,
                per_tick = self.consolidate.max_dedupe_per_tick,
                "consolidate.max_judgements has been retired and is being ignored; \
                 the budget is a rate now — see dedupe_interval_mins and max_dedupe_per_tick"
            );
        }
        if self.consolidate.sample.is_some() {
            tracing::warn!(
                "consolidate.sample has been retired and is being ignored; every artifact \
                 looks for its own duplicates when it is indexed"
            );
        }
        if self.consolidate.autonomous.is_some() {
            tracing::warn!(
                "consolidate.autonomous has been retired and is being ignored; every verdict \
                 is acted on, and every merge and supersede has an undo. \
                 max_dedupe_per_tick = 0 is what stops the pass asking"
            );
        }
    }

    /// Values that would make a feature quietly useless, put back rather than
    /// refused.
    ///
    /// `feedback.candidates = 0` stores an empty pool for every captured
    /// search: every card renders with nothing to choose, every judgement is
    /// forced through "none of these", and every one of those is recorded as a
    /// find — a ranking failure that never happened, permanently in the
    /// dataset. Nobody types a zero meaning that. It goes back to the default
    /// with a line in the log rather than stopping a server over a number that
    /// only affects an optional feature.
    ///
    /// The ceiling is the other end of the same argument. A captured search
    /// fetches at least `candidates` vectors whatever the caller asked for, so
    /// the number is the width of every search through a captured door, not
    /// just the depth of the pool stored behind it. Left unbounded, a four-digit
    /// value read as "keep plenty" turns every API call into a four-digit vector
    /// fetch, and nothing in the file says so. The ceiling is what the widest
    /// legal search already costs: `MAX_LIMIT` results over-fetched by the
    /// candidate multiplier.
    fn normalize(&mut self) {
        for w in &self.infer.legacy_warnings {
            tracing::warn!("{w}");
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
        let ceiling = crate::core::search::MAX_LIMIT * crate::core::search::CANDIDATE_MULTIPLIER;
        if self.feedback.candidates > ceiling {
            tracing::warn!(
                configured = self.feedback.candidates,
                using = ceiling,
                "feedback.candidates is the fetch width of every captured search; \
                 capping it at the widest ordinary search"
            );
            self.feedback.candidates = ceiling;
        }
        // Same argument as above: every judgeable component flattens to at
        // least two roots, so a cap of zero or one settles all of them
        // Oversized before any call — merging silently off from a number
        // nobody types meaning that.
        if self.consolidate.merge_max_roots < 2 {
            let d = ConsolidateConfig::default().merge_max_roots;
            tracing::warn!(
                configured = self.consolidate.merge_max_roots,
                using = d,
                "consolidate.merge_max_roots below 2 would settle every component \
                 as oversized; using the default"
            );
            self.consolidate.merge_max_roots = d;
        }
        // The association widths multiply each other on the search path to
        // size one SQL `LIMIT`, and `interval_mins` is multiplied by sixty to
        // make a `Duration`. None of the three has a ceiling that comes from
        // anywhere else, so it is stated here: past these, the number has
        // stopped describing a search someone would run or a rhythm someone
        // would wait for, and the arithmetic is the only thing still reading
        // it. Clamped rather than refused — an oversized width is a typo, not
        // a config that destroys anything.
        const MAX_SPREAD_FROM: usize = 64;
        const MAX_SPREAD_MAX: usize = 64;
        const MAX_PRIME_LIFT: usize = 64;
        // A year. Longer than this and the ticker fires once and effectively
        // never again, which the operator can say by setting `enabled = false`.
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
    }

    /// Rules that a config can satisfy syntactically and still be wrong.
    ///
    /// The thresholds are the only ones so far, and they are worth refusing to
    /// start over: `auto_supersede` at or below `review_min` means every pair
    /// the sweep finds is hidden without asking, with no review band left at
    /// all. That destroys knowledge quietly, and the operator who typed one
    /// number would find out from search results going missing weeks later.
    fn validate(&self) -> Result<(), ConfigError> {
        let c = &self.consolidate;
        if c.auto_supersede <= c.review_min {
            return Err(ConfigError::Invalid(format!(
                "consolidate.auto_supersede ({}) must be above consolidate.review_min ({}), \
                 or every pair found is hidden without review",
                c.auto_supersede, c.review_min
            )));
        }
        Ok(())
    }

    /// A setting that moved is a setting that stopped working, and an unknown
    /// key parses without complaint. Say so once at startup rather than letting
    /// an operator discover the pacing they configured has been off since the
    /// upgrade.
    fn warn_on_moved_keys(&self) {
        if self.pacing.breaker_after.is_some() || self.pacing.breaker_probe_secs.is_some() {
            tracing::warn!(
                "pacing.breaker_after / breaker_probe_secs have been retired and are being \
                 ignored; one background call runs at a time and a failing unit backs off"
            );
        }
        if self.infer.synthesize.tokenizer_path.is_some() {
            tracing::warn!(
                "infer.synthesize.tokenizer_path has been retired and is being ignored; \
                 token budgets use the character estimate"
            );
        }
        if self.infer.synthesize.cooldown_secs.is_some() {
            tracing::warn!(
                "infer.synthesize.cooldown_secs has moved to [pacing].cooldown_secs and is \
                 being ignored; pacing is one gap in front of one endpoint now, so it can no \
                 longer be set per role"
            );
        }
        if self.associate.enabled && !self.feedback.enabled {
            tracing::warn!(
                "associate.enabled has no effect while feedback.enabled is false: links are \
                 learned from recorded searches, and none are being recorded. Recording queries \
                 is a privacy decision, so it keeps its own switch."
            );
        }
    }

    /// The output ceiling's name is a guess whenever `reasoning_effort` is set
    /// and `ceiling_param` is not, and one of the two ways it can be wrong is
    /// silent: an endpoint that ignores the field it does not recognise applies
    /// no ceiling and returns no error, so nothing downstream ever finds out.
    ///
    /// Say which way it guessed, at startup, where an operator can compare it
    /// against the server they know they are running. The 400 that corrects the
    /// other direction needs no warning — it corrects itself.
    fn warn_on_inferred_ceiling_param(&self) {
        let synth = &self.infer.synthesize;
        let mut roles: Vec<(&str, Option<&str>, Option<CeilingParam>)> = vec![
            (
                "infer.synthesize",
                synth.reasoning_effort.as_deref(),
                synth.ceiling_param,
            ),
            (
                "infer.ask",
                self.infer.ask.reasoning_effort.as_deref(),
                self.infer.ask.ceiling_param,
            ),
        ];
        // Vision resolves a name the same way, off values it may have inherited
        // from synthesize — and it is the role a dropped ceiling costs most,
        // since what it writes is stored as a corpus and segmented again.
        if let Some(v) = &self.infer.vision {
            roles.push((
                "infer.vision",
                v.inherited_reasoning_effort(synth),
                v.ceiling_param(synth),
            ));
        }
        for (role, effort, configured) in roles {
            let (Some(effort), None) = (effort, configured) else {
                continue;
            };
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

    /// Secrets belong in the environment. A secret sitting in the config file
    /// is a real risk (it gets committed), so say so loudly rather than
    /// rejecting a config that otherwise works.
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
        c.infer.synthesize.api_key = c.infer.synthesize.api_key.map(|_| R.into());
        c.infer.embed.api_key = c.infer.embed.api_key.map(|_| R.into());
        c.infer.ask.api_key = c.infer.ask.api_key.map(|_| R.into());
        if let Some(f) = c.infer.ask.follow_up_endpoint.as_mut() {
            f.api_key = f.api_key.as_ref().map(|_| R.into());
        }
        if let Some(r) = c.infer.rerank.as_mut() {
            r.api_key = r.api_key.as_ref().map(|_| R.into());
        }
        if let Some(v) = c.infer.vision.as_mut() {
            v.api_key = v.api_key.as_ref().map(|_| R.into());
        }
        if let Some(o) = c.auth.oidc.as_mut() {
            o.client_secret = o.client_secret.as_ref().map(|_| R.into());
        }
        if let Some(l) = c.auth.local.as_mut() {
            l.password_hash = R.into();
        }
        format!("{c:#?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables are process-global, but `cargo test` runs tests on
    /// parallel threads. Without this, the env-override test mutates `ENGRAM__*`
    /// while another test is deserializing config and the two race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_capture_defaults_are_the_documented_ones() {
        let c = CaptureConfig::default();
        assert_eq!(c.fetch_timeout_secs, 30);
        assert_eq!(c.fetch_max_bytes, 8 * 1024 * 1024);
        // The floor below which extraction is reported as a failure rather
        // than stored as a corpus.
        assert_eq!(c.min_extracted_chars, 200);
    }

    #[test]
    fn the_example_config_carries_the_capture_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.capture.min_extracted_chars, 200);
    }

    /// The setting exists for the endpoint the guess gets wrong, so the value an
    /// operator types has to be the value the wire carries. Unset stays unset —
    /// that is what leaves the guess and its correction in charge.
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
        // And the two names share no substring, which is what lets a rejection
        // naming one of them be read unambiguously.
        assert!(
            !CeilingParam::MaxCompletionTokens
                .as_str()
                .contains(CeilingParam::MaxTokens.as_str())
        );

        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(
            cfg.infer.ask.ceiling_param.is_none() && cfg.infer.synthesize.ceiling_param.is_none(),
            "the example config pins a ceiling name it should be leaving to the guess"
        );
    }

    #[test]
    fn the_default_timeout_survives_a_slow_local_model() {
        // A segmentation window against a 9B model on one consumer GPU has been
        // measured at seven minutes. Anything shorter turns a working setup
        // into an endless retry loop, so this number is load-bearing rather
        // than arbitrary.
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
        assert_eq!(cfg.infer.synthesize.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(cfg.infer.embed.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(cfg.infer.ask.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(cfg.infer.synthesize.reasoning_effort, None);
    }

    #[test]
    fn a_merge_cap_below_two_goes_back_to_the_default() {
        // The same put-back as feedback.candidates: every judgeable component
        // flattens to at least two roots, so a cap of 0 or 1 settles all of
        // them Oversized before any call — merging silently off.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[consolidate]\nmerge_max_roots = 1\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.consolidate.merge_max_roots,
            ConsolidateConfig::default().merge_max_roots
        );
    }

    #[test]
    fn a_zero_candidate_pool_is_put_back_to_the_default() {
        // Zero would store an empty pool for every captured search: nothing to
        // choose on any card, so every judgement is forced through "none of
        // these" and recorded as a find that never happened.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[feedback]\nenabled = true\ncandidates = 0\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.feedback.candidates,
            FeedbackConfig::default().candidates
        );
        assert!(cfg.feedback.enabled, "the rest of the section was dropped");
    }

    #[test]
    fn an_oversized_candidate_pool_is_capped_at_the_widest_ordinary_search() {
        // A captured search fetches at least this many vectors whatever the
        // caller asked for, so the number is the width of every UI, API and MCP
        // search — not just the depth of the pool stored behind it. Four digits
        // here silently made every API call a four-digit vector fetch.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[feedback]\nenabled = true\ncandidates = 2000\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.feedback.candidates,
            crate::core::search::MAX_LIMIT * crate::core::search::CANDIDATE_MULTIPLIER
        );
    }

    #[test]
    fn a_deliberate_candidate_count_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 5\n"));
        assert_eq!(Config::load(Some(&p)).unwrap().feedback.candidates, 5);
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("config.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    const MINIMAL: &str = r#"
[server]
bind = "127.0.0.1:8080"

[store]
path = "engram.db"

[vector]
url = "http://localhost:6334"
collection = "chunks"

[infer.synthesize]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 8192
output_ratio = 1.4

[infer.embed]
base_url = "http://localhost:8000/v1"
model = "bge-m3"
dim = 1024
max_input_tokens = 8192

[infer.ask]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768

[auth]
mode = "local"

[auth.local]
username = "dev"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"
"#;

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
    fn an_operator_who_switched_the_judge_off_is_still_not_asked() {
        // `judge = false` is the record of someone declining the one stage that
        // spends inference and hides artifacts. It is carried to the key that
        // stops the asking.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[consolidate]\njudge = false\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.consolidate.max_dedupe_per_tick, 0,
            "a config declining the model call was asked anyway"
        );
    }

    #[test]
    fn a_retired_key_does_not_stop_a_config_that_otherwise_works() {
        // Named in the log, not refused. `max_judgements` has no successor to
        // carry it to, and a server that will not start is a worse answer than
        // one that says where the setting went.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[consolidate]\nmax_judgements = 20\njudge = true\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.consolidate.max_dedupe_per_tick,
            ConsolidateConfig::default().max_dedupe_per_tick
        );
    }

    #[test]
    fn thresholds_that_leave_no_review_band_are_refused() {
        // `auto_supersede` at or below `review_min` hides every pair the sweep
        // finds without asking anyone. The operator would find out from search
        // results going missing, weeks later.
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
        // Its own ceiling, sent on every call like every other role's.
        assert_eq!(v.max_output_tokens, 4096);
        let (url, key) = v.resolve(&cfg.infer.synthesize);
        assert_eq!(url, cfg.infer.synthesize.base_url);
        assert_eq!(key, cfg.infer.synthesize.api_key);
    }

    /// How a request is read is a property of the server reading it, so the
    /// borrowed name travels exactly as far as the borrowed endpoint does.
    #[test]
    fn the_ceiling_name_is_inherited_only_by_a_vision_role_sharing_the_endpoint() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let mut cfg = Config::load(Some(&p)).unwrap();
        cfg.infer.synthesize.ceiling_param = Some(CeilingParam::MaxTokens);
        let synth = cfg.infer.synthesize.clone();
        let v = cfg.infer.vision.as_mut().expect("configured");

        assert_eq!(v.ceiling_param(&synth), Some(CeilingParam::MaxTokens));

        // Its own endpoint: a different server, and nothing carries over.
        v.base_url = Some("http://vision:9000/v1".into());
        assert_eq!(v.ceiling_param(&synth), None);

        // Unless it says so itself, which beats both.
        v.ceiling_param = Some(CeilingParam::MaxCompletionTokens);
        assert_eq!(
            v.ceiling_param(&synth),
            Some(CeilingParam::MaxCompletionTokens)
        );
    }

    /// The setting is not the only thing a borrowed endpoint has to inherit.
    /// With `ceiling_param` unset there is nothing explicit to carry, and the
    /// name is inferred from `reasoning_effort` — so a role that borrows the
    /// endpoint has to borrow the signal too, or the two guess differently about
    /// one server.
    #[test]
    fn the_effort_the_name_is_guessed_from_travels_with_the_borrowed_endpoint() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let mut cfg = Config::load(Some(&p)).unwrap();
        cfg.infer.synthesize.reasoning_effort = Some("high".into());
        let synth = cfg.infer.synthesize.clone();
        let v = cfg.infer.vision.as_mut().expect("configured");

        assert_eq!(v.ceiling_param(&synth), None, "nothing explicit to inherit");
        assert_eq!(v.inherited_reasoning_effort(&synth), Some("high"));

        // Its own address is its own server, and the signal stops there.
        v.base_url = Some("http://vision:9000/v1".into());
        assert_eq!(v.inherited_reasoning_effort(&synth), None);
    }

    /// The ceiling's name is guessed from `reasoning_effort`, and the two ways
    /// of guessing wrong do not cost the same: a hosted endpoint refuses the
    /// wrong name in a 400 the transport learns from, while a local one ignores
    /// it silently and applies no ceiling at all. `"none"` is the value that
    /// says nothing either way — both kinds accept it, and it is what this
    /// project recommends for suppressing local thinking — so it has to fall to
    /// the side that self-corrects.
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
            .resolve(&cfg.infer.synthesize);
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
    }

    #[test]
    fn the_association_defaults_are_the_documented_ones() {
        let a = AssociateConfig::default();
        assert!(a.enabled);
        assert_eq!(a.interval_mins, 30);
        assert_eq!(a.half_life_days, 30.0);
        assert_eq!((a.show_min, a.judge_min, a.prune_below), (2.0, 4.0, 0.5));
        assert_eq!((a.spread_from, a.spread_max), (3, 3));
        assert_eq!((a.prime_margin, a.prime_lift), (0.5, 2));
        let v = ActivationConfig::default();
        assert_eq!(v.half_life_days, 14.0);
        assert_eq!((v.retrieved, v.opened, v.confirmed), (1.0, 0.5, 3.0));
    }

    #[test]
    fn a_config_with_no_association_block_still_gets_one() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.associate.enabled);
        // ...and the feature is inert regardless, because there is nothing to
        // learn from until searches are recorded.
        assert!(!cfg.feedback.enabled);
    }

    #[test]
    fn the_example_config_carries_the_association_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.associate.spread_max, 3);
    }
    /// The tier tests below say nothing about auth, but a `Config` cannot be
    /// loaded without one; kept out of the fixtures so they read as the shapes
    /// they are actually about.
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

    /// The whole point of the rename: a role that names a tier and a role that
    /// carries the same endpoint inline must produce the same completer settings.
    /// If these ever diverge, an operator's migration silently changes their model.
    #[test]
    fn a_tier_and_an_inline_endpoint_resolve_to_the_same_role() {
        let _guard = env_guard();
        let tiered = load_infer(
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
        .expect("tiered config parses");

        let inline = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.synthesize]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        "#,
        )
        .expect("the legacy shape still parses");

        assert_eq!(
            tiered.infer.synthesize.base_url,
            inline.infer.synthesize.base_url
        );
        assert_eq!(tiered.infer.synthesize.model, inline.infer.synthesize.model);
        assert_eq!(
            tiered.infer.synthesize.context_tokens,
            inline.infer.synthesize.context_tokens
        );
        assert_eq!(
            tiered.infer.synthesize.max_output_tokens,
            inline.infer.synthesize.max_output_tokens
        );
        assert_eq!(tiered.infer.ask.base_url, inline.infer.ask.base_url);
        assert_eq!(tiered.infer.ask.model, inline.infer.ask.model);
    }

    /// A role may override any field its tier defines. Without this the two tiers
    /// would have to multiply by every ceiling an operator wants.
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
            cfg.infer.ask.max_output_tokens, 4096,
            "the role's value wins"
        );
        assert_eq!(
            cfg.infer.ask.context_tokens, 131072,
            "unset fields come from the tier"
        );
        assert_eq!(cfg.infer.synthesize.max_output_tokens, 16384);
    }

    /// A typo in a tier name must be a startup failure naming the typo, never a
    /// silent fallback to some other model.
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

    /// The legacy shape is accepted, but never silently: an operator must be told
    /// what to write instead. Same reasoning as `SynthesizeRole::cooldown_secs`.
    #[test]
    fn the_legacy_shape_records_a_warning_naming_its_replacement() {
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
        [infer.synthesize]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        "#,
        )
        .unwrap();
        assert_eq!(cfg.infer.legacy_warnings.len(), 2, "one per inline role");
        assert!(
            cfg.infer
                .legacy_warnings
                .iter()
                .any(|w| w.contains("infer.synthesize"))
        );
        assert!(
            cfg.infer
                .legacy_warnings
                .iter()
                .any(|w| w.contains("infer.tiers"))
        );
    }

    /// The example config is the migration's own documentation, so it has to be
    /// the shape being migrated *to* — and resolve to what it resolved to before.
    #[test]
    fn the_example_config_reaches_its_endpoints_through_tiers() {
        let text = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            text.contains("[infer.tiers."),
            "the example must show a tier"
        );
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(
            cfg.infer.legacy_warnings.is_empty(),
            "the example config still carries an inline endpoint: {:?}",
            cfg.infer.legacy_warnings
        );
        assert_eq!(cfg.infer.synthesize.context_tokens, 32768);
        assert_eq!(cfg.infer.synthesize.max_output_tokens, 16384);
        assert_eq!(cfg.infer.ask.context_tokens, 32768);
        assert_eq!(cfg.infer.ask.max_output_tokens, 4096);
    }
    /// A tiered config with one endpoint everything can point at, so the tests
    /// below say only the thing they are about. `[infer.ask]` is last, which is
    /// what lets a test append a key to it.
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

    /// Vision is the one role folded by hand rather than through
    /// `resolve_endpoint`, and the two halves of that are asymmetric on
    /// purpose: a timeout describes the server, so it travels with the tier,
    /// and an output ceiling describes what the reply is for, so it does not.
    /// A later tidy-up that ran vision through the shared path would give it a
    /// fifteen-minute timeout and the segmenter's ceiling with a green suite,
    /// so the asymmetry is pinned here rather than left to a comment.
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
        assert!(
            cfg.infer.legacy_warnings.is_empty(),
            "a fully tiered config has nothing to migrate: {:?}",
            cfg.infer.legacy_warnings
        );
        assert!(!cfg.redacted().contains("tier-key"), "tier key leaked");
    }

    /// A vision block with no endpoint at all is the documented common case —
    /// one server hosting a multimodal model — not a legacy shape. Warning
    /// about it would train operators to ignore the warning that matters.
    #[test]
    fn a_vision_role_borrowing_the_synthesize_endpoint_is_not_a_legacy_shape() {
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
            v.resolve(&cfg.infer.synthesize).0,
            cfg.infer.synthesize.base_url
        );
        assert!(
            cfg.infer.legacy_warnings.is_empty(),
            "borrowing the synthesize endpoint is not something to migrate: {:?}",
            cfg.infer.legacy_warnings
        );
    }

    /// The migration warning is an instruction someone follows literally, so
    /// per role it must name only the keys that role can actually move. Told to
    /// move `model` out of `[infer.vision]`, an operator would delete the one
    /// key the block requires and cannot inherit, and vision would stop loading
    /// entirely — a shim doing more damage than the shape it deprecates.
    #[test]
    fn the_vision_warning_names_only_the_keys_vision_can_move() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\nbase_url = \"http://vision:9000/v1\"\n"
        ))
        .unwrap();
        assert_eq!(
            cfg.infer.legacy_warnings.len(),
            1,
            "only vision carries an endpoint inline here"
        );
        let w = &cfg.infer.legacy_warnings[0];
        assert!(w.contains("[infer.vision]"), "{w}");
        assert!(w.contains("base_url and api_key"), "{w}");
        assert!(
            w.contains("model stays here"),
            "the message has to say what not to move: {w}"
        );
        assert!(
            !w.contains("context_tokens"),
            "not a vision key at all: {w}"
        );
        // And the endpoint it named is still the one it calls.
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(v.resolve(&cfg.infer.synthesize).0, "http://vision:9000/v1");
    }

    /// The follow-up call's whole reason to name a tier is to run somewhere
    /// cheaper than the answer it feeds, so the endpoint has to arrive resolved
    /// and complete — the completer is handed this, not a role.
    #[test]
    fn a_follow_up_tier_resolves_to_a_complete_endpoint() {
        let _guard = env_guard();
        let cfg = load_infer(&format!("{TIERED}\nfollow_up_tier = \"efficient\"\n")).unwrap();
        let f = cfg
            .infer
            .ask
            .follow_up_endpoint
            .as_ref()
            .expect("the named tier resolved");
        assert_eq!(f.base_url, "http://localhost:8000/v1");
        assert_eq!(f.model, "qwen");
        assert_eq!(f.max_output_tokens, 16384);
        assert_eq!(f.api_key.as_deref(), Some("tier-key"));
        assert!(
            !cfg.redacted().contains("tier-key"),
            "the follow-up endpoint's key leaked"
        );
    }

    /// Resolved at startup for the same reason every other tier name is: a typo
    /// must fail where the operator can see it, not on the first question
    /// somebody asks.
    #[test]
    fn a_follow_up_tier_that_does_not_exist_is_refused_like_any_other() {
        let _guard = env_guard();
        let err = load_infer(&format!("{TIERED}\nfollow_up_tier = \"efficent\"\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("follow_up_tier"), "name the key: {err}");
        assert!(
            err.contains("efficent"),
            "the error must name the typo: {err}"
        );
        assert!(err.contains("efficient"), "and what was available: {err}");
    }
}
