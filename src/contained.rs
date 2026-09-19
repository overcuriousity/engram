//! A whole engram behind one call, for an app that carries its own.
//!
//! What `main` does for a server, shaped as a function: a config, the control
//! store, the tenant registry over SQLite vectors and in-process models, the
//! workers, and the router on a loopback port of the kernel's choosing. The
//! caller gets the port and a token minted for this launch, and a handle that
//! stops it all.

use crate::error::{Error, Result};
use crate::infer::local::LocalModels;
use std::path::Path;
use std::sync::Arc;

/// The one person a contained engram serves.
pub const SUBJECT: &str = "phone";
/// What this launch's token is called, and so what last launch's was.
const TOKEN_NAME: &str = "contained";

pub struct Started {
    pub port: u16,
    pub token: String,
}

/// An OpenAI-compatible endpoint of the person's choosing, for the one role a
/// phone may not be able to carry a model for.
#[derive(Clone, Default)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// What a launch is given: the model files on the device, and where to ask
/// when there is no file for that.
#[derive(Default)]
pub struct Setup {
    pub models: LocalModels,
    pub ask: Option<Endpoint>,
}

pub struct Running {
    /// The queue's gate on generating stages, and whether anything could do
    /// that work if it were opened.
    generation: Arc<std::sync::atomic::AtomicBool>,
    can_generate: bool,
    shutdown: tokio::sync::watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::Internal(e.to_string())
}

/// A string as a TOML basic string. A data directory is somebody else's
/// choice of characters, and the config is text.
fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The hash `[auth.local]` has to carry, made once per base.
///
/// Nothing ever logs in with it: the app holds a token, and there is no page
/// to type a password into. It is random rather than fixed because a fixed one
/// would be a password anyone could read in this repository, and kept in a
/// file rather than made per launch because Argon2 is slow on purpose.
fn local_hash(data_dir: &Path) -> Result<String> {
    let path = data_dir.join("local.hash");
    if let Ok(hash) = std::fs::read_to_string(&path) {
        if hash.starts_with("$argon2") {
            return Ok(hash.trim().to_string());
        }
    }
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let hash = crate::auth::local::hash_password(&hex::encode(bytes))?;
    std::fs::write(&path, &hash).map_err(internal)?;
    Ok(hash)
}

/// This build's defaults, rendered for one directory.
///
/// The endpoint every role names is port 9 on loopback, where nothing
/// listens: a role with no model on the device then fails at once and in
/// words, instead of waiting out a timeout against a server that was never
/// going to be there. A role that *has* a model never reads its endpoint —
/// see `Core::with_local_models`.
///
/// An endpoint, where the person set one, serves both generating roles: it is
/// one OpenAI-compatible server, and somebody who pointed ask at it expects
/// their captures read by it too. The model on the phone is another matter —
/// it is swapped in for ask alone, in `Core::with_local_models`.
///
/// Two tiers, because the two windows mean different things. The
/// synthesizer's is a planning budget: its instructions alone are some three
/// thousand tokens, and a window too small to plan in fails the capture
/// *before* its passages are embedded — which is how this file first came to
/// have the server's default here. Ask's is memory: it is the KV cache a model
/// on this device allocates, so it is as small as an answer over retrieved
/// passages allows. `plan = false` spares a phone the second completion per
/// question.
fn config_for(data_dir: &Path, local_hash: &str, ask: Option<&Endpoint>) -> String {
    let dir = data_dir.join("bases");
    let control = data_dir.join("control.db");
    let ask_url = ask.map_or("http://127.0.0.1:9/v1", |e| e.base_url.as_str());
    let ask_model = ask.map_or("device", |e| e.model.as_str());
    let ask_key = ask
        .and_then(|e| e.api_key.as_deref())
        .map(|k| format!("api_key = {}\n", quoted(k)))
        .unwrap_or_default();
    format!(
        r#"[server]
bind = "127.0.0.1:0"
workers = 1

[store]
dir = {dir}
control_path = {control}

[vector]
url = "http://127.0.0.1:9"
collection = "artifacts"

[infer.tiers.device-synthesize]
base_url = {ask_url}
model = {ask_model}
{ask_key}context_tokens = 32768
max_output_tokens = 4096

[infer.tiers.device-ask]
base_url = {ask_url}
model = {ask_model}
{ask_key}context_tokens = 8192
max_output_tokens = 1024

[infer.synthesize]
tier = "device-synthesize"

[infer.embed]
base_url = "http://127.0.0.1:9/v1"
model = "embeddinggemma"
dim = 768
max_input_tokens = 2048

[infer.ask]
tier = "device-ask"
plan = false

[auth]
mode = "local"

[auth.local]
username = "phone"
password_hash = {hash}
"#,
        dir = quoted(&dir.to_string_lossy()),
        control = quoted(&control.to_string_lossy()),
        hash = quoted(local_hash),
        ask_url = quoted(ask_url),
        ask_model = quoted(ask_model),
    )
}

pub async fn start(data_dir: &Path, models: LocalModels) -> Result<(Started, Running)> {
    start_with(data_dir, Setup { models, ask: None }).await
}

