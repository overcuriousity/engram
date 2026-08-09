//! Freeze the evaluation corpus: segment every `corpus/*.txt` once with the
//! real chunker and write the result to `chunks.json`.
//!
//! Run deliberately, not per benchmark. Segmenting on every run would cost a
//! completion per window and return slightly different chunks each time, so a
//! two percent ranking change would be indistinguishable from segmenter noise.
//!
//! Chunk ids change on every run, so re-running invalidates `pairs.json` and
//! the pairs have to be re-checked against the new ids. That is the reason
//! this is a separate command rather than a step of the harness.
//!
//!   ENGRAM_EVAL_DIR=~/engram-eval cargo run --bin eval-prepare

use anyhow::{Context, Result, bail};
use engram::config::Config;
use engram::core::Core;
use engram::eval::{FrozenChunk, eval_dir, save_chunks};
use engram::store::Store;
use engram::vector::memory::MemoryVectors;
use std::sync::Arc;

/// How many times the whole source is driven before its remaining windows are
/// called hopeless. `segment::run` resumes from the first pending window, so a
/// pass is one attempt at each window still outstanding — this stands in for
/// the job runner's own attempt budget.
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

    let mut frozen: Vec<FrozenChunk> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

        tracing::info!(file = %name, bytes = text.len(), "segmenting");
        let out = core.ingest(&text, "eval", Some(&name)).await?;

        // Stand in for the job runner: `segment::run` resumes from the first
        // pending window, so repeating it is what drives a multi-window source
        // to completion.
        let mut passes = 0;
        loop {
            if let Err(e) = engram::jobs::segment::run(&core, &out.id).await {
                tracing::warn!(error = %e, file = %name, "segmentation pass failed");
            }
            passes += 1;
            let pending = core.store.pending_windows(&out.id).await?;
            if pending.is_empty() {
                break;
            }
            if passes >= MAX_PASSES {
                bail!(
                    "{name}: {} window(s) still unsegmented after {MAX_PASSES} passes. \
                     The corpus has to be segmented in full, or the benchmark ranks a \
                     document with holes in it and the numbers mean something different \
                     from one run to the next.",
                    pending.len()
                );
            }
        }

        let chunks = core.store.chunks_for_source(&out.id).await?;
        tracing::info!(file = %name, chunks = chunks.len(), "segmented");
        for c in chunks {
            frozen.push(FrozenChunk {
                id: c.id,
                source: name.clone(),
                text: c.text,
                title: c.title,
                category: c.category,
                tags: c.tags,
            });
        }
    }

    save_chunks(&dir, &frozen)?;
    println!(
        "froze {} chunks from {} documents into {}",
        frozen.len(),
        files.len(),
        dir.join("chunks.json").display()
    );
    Ok(())
}
