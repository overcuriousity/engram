# Contained mode, part 3: the core as a library the app loads — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One call starts a whole engram on loopback and answers a port and a token; one call stops it. Reachable from Kotlin through a `.so` built for `arm64-v8a`.

**Architecture:** The boot sequence — config, control store, tenant registry, workers, router — moves out of `main.rs`'s shape into `engram::contained::start`, behind the `contained` feature, where a desktop test drives it over real HTTP. `android/native` is a separate `cdylib` crate that depends on engram and exports two JNI functions around it; the main crate stays a plain library and the server build links nothing new. Gradle builds the `.so` with `cargo +stable ndk` when asked to.

**Tech Stack:** axum, tokio, `jni` 0.21, `cargo-ndk` 4.1.2, NDK r28c (`28.2.13676358`), API level 29.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, sections 1 and 6. Established before this plan: `cargo +stable ndk -t arm64-v8a --platform 29 build --lib --features contained` builds the whole crate, llama.cpp and sqlite-vec included, in 2m38s with nothing patched.

## Global Constraints

- The listener binds `127.0.0.1:0` and nothing else. The port is the kernel's choice, reported back.
- One token per launch. Tokens from earlier launches are revoked at start, so a token that leaked from a previous run opens nothing.
- The base is one file: the vectors' `vec_*` tables live in the tenant's own database, `<dir>/<slug>.db`. `SqliteFactory` changes shape to make that true under the tenant registry.
- The single subject is `phone`.
- The main crate's `[lib]` stays as it is. No `cdylib` there.
- The Gradle build must still succeed on a machine with no Rust: the native build runs only with `-Pengram.native=1`. `jniLibs/` is ignored by git.
- Everyday `cargo` on the dev machine is Fedora's; the Android target exists only on the rustup `stable` toolchain. Every Android cargo command is `cargo +stable ndk …` with `ANDROID_NDK_HOME` set.
- Device verification is batched at the end of the programme. This plan verifies on the desktop: the boot over HTTP, the app's own client against it (`LiveServerTest`), the exported symbols of the `.so`, and its presence in the APK.

## File Structure

- Create `src/contained.rs` — `start`, `Running::stop`, the config it writes, its tests.
- Modify `src/lib.rs` — declare it.
- Modify `src/tenants.rs` — `SqliteFactory { dir, prefix, scoring }`; `Tenants::with_local_models`.
- Modify `src/core/mod.rs` — the part 1 and part 2 tests follow the factory's new shape.
- Create `examples/contained.rs` — runs the contained core on the desktop and prints origin and token, for `LiveServerTest`.
- Create `android/native/Cargo.toml`, `android/native/src/lib.rs` — the JNI shim.
- Create `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/contained/Core.kt`.
- Modify `android/core/build.gradle.kts` — the `buildNative` task.
- Modify `.gitignore`.

---

### Task 1: The factory opens the tenant's own file; the registry applies local models

**Interfaces — produces:**
- `SqliteFactory { pub dir: PathBuf, pub prefix: String, pub scoring: Scoring }` — `open(alias, dim)` opens `<dir>/<slug>.db`, where `alias == "<prefix>_<slug>"` exactly as `Tenants::alias` builds it; any other alias is `Error::Vector`.
- `Tenants::with_local_models(self, models: LocalModels) -> Tenants`.

- [ ] **Step 1: Failing test** in `src/tenants.rs`'s test module:

```rust
    #[cfg(feature = "contained")]
    #[tokio::test]
    async fn the_vectors_live_in_the_tenants_own_database_file() {
        use crate::vector::sqlite::Scoring;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::test_default();
        cfg.store.dir = dir.path().to_string_lossy().to_string();
        let prefix = cfg.vector.collection.clone();
        let cfg = Arc::new(cfg);
        let control = Control::memory().await.unwrap();
        let tenants = Tenants::new(
            cfg.clone(),
            control,
            Arc::new(SqliteFactory { dir: dir.path().into(), prefix, scoring: Scoring::off() }),
        );
        let t = tenants.get_or_provision("phone", None).await.unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".db"))
            .collect();
        assert_eq!(files, [format!("{}.db", t.user.slug)], "one base, one file");
        let tables: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE name LIKE 'vec_%' AND type='table'")
            .fetch_all(&t.core.store.pool).await.unwrap();
        assert!(tables.contains(&"vec_points".to_string()), "{tables:?}");
    }

    #[cfg(feature = "contained")]
    #[tokio::test]
    async fn an_alias_the_registry_did_not_build_is_refused() {
        use crate::vector::sqlite::Scoring;
        let f = SqliteFactory { dir: "/tmp".into(), prefix: "artifacts".into(), scoring: Scoring::off() };
        assert!(f.open("something_else", 8).await.is_err());
    }
```