pub async fn start_with(data_dir: &Path, setup: Setup) -> Result<(Started, Running)> {
    std::fs::create_dir_all(data_dir).map_err(internal)?;
    let config_path = data_dir.join("config.toml");
    // Written on every launch, not only the first: the file is this build's
    // defaults rendered for this directory, not a place anyone edits.
    std::fs::write(
        &config_path,
        config_for(data_dir, &local_hash(data_dir)?, setup.ask.as_ref()),
    )
    .map_err(internal)?;
    let cfg = crate::config::Config::load(Some(&config_path)).map_err(internal)?;

    let control = crate::store::control::Control::connect(&cfg.store.control_path).await?;
    let cfg = Arc::new(cfg);
    let tenants = Arc::new(
        crate::tenants::Tenants::new(
            cfg.clone(),
            control.clone(),
            Arc::new(crate::tenants::SqliteFactory {
                dir: Path::new(&cfg.store.dir).to_path_buf(),
                prefix: cfg.vector.collection.clone(),
                scoring: crate::vector::sqlite::Scoring {
                    recency: crate::vector::Recency {
                        weight: cfg.vector.recency_weight,
                        half_life_days: cfg.vector.recency_half_life_days,
                    },
                    pinned_boost: cfg.vector.pinned_boost,
                },
            }),
        )
        .with_local_models(setup.models),
    );
    // Shut from the first moment: nothing here may generate until the app
    // says the phone can afford it, and never where there is nothing to
    // generate with. See `Running::allow_generation`.
    let generation = tenants.generation();
    generation.store(false, std::sync::atomic::Ordering::Relaxed);
    let can_generate = setup.ask.is_some();
    tenants.get_or_provision(SUBJECT, None).await?;

    for old in control.list_tokens(SUBJECT).await? {
        if old.name == TOKEN_NAME && old.revoked_at.is_none() {
            crate::auth::tokens::revoke(&control, &old.id, SUBJECT).await?;
        }
    }
    let (_, token) = crate::auth::tokens::mint(&control, TOKEN_NAME, SUBJECT, None).await?;

    let state = crate::web::state::AppState {
        tenants: tenants.clone(),
        config: cfg.clone(),
        auth: Arc::new(crate::web::state::AuthContext {
            mode: cfg.auth.mode,
            local: cfg.auth.local.clone(),
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            secure_cookies: false,
        }),
        ask_handoff: Default::default(),
    };

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut workers =
        crate::jobs::Worker::spawn(tenants.clone(), cfg.server.workers, shutdown_rx.clone());
    workers.push(crate::core::background::spawn_repair_ticker(
        tenants.clone(),
        shutdown_rx.clone(),
    ));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(internal)?;
    let port = listener.local_addr().map_err(internal)?.port();
    let mut stop = shutdown_rx;
    let server = tokio::spawn(async move {
        let served = axum::serve(listener, crate::web::router(state))
            .with_graceful_shutdown(async move {
                let _ = stop.wait_for(|s| *s).await;
            })
            .await;
        if let Err(e) = served {
            tracing::error!(error = %e, "the contained listener stopped");
        }
    });
    Ok((
        Started { port, token },
        Running {
            generation,
            can_generate,
            shutdown,
            server,
            workers,
        },
    ))
}

impl Running {
    /// Open or shut the queue's gate on the stages that call a generation
    /// model, and say what it now is. It opens only where an endpoint was
    /// given: the model on the phone answers questions and does not read
    /// captures, so without an endpoint that work has nobody to do it and
    /// stays held however the app asks.
    pub fn allow_generation(&self, allow: bool) -> bool {
        let open = allow && self.can_generate;
        self.generation
            .store(open, std::sync::atomic::Ordering::Relaxed);
        open
    }

