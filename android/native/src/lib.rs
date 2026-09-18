//! The four functions the app calls. Everything else is `engram::contained`.
//!
//! Both answer JSON in a string, errors included: an exception thrown across
//! JNI from Rust is one more thing to get wrong, and the Kotlin side decodes
//! JSON already.

use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JObject, JString};
use std::sync::Mutex;

struct Live {
    runtime: tokio::runtime::Runtime,
    running: engram::contained::Running,
    started: serde_json::Value,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Setup {
    embed: Option<std::path::PathBuf>,
    rerank: Option<std::path::PathBuf>,
    ask: Option<std::path::PathBuf>,
    ask_endpoint: Option<Endpoint>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    base_url: String,
    model: String,
    api_key: Option<String>,
}

fn start(data_dir: String, setup: String) -> Result<serde_json::Value, String> {
    let mut live = LIVE
        .lock()
        .map_err(|_| "a previous start panicked".to_string())?;
    // Started twice is answered with what is already running: an Activity
    // recreated does not get a second engram over the same files.
    if let Some(l) = live.as_ref() {
        return Ok(l.started.clone());
    }
    let s: Setup = serde_json::from_str(&setup).map_err(|e| format!("setup: {e}"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("engram-core")
        .build()
        .map_err(|e| e.to_string())?;
    let (started, running) = runtime
        .block_on(engram::contained::start_with(
            std::path::Path::new(&data_dir),
            engram::contained::Setup {
                models: engram::infer::local::LocalModels {
                    embed: s.embed,
                    rerank: s.rerank,
                    ask: s.ask,
                },
                ask: s.ask_endpoint.map(|e| engram::contained::Endpoint {
                    base_url: e.base_url,
                    model: e.model,
                    api_key: e.api_key,
                }),
            },
        ))
        .map_err(|e| e.to_string())?;
    let started = serde_json::json!({ "port": started.port, "token": started.token });
    *live = Some(Live {
        runtime,
        running,
        started: started.clone(),
    });
    Ok(started)
}

fn stop() -> Result<serde_json::Value, String> {
    let mut live = LIVE
        .lock()
        .map_err(|_| "a previous call panicked".to_string())?;
    if let Some(l) = live.take() {
        l.runtime.block_on(l.running.stop());
    }
    Ok(serde_json::json!({ "stopped": true }))
}

fn background(allow: bool) -> Result<serde_json::Value, String> {
    let live = LIVE
        .lock()
        .map_err(|_| "a previous call panicked".to_string())?;
    let l = live.as_ref().ok_or("nothing is running")?;
    Ok(serde_json::json!({ "open": l.running.allow_generation(allow) }))
}

fn body(value: Result<serde_json::Value, String>) -> String {
    match value {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "error": e }),
    }
    .to_string()
}

/// Hands the platform's certificate verifier the app's Context. Without it
/// the first HTTPS request the core makes on Android panics, because rustls
/// asks Android whether a chain is trusted and has nobody to ask through.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_init<'l>(
    mut env: EnvUnowned<'l>,
    _class: JClass<'l>,
    context: JObject<'l>,
) -> JString<'l> {
    env.with_env(|env| -> Result<_, jni::errors::Error> {
        let outcome = rustls_platform_verifier::android::init_with_env(env, context)
            .map(|()| serde_json::json!({ "ready": true }))
            .map_err(|e| e.to_string());
        JString::from_str(env, body(outcome))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_start<'l>(
    mut env: EnvUnowned<'l>,
    _class: JClass<'l>,
    data_dir: JString<'l>,
    setup: JString<'l>,
) -> JString<'l> {
    env.with_env(|env| -> Result<_, jni::errors::Error> {
        let (dir, setup) = (data_dir.to_string(), setup.to_string());
        // A panic in the core is an answer in words, not a RuntimeException
        // with a Rust backtrace for a message.
        let outcome = std::panic::catch_unwind(|| start(dir, setup))
            .unwrap_or_else(|_| Err("the core panicked while starting".into()));
        JString::from_str(env, body(outcome))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_stop<'l>(
    mut env: EnvUnowned<'l>,
    _class: JClass<'l>,
) -> JString<'l> {
    env.with_env(|env| -> Result<_, jni::errors::Error> { JString::from_str(env, body(stop())) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Whether model work may run now. The app knows what the core cannot: that
/// the phone is charging, idle and cool. Answers what the gate now is, which
/// is shut however it was asked where there is no endpoint to do the work.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_background<'l>(
    mut env: EnvUnowned<'l>,
    _class: JClass<'l>,
    allow: jni::sys::jboolean,
) -> JString<'l> {
    env.with_env(|env| -> Result<_, jni::errors::Error> {
        JString::from_str(env, body(background(allow)))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}