- [ ] **Step 2: Implement.** Replace `SqliteFactory` and its impl:

```rust
/// The contained build's: the vectors go in the tenant's own database.
///
/// `vec_*` tables beside the artifacts, so a base is one file to keep rather
/// than two. The registry names a collection `<prefix>_<slug>` (`alias`) and
/// names the database `<slug>.db` (`db_path`); this undoes the first to arrive
/// at the second, and refuses an alias that is not of that form rather than
/// open a file of its own invention.
#[cfg(feature = "contained")]
pub struct SqliteFactory {
    pub dir: std::path::PathBuf,
    pub prefix: String,
    pub scoring: crate::vector::sqlite::Scoring,
}

#[cfg(feature = "contained")]
#[async_trait::async_trait]
impl VectorFactory for SqliteFactory {
    async fn open(&self, alias: &str, dim: usize) -> Result<Arc<dyn crate::vector::VectorStore>> {
        let slug = alias
            .strip_prefix(self.prefix.as_str())
            .and_then(|rest| rest.strip_prefix('_'))
            .filter(|slug| !slug.is_empty())
            .ok_or_else(|| Error::Vector(format!("`{alias}` is not a collection of `{}`", self.prefix)))?;
        let path = self.dir.join(format!("{slug}.db"));
        let vectors: Arc<dyn crate::vector::VectorStore> =
            Arc::new(crate::vector::sqlite::SqliteVectors::connect(&path, self.scoring).await?);
        vectors.ensure_collection(dim).await?;
        Ok(vectors)
    }
}
```

In `Tenants`: a field `#[cfg(feature = "contained")] local: Option<crate::infer::local::LocalModels>`, `None` in `new`, and

```rust
    /// Every core this registry opens has these roles served in process.
    #[cfg(feature = "contained")]
    pub fn with_local_models(mut self, models: crate::infer::local::LocalModels) -> Tenants {
        self.local = Some(models);
        self
    }
```

and where a core is built (`Core::from_config_with`, near line 345), directly after it:

```rust
        #[cfg(feature = "contained")]
        let core = match &self.local {
            Some(models) => core.with_local_models(&self.cfg, models),
            None => core,
        };
```

Update the two `contained_tests` in `src/core/mod.rs` to the new shape: `dir: tmp.path().into(), prefix: "artifacts".into()` and `factory.open("artifacts_phone", …)`; the reopen in the first test opens the same alias.

- [ ] **Step 3: Run** `cargo test --features contained --lib tenants:: contained_tests` (two invocations; cargo takes one filter). Expected: pass.

- [ ] **Step 4: Commit** `feat(contained): the vectors go in the tenant's own database`

---

### Task 2: `engram::contained::start`

**Interfaces — produces:**

```rust
pub struct Started { pub port: u16, pub token: String }
pub struct Running { /* shutdown, tasks */ }
pub async fn start(data_dir: &Path, models: LocalModels) -> Result<(Started, Running)>;
impl Running { pub async fn stop(self); }
pub const SUBJECT: &str = "phone";
```

- [ ] **Step 1: Failing tests** at the bottom of the new `src/contained.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    async fn status(port: u16, token: Option<&str>) -> reqwest::StatusCode {
        let mut req = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/api/v1/status"));
        if let Some(t) = token { req = req.bearer_auth(t); }
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
        assert!(reqwest::Client::new().get(format!("http://127.0.0.1:{}/api/v1/status", started.port)).send().await.is_err(), "still listening after stop");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_launch_keeps_the_base_and_retires_the_first_token() {
        let dir = tempfile::tempdir().unwrap();
        let (first, running) = start(dir.path(), Default::default()).await.unwrap();
        let made = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/api/v1/corpora", first.port))
            .bearer_auth(&first.token)
            .json(&serde_json::json!({ "text": "kept across a restart", "source": "web" }))
            .send().await.unwrap();
        assert_eq!(made.status(), 201);
        running.stop().await;

        let (second, running) = start(dir.path(), Default::default()).await.unwrap();
        assert_ne!(first.token, second.token);
        assert_eq!(status(second.port, Some(&first.token)).await, 401, "last launch's token still opens the base");
        let list = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/api/v1/corpora", second.port))
            .bearer_auth(&second.token).send().await.unwrap().text().await.unwrap();
        assert!(list.contains("kept across a restart"), "{list}");
        running.stop().await;
    }
}
```

