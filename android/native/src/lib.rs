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
    let mut live = LIVE
        .lock()
        .map_err(|_| "a previous start panicked".to_string())?;
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
            engram::infer::local::LocalModels {
                embed: m.embed,
                rerank: m.rerank,
                ask: m.ask,
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

fn answer(env: &mut JNIEnv, value: Result<serde_json::Value, String>) -> jstring {
    let body = match value {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "error": e }),
    };
    env.new_string(body.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_start<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    data_dir: JString<'l>,
    models: JString<'l>,
) -> jstring {
    let read = |env: &mut JNIEnv<'l>, s: &JString<'l>| {
        env.get_string(s)
            .map(String::from)
            .map_err(|e| e.to_string())
    };
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