    /// Stop listening, then let a job in flight finish: a `running` row left
    /// behind is a job the next launch has to find and repair.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.server.await;
        for w in self.workers {
            let _ = w.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn status(port: u16, token: Option<&str>) -> reqwest::StatusCode {
        let mut req = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/api/v1/status"));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap().status()
    }

    fn parsed(ask: Option<&Endpoint>) -> crate::config::Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, config_for(dir.path(), "$argon2id$x", ask)).unwrap();
        crate::config::Config::load(Some(&path)).unwrap()
    }

    #[test]
    fn with_no_endpoint_ask_points_where_nothing_listens() {
        let cfg = parsed(None);
        assert_eq!(cfg.infer.ask.unwrap().base_url, "http://127.0.0.1:9/v1");
    }

    #[test]
    fn an_endpoint_is_written_as_it_was_typed() {
        let e = Endpoint {
            base_url: "https://llm.example/v1".into(),
            model: "some \"quoted\" model".into(),
            api_key: Some("sk-\\odd".into()),
        };
        let cfg = parsed(Some(&e));
        let ask = cfg.infer.ask.as_ref().unwrap();
        assert_eq!(ask.base_url, "https://llm.example/v1");
        assert_eq!(ask.model, "some \"quoted\" model");
        assert_eq!(ask.api_key.as_deref(), Some("sk-\\odd"));
    }

    #[test]
    fn an_endpoint_serves_synthesis_too() {
        let e = Endpoint {
            base_url: "https://llm.example/v1".into(),
            model: "m".into(),
            api_key: Some("k".into()),
        };
        let cfg = parsed(Some(&e));
        assert_eq!(cfg.infer.synthesize.base_url, "https://llm.example/v1");
        assert_eq!(cfg.infer.synthesize.api_key.as_deref(), Some("k"));
        assert_eq!(cfg.infer.synthesize.context_tokens, 32768);
    }

    async fn status_json(started: &Started) -> serde_json::Value {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/api/v1/status", started.port))
            .bearer_auth(&started.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_no_endpoint_the_gate_never_opens_and_nothing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (started, running) = start(dir.path(), Default::default()).await.unwrap();
        assert!(!running.allow_generation(true));
        let made = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/api/v1/corpora", started.port))
            .bearer_auth(&started.token)
            .json(&serde_json::json!({ "text": "the key hangs behind the kitchen door", "source": "web" }))
            .send()
            .await
            .unwrap();
        assert_eq!(made.status(), 201);
        // The small capture's one synthesis call is armed by the `synthesize`
        // unit. With no embedder here the embed fails, which is not this
        // test's business; the generating unit must simply wait.
        let mut waiting = 0;
        for _ in 0..50 {
            waiting = status_json(&started).await["waiting_generation"]
                .as_i64()
                .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(waiting, 1);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let status = status_json(&started).await;
        assert_eq!(status["waiting_generation"], 1, "it was claimed after all");
        let failed_generating = status["failed"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["stage"] == "segment_window")
            .count();
        assert_eq!(failed_generating, 0);
        running.stop().await;
    }

    async fn dictate(started: &Started, wav: Vec<u8>) -> reqwest::Response {
        let part = reqwest::multipart::Part::bytes(wav)
            .file_name("recording")
            .mime_str("audio/wav")
            .unwrap();
        reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{}/api/v1/transcribe",
                started.port
            ))
            .bearer_auth(&started.token)
            .multipart(reqwest::multipart::Form::new().part("audio", part))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_no_speech_model_the_microphones_door_is_shut() {
        let dir = tempfile::tempdir().unwrap();
        let (started, running) = start(dir.path(), Default::default()).await.unwrap();
        assert_eq!(status_json(&started).await["transcribe"], false);
        assert_eq!(dictate(&started, b"RIFF".to_vec()).await.status(), 404);
        running.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_speech_model_opens_it() {
        let (Some(model), Some(wav)) = (
            crate::infer::whisper::tests::file("speech.bin"),
            crate::infer::whisper::tests::file("speech.wav"),
        ) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let models = LocalModels {
            speech: Some(model),
            ..Default::default()
        };
        let (started, running) = start(dir.path(), models).await.unwrap();
        assert_eq!(status_json(&started).await["transcribe"], true);
        let heard = dictate(&started, std::fs::read(wav).unwrap()).await;
        assert_eq!(heard.status(), 200);
        let words = heard.text().await.unwrap().to_lowercase();
        assert!(words.contains("ask not what your country"), "{words}");
        running.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_an_endpoint_the_app_decides() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Setup {
            models: Default::default(),
            ask: Some(Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "m".into(),
                api_key: None,
            }),
        };
        let (_, running) = start_with(dir.path(), setup).await.unwrap();
        assert!(
            !running
                .generation
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        assert!(running.allow_generation(true));
        assert!(!running.allow_generation(false));
        running.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_answers_its_token_and_nobody_else() {
        let dir = tempfile::tempdir().unwrap();
        let (started, running) = start(dir.path(), Default::default()).await.unwrap();
        assert_ne!(started.port, 0);
        assert_eq!(status(started.port, Some(&started.token)).await, 200);
        assert_eq!(status(started.port, None).await, 401);
        assert_eq!(status(started.port, Some("not-the-token")).await, 401);
        running.stop().await;
        assert!(
            reqwest::Client::new()
                .get(format!("http://127.0.0.1:{}/api/v1/status", started.port))
                .send()
                .await
                .is_err(),
            "still listening after stop"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_launch_keeps_the_base_and_retires_the_first_token() {
        let dir = tempfile::tempdir().unwrap();
        let (first, running) = start(dir.path(), Default::default()).await.unwrap();
        let made = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/api/v1/corpora", first.port))
            .bearer_auth(&first.token)
            .json(&serde_json::json!({ "text": "kept across a restart", "source": "web" }))
            .send()
            .await
            .unwrap();
        assert_eq!(made.status(), 201);
        running.stop().await;

        let (second, running) = start(dir.path(), Default::default()).await.unwrap();
        assert_ne!(first.token, second.token);
        assert_eq!(
            status(second.port, Some(&first.token)).await,
            401,
            "last launch's token still opens the base"
        );
        let list = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/api/v1/corpora", second.port))
            .bearer_auth(&second.token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(list.contains("kept across a restart"), "{list}");
        running.stop().await;
    }
}