- [ ] **Step 2: Implement.** `src/lib.rs`: `#[cfg(feature = "contained")] pub mod contained;`. Then `src/contained.rs`:

```rust
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

pub async fn start(data_dir: &Path, models: LocalModels) -> Result<(Started, Running)> {
    std::fs::create_dir_all(data_dir).map_err(internal)?;
    let config_path = data_dir.join("config.toml");
    // Written on every launch, not only the first: the file is this build's
    // defaults rendered for this directory, not a place anyone edits.
    std::fs::write(&config_path, config_for(data_dir)).map_err(internal)?;
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
    let mut workers = crate::jobs::Worker::spawn(tenants.clone(), cfg.server.workers, shutdown_rx.clone());
    workers.push(crate::core::background::spawn_repair_ticker(tenants.clone(), shutdown_rx.clone()));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(internal)?;
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
    Ok((Started { port, token }, Running { shutdown, server, workers }))
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
```

`config_for(data_dir) -> String` renders a TOML config: `[server] bind = "127.0.0.1:0"`, `workers = 1`; `[store]` with `dir` and `control_path` under `data_dir`; `[auth] mode = "local"`; `[vector]` with the collection `artifacts` and an unused URL; `[infer.embed]` with `model = "embeddinggemma"`, `dim = 768`, `max_input_tokens = 2048` and the three default templates by omission; `[infer.synthesize]` and whatever else `Config::validate` requires, pointed at `http://127.0.0.1:9/v1` — a port nothing listens on — so that a role with no model fails fast and in words instead of hanging. Write it by starting from the smallest plausible file and running the first test: `Config::load` names each missing or refused key in its error, one run at a time, until it loads. Paths go through `toml::Value::String` (or are escaped for a TOML basic string) — a data directory is not guaranteed free of quotes and backslashes. Record in a comment on `config_for` which keys validation demanded and why each has the value it has.

If `AuthMode::Local` demands `[auth.local]` credentials, give it a username `phone` and a password hash of 32 random bytes generated at first launch and stored in the file: nothing ever logs in with it, and a fixed one would be a password anyone could read in this repository. If `validate_auth`-like loopback checks live only in `main.rs`, nothing is needed here: the bind is loopback by construction.

If `control.list_tokens`, `ApiToken::revoked_at` or `Worker::spawn`'s signature differ from the above, follow the code; `src/main.rs:433-482` is the reference for the state and the workers.

- [ ] **Step 3: Run** `cargo test --features contained --lib contained::` — Expected: 2 passed.

- [ ] **Step 4: Commit** `feat(contained): a whole engram behind one call`

---

### Task 3: The desktop runner, and the app's own client against it

- [ ] **Step 1:** Create `examples/contained.rs`:

```rust
//! The contained core, run on a desktop: what the phone runs, reachable by
//! anything that wants to talk to it. Prints the origin and the token and
//! serves until interrupted.
//!
//!     cargo run --features contained --example contained -- /tmp/engram-contained [models-dir]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("a data directory"));
    let models = args.next().map(std::path::PathBuf::from);
    let pick = |name: &str| models.as_ref().map(|d| d.join(name)).filter(|p| p.is_file());
    let models = engram::infer::local::LocalModels {
        embed: pick("embed.gguf"),
        rerank: pick("rerank.gguf"),
        ask: pick("ask.gguf"),
    };
    let (started, running) = engram::contained::start(&dir, models).await?;
    println!("origin=http://127.0.0.1:{}", started.port);
    println!("token={}", started.token);
    tokio::signal::ctrl_c().await?;
    running.stop().await;
    Ok(())
}
```

and in `Cargo.toml`:

```toml
[[example]]
name = "contained"
required-features = ["contained"]
```

- [ ] **Step 2: Run it and point `LiveServerTest` at it.**

```bash
cargo run --features contained --example contained -- /tmp/engram-live ~/.cache/engram-test-models > /tmp/engram-live.out &
until grep -q '^token=' /tmp/engram-live.out; do sleep 0.5; done
cd android && ./gradlew :core:testDebugUnitTest --tests '*LiveServerTest*' \
  -Pengram.live.origin=$(grep '^origin=' /tmp/engram-live.out | cut -d= -f2) \
  -Pengram.live.token=$(grep '^token=' /tmp/engram-live.out | cut -d= -f2)
```

