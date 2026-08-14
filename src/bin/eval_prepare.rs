//! Freeze the evaluation corpus: synthesise every `corpus/*.txt` once with the
//! real model and write the result to `artifacts.json`.
//!
//! Run deliberately, not per benchmark. Synthesising on every run would cost a
//! completion per segment and return slightly different artifacts each time, so
//! a two percent ranking change would be indistinguishable from model noise.
//!
//! Chunk ids change on every run, so re-running invalidates `pairs.json` and
//! the pairs have to be re-checked against the new ids. That is the reason
//! this is a separate command rather than a step of the harness.
//!
//!   ENGRAM_EVAL_DIR=~/engram-eval cargo run --bin eval-prepare

use anyhow::{Context, Result, bail};
use engram::config::Config;
use engram::core::Core;
use engram::eval::{FrozenArtifact, eval_dir, save_artifacts};
use engram::store::Store;
use engram::vector::memory::MemoryVectors;
use std::sync::Arc;

/// How many times the whole corpus is driven before its remaining segments are
/// called hopeless. A pass drains every unit the queue will hand over, so this
/// stands in for the job runner's own attempt budget.
const MAX_PASSES: usize = 4;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "eval_prepare=info,engram=info".into()),
        )
        .init();

    let dir = eval_dir();
    let corpus = dir.join("corpus");
    if !corpus.is_dir() {
        bail!(
            "no corpus at {}. Put the extracted .txt files there, or set ENGRAM_EVAL_DIR.",
            corpus.display()
        );
    }

    let cfg = Config::load(None).context("loading config.toml")?;
    // A throwaway in-memory store and vector index: this run produces a JSON
    // file, not a searchable instance.
    let store = Store::memory().await?;
    let core = Core::from_config(&cfg, Arc::new(MemoryVectors::new()), store);

    let mut files: Vec<_> = std::fs::read_dir(&corpus)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    // Sorted so the frozen file is stable between runs on different machines.
    files.sort();
    if files.is_empty() {
        bail!("no .txt files in {}", corpus.display());
    }

    let mut frozen: Vec<FrozenArtifact> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

        tracing::info!(file = %name, bytes = text.len(), "synthesising");
        let out = core.ingest(&text, "eval", Some(&name)).await?;

        // Be the job runner. Planning arms one unit per window and the queue
        // hands them over one at a time, so a pass here is "drain what is ready"
        // rather than "one attempt at each outstanding segment". The backoff is
        // wound back between passes: this is a batch tool with a whole corpus to
        // get through, not a service pacing itself against a live endpoint.
        engram::jobs::synthesize::plan(&core, &out.id).await?;
        let mut passes = 0;
        loop {
            while engram::jobs::run_one(&core).await.unwrap_or(false) {}
            passes += 1;
            let pending = core.store.pending_segments(&out.id).await?;
            if pending.is_empty() {
                break;
            }
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
                .await?;
            if passes >= MAX_PASSES {
                bail!(
                    "{name}: {} segment(s) still unsynthesised after {MAX_PASSES} passes. \
                     The corpus has to be synthesised in full, or the benchmark ranks a \
                     document with holes in it and the numbers mean something different \
                     from one run to the next.",
                    pending.len()
                );
            }
        }

        let artifacts = core.store.artifacts_for_corpus(&out.id).await?;
        tracing::info!(file = %name, artifacts = artifacts.len(), "synthesised");
        for c in artifacts {
            frozen.push(FrozenArtifact {
                id: c.id,
                source: name.clone(),
                text: c.text,
                title: c.title,
                category: c.category,
                tags: c.tags,
            });
        }
    }

    save_artifacts(&dir, &frozen)?;
    println!(
        "froze {} artifacts from {} documents into {}",
        frozen.len(),
        files.len(),
        dir.join("artifacts.json").display()
    );
    Ok(())
}
