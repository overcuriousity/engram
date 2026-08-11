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
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ConsolidateConfig {
    pub enabled: bool,
    pub near_dupe_min: f64,
    pub review_min: f32,
    pub auto_supersede: f32,
    pub sample: usize,
    pub per_point: usize,
    pub interval_hours: u64,
    pub judge: bool,
    pub max_judgements: usize,
    pub stale_after_days: u32,
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
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f32,
    #[serde(default = "default_recency_half_life_days")]
    pub recency_half_life_days: u32,
    #[serde(default = "default_pinned_boost")]
    pub pinned_boost: f32,
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
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub cooldown_secs: u64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
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
        let cfg: Config = raw.try_deserialize()?;
        cfg.validate()?;
        cfg.warn_on_file_secrets(path);
        Ok(cfg)
    }

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

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_default_timeout_survives_a_slow_local_model() {
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