Expected: `LiveServerTest` passes — the app's real `Transport`, `ServerReader` and models against the contained core. A failure here is a finding about the contained core, not about the test: read which call failed, and fix the core or record why that call has no contained answer (pairing and push are the expected ones; see the spec, section 4).

- [ ] **Step 3: Commit** `feat(contained): a desktop runner, and the app's client passes against it`, with the test's result in the message.

---

### Task 4: The JNI shim and the `.so`

**Interfaces — produces:** `libengram_android.so` exporting `Java_io_github_overcuriousity_engram_core_contained_Core_start` and `…_Core_stop`.

- [ ] **Step 1:** `android/native/Cargo.toml`:

```toml
[package]
name = "engram-android"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
engram = { path = "../..", features = ["contained"] }
# Named again only to choose how the C++ runtime is linked on Android: into
# the library, so the APK ships one `.so` and no `libc++_shared.so` beside it.
llama-cpp-2 = { version = "=0.1.156", default-features = false, features = ["android-static-stdcxx"] }
jni = "0.21"
tokio = { version = "1", features = ["rt-multi-thread"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
opt-level = 3
lto = "thin"
strip = true
panic = "unwind"

[workspace]
```

The empty `[workspace]` keeps cargo from looking for one above it.

- [ ] **Step 2:** `android/native/src/lib.rs`:

```rust
//! The two functions the app calls. Everything else is `engram::contained`.
//!
//! Both answer JSON in a string, errors included: an exception thrown across
//! JNI from Rust is one more thing to get wrong, and the Kotlin side decodes
//! JSON already.

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use std::sync::Mutex;

struct Live {
    runtime: tokio::runtime::Runtime,
    running: engram::contained::Running,
    started: serde_json::Value,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

#[derive(serde::Deserialize, Default)]
struct Models {
    embed: Option<std::path::PathBuf>,
    rerank: Option<std::path::PathBuf>,
    ask: Option<std::path::PathBuf>,
}

fn start(data_dir: String, models: String) -> Result<serde_json::Value, String> {
    let mut live = LIVE.lock().map_err(|_| "a previous start panicked".to_string())?;
    // Started twice is answered with what is already running: an Activity
    // recreated does not get a second engram over the same files.
    if let Some(l) = live.as_ref() {
        return Ok(l.started.clone());
    }
    let m: Models = serde_json::from_str(&models).map_err(|e| format!("models: {e}"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("engram-core")
        .build()
        .map_err(|e| e.to_string())?;
    let (started, running) = runtime
        .block_on(engram::contained::start(
            std::path::Path::new(&data_dir),
            engram::infer::local::LocalModels { embed: m.embed, rerank: m.rerank, ask: m.ask },
        ))
        .map_err(|e| e.to_string())?;
    let started = serde_json::json!({ "port": started.port, "token": started.token });
    *live = Some(Live { runtime, running, started: started.clone() });
    Ok(started)
}

fn answer(env: &mut JNIEnv, value: Result<serde_json::Value, String>) -> jstring {
    let body = match value {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "error": e }),
    };
    env.new_string(body.to_string()).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_start<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    data_dir: JString<'l>,
    models: JString<'l>,
) -> jstring {
    let read = |env: &mut JNIEnv<'l>, s: &JString<'l>| env.get_string(s).map(String::from).map_err(|e| e.to_string());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dir = read(&mut env, &data_dir)?;
        let models = read(&mut env, &models)?;
        start(dir, models)
    }))
    .unwrap_or_else(|_| Err("the core panicked while starting".into()));
    answer(&mut env, outcome)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_stop<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    let outcome = match LIVE.lock() {
        Ok(mut live) => {
            if let Some(l) = live.take() {
                l.runtime.block_on(l.running.stop());
            }
            Ok(serde_json::json!({ "stopped": true }))
        }
        Err(_) => Err("a previous call panicked".to_string()),
    };
    answer(&mut env, outcome)
}
```

- [ ] **Step 3: Build and inspect.**

```bash
export PATH=$HOME/.cargo/bin:$PATH ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.2.13676358
cd android/native && cargo +stable ndk -t arm64-v8a --platform 29 -o ../core/src/main/jniLibs build --release
ls -la ../core/src/main/jniLibs/arm64-v8a/
$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-nm -D --defined-only ../core/src/main/jniLibs/arm64-v8a/libengram_android.so | grep Java_
$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf -d ../core/src/main/jniLibs/arm64-v8a/libengram_android.so | grep NEEDED
$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf -l ../core/src/main/jniLibs/arm64-v8a/libengram_android.so | grep -A1 LOAD | head
```

