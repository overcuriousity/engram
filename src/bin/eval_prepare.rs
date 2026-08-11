use anyhow::{Context, Result, bail};
use engram::config::Config;
use engram::core::Core;
use engram::eval::{FrozenArtifact, eval_dir, save_artifacts};
use engram::store::Store;
use engram::vector::memory::MemoryVectors;
use std::sync::Arc;

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
    let store = Store::memory().await?;
    let core = Core::from_config(&cfg, Arc::new(MemoryVectors::new()), store);

    let mut files: Vec<_> = std::fs::read_dir(&corpus)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
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

        let mut passes = 0;
        loop {
            if let Err(e) = engram::jobs::synthesize::run(&core, &out.id).await {
                tracing::warn!(error = %e, file = %name, "synthesis pass failed");
            }
            passes += 1;
            let pending = core.store.pending_segments(&out.id).await?;
            if pending.is_empty() {
                break;
            }
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
