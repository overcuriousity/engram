use serde::Deserialize;
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
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fetch_timeout_secs: 30,
            fetch_max_bytes: 8 * 1024 * 1024,
            min_extracted_chars: 200,
        }
    }
}

/// Pacing for every inference call, not just synthesis.
///
/// The roles share one GPU, so a per-role gap could not bound total load: three
/// roles each honouring their own cooldown still interleave into unbroken work.
/// One gap in front of all of them is the only version of this setting that
/// means what it says.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PacingConfig {
    /// Minimum seconds between the end of one background call and the start of
    /// the next. Zero disables pacing. `ask` ignores it: a person is waiting,
    /// and the pacer exists to protect the GPU from batch work, not from them.
    pub cooldown_secs: u64,
    /// Consecutive transport failures before background calls are held.
    /// Unreadable model output does not count — the endpoint answered. Zero
    /// disables the breaker, as zero disables the cooldown above.
    pub breaker_after: usize,
    /// How long to hold them for before letting one through to probe.
    pub breaker_probe_secs: u64,
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: 0,
            breaker_after: 3,
            breaker_probe_secs: 60,
        }
    }
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
    /// Points sampled from the collection per sweep by the matrix API.
    pub sample: usize,
    /// Neighbours considered per sampled point.
    pub per_point: usize,
    /// How often the sweep is queued.
    pub interval_hours: u64,
    /// Whether pairs in the review band that survive the fact-token prefilter
    /// are sent to the completer. Off by default: it is the only part of
    /// consolidation that costs inference.
    pub judge: bool,
    /// Ceiling on judge calls per sweep, so one sweep cannot occupy the GPU.
    pub max_judgements: usize,
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
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            near_dupe_min: 0.90,
            review_min: 0.88,
            auto_supersede: 0.95,
            sample: 2000,
            per_point: 5,
            interval_hours: 24,
            judge: false,
            max_judgements: 20,
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

#[derive(Debug, Deserialize, Clone)]
pub struct InferConfig {
    pub synthesize: SynthesizeRole,
    pub embed: EmbedRole,
    pub ask: AskRole,
    #[serde(default)]
    pub rerank: Option<RerankRole>,
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
    #[serde(default)]
    pub tokenizer_path: Option<String>,
    /// Sent as `reasoning_effort` when set. A reasoning model spends output
    /// budget thinking before it writes any JSON, and that budget is the same
    /// one the chunk list has to fit in — on a small local model the thinking
    /// is what truncates the answer.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Seconds to wait on one call before giving up on it.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
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
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// See `SynthesizeRole::reasoning_effort`.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
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
        cfg.normalize();
        cfg.validate()?;
        cfg.warn_on_file_secrets(path);
        cfg.warn_on_moved_keys();
        Ok(cfg)
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
        if self.infer.synthesize.cooldown_secs.is_some() {
            tracing::warn!(
                "infer.synthesize.cooldown_secs has moved to [pacing].cooldown_secs and is \
                 being ignored; pacing is one gap in front of one endpoint now, so it can no \
                 longer be set per role"
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
        if let Some(r) = c.infer.rerank.as_mut() {
            r.api_key = r.api_key.as_ref().map(|_| R.into());
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
}
