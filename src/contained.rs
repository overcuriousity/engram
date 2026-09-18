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

pub struct Running {
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
/// Two tiers, because the two windows mean different things. The
/// synthesizer's is a planning budget: its instructions alone are some three
/// thousand tokens, and a window too small to plan in fails the capture
/// *before* its passages are embedded — which is how this file first came to
/// have the server's default here. Ask's is memory: it is the KV cache a model
/// on this device allocates, so it is as small as an answer over retrieved
/// passages allows. `plan = false` spares a phone the second completion per
/// question.
fn config_for(data_dir: &Path, local_hash: &str) -> String {
    let dir = data_dir.join("bases");
    let control = data_dir.join("control.db");
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
base_url = "http://127.0.0.1:9/v1"
model = "device"
context_tokens = 32768
max_output_tokens = 4096

[infer.tiers.device-ask]
base_url = "http://127.0.0.1:9/v1"
model = "device"
context_tokens = 8192
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
    )
}

pub async fn start(data_dir: &Path, models: LocalModels) -> Result<(Started, Running)> {
    std::fs::create_dir_all(data_dir).map_err(internal)?;
    let config_path = data_dir.join("config.toml");
    // Written on every launch, not only the first: the file is this build's
    // defaults rendered for this directory, not a place anyone edits.
    std::fs::write(&config_path, config_for(data_dir, &local_hash(data_dir)?)).map_err(internal)?;
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
        .with_local_models(models),
    );
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
            shutdown,
            server,
            workers,
        },
    ))
}

impl Running {
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