Expected: one `libengram_android.so`; both `Java_…` symbols; `NEEDED` lists only system libraries (`libc.so`, `libm.so`, `libdl.so`, `liblog.so`) and no `libc++_shared.so`; every `LOAD` segment aligned `0x4000` (16 KB pages — NDK r28's default). Record the file's size: it is the number the spec's "is carving out OIDC, MCP and the templates worth it" question is decided against.

- [ ] **Step 4: Commit** `feat(android): the core as a library, two functions wide`

---

### Task 5: Kotlin's side of the two functions, and Gradle's

- [ ] **Step 1:** `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/contained/Core.kt`:

```kotlin
package io.github.overcuriousity.engram.core.contained

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/** Where a started core listens, and the word that opens it for this launch. */
@Serializable
data class Started(val port: Int, val token: String)

/** The model file for each role that runs on this device. Absent: that role does not. */
@Serializable
data class Models(val embed: String? = null, val rerank: String? = null, val ask: String? = null)

class CoreFailed(message: String) : Exception(message)

/**
 * The engram that lives in this process. Two calls wide on purpose: everything
 * it can do is reached over the same HTTP API the server speaks, so nothing
 * above `core` has a second way to ask.
 */
object Core {
    private val json = Json { ignoreUnknownKeys = true }

    @Serializable
    private data class Answer(val port: Int? = null, val token: String? = null, val error: String? = null)

    init {
        System.loadLibrary("engram_android")
    }

    /** Starts the core over [dataDir], or answers the one already running. Blocks; call off the main thread. */
    fun start(dataDir: String, models: Models): Started {
        val a = json.decodeFromString<Answer>(start(dataDir, json.encodeToString(Models.serializer(), models)))
        if (a.error != null || a.port == null || a.token == null) throw CoreFailed(a.error ?: "the core answered nothing")
        return Started(a.port, a.token)
    }

    /** Stops it and waits for a job in flight. Safe to call when nothing runs. */
    fun shutdown() {
        stop()
    }

    @JvmStatic private external fun start(dataDir: String, models: String): String

    @JvmStatic private external fun stop(): String
}
```

The JNI names in Task 4 are for static methods of `…core.contained.Core`; `@JvmStatic` on an `object`'s `external fun` is what makes them static.

- [ ] **Step 2:** In `android/core/build.gradle.kts`, after the `android { }` block:

```kotlin
// The Rust core, built for the phone. Only with `-Pengram.native=1`: a
// checkout with no Rust toolchain must still build the app, and then runs in
// server mode only. `cargo +stable` because the Android target lives on the
// rustup toolchain, whatever the machine's everyday cargo is.
val buildNative by tasks.registering(Exec::class) {
    val ndk = providers.environmentVariable("ANDROID_NDK_HOME")
        .orElse(android.sdkDirectory.resolve("ndk/28.2.13676358").absolutePath)
    workingDir = rootProject.file("native")
    environment("ANDROID_NDK_HOME", ndk.get())
    commandLine(
        "cargo", "+stable", "ndk", "-t", "arm64-v8a", "--platform", "29",
        "-o", file("src/main/jniLibs").absolutePath, "build", "--release",
    )
}
if (providers.gradleProperty("engram.native").isPresent) {
    tasks.named("preBuild") { dependsOn(buildNative) }
}
```

and in `.gitignore`: `android/core/src/main/jniLibs/` and `android/native/target/`.

- [ ] **Step 3: Verify.**

```bash
cd android && ./gradlew :app:assembleDebug -Pengram.native=1
unzip -l app/build/outputs/apk/debug/app-debug.apk | grep libengram_android
./gradlew :core:lintDebug :app:lintDebug
```

Expected: the APK holds `lib/arm64-v8a/libengram_android.so`; lint passes. Then, without the property and with `jniLibs` removed, `./gradlew :app:assembleDebug` still succeeds.

- [ ] **Step 4: Commit** `feat(android): Kotlin's two calls into the core, and the build that makes it`

## Left for later parts, on purpose

- Nothing calls `Core.start` yet. Part 4 is the mode, the loopback `Connection` and the choice in `Engram.kt`.
- Outgoing HTTPS from the core on Android. `rustls-platform-verifier` is in the tree and wants initialising with an Android `Context` before a certificate verifies; until then a captured URL's fetch and an ask endpoint fail on the phone, in words. It gets its own task in the part that first needs it, and a line in the device pass.
- The release workflow's native build: with part 5, when there is a contained mode to ship.
