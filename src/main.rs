use clap::Parser;
use engram::config::{AuthMode, Config};
use engram::core::Core;
use engram::error::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "engram")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print the effective config with secrets redacted, then exit.
    #[arg(long)]
    print_config: bool,
    /// Permit auth.mode = "local" on a non-loopback bind address.
    #[arg(long)]
    i_know_this_is_insecure: bool,
    /// Print an argon2id hash for a password, for auth.local.password_hash.
    #[arg(long)]
    hash_password: Option<String>,
    /// Copy every vector into a fresh collection generation and swap the alias
    /// onto it, then exit. Costs no embedding calls and leaves the previous
    /// generation in place.
    #[arg(long)]
    reindex: bool,
    /// With --reindex, permit deleting a pre-alias collection once its points
    /// have been copied and counted. Needed only for a collection created
    /// before engram addressed vectors through an alias.
    #[arg(long)]
    replace_legacy: bool,
    /// Re-measure every corpus's coverage from the artifacts already stored,
    /// then exit. Local work over existing rows: no inference, no vector calls,
    /// nothing re-synthesised. Run it after upgrading past a change to how
    /// coverage is measured, since the figure is otherwise written once.
    #[arg(long)]
    recompute_coverage: bool,
    /// Push every artifact's SQLite-side lifecycle state (status,
    /// last_verified_at, superseded_by) into Qdrant, then exit. Run once after
    /// deploying the lifecycle migration: existing points have none of these
    /// fields until this runs, which every filter treats as active in the
    /// meantime, just not yet filterable as deprecated.
    #[arg(long)]
    backfill_lifecycle: bool,
}

fn validate_auth(cfg: &Config, insecure_ok: bool) -> Result<()> {
    match cfg.auth.mode {
        AuthMode::Oidc => {
            if cfg.auth.oidc.is_none() {
                return Err(Error::Validation(
                    "auth.mode = \"oidc\" but no [auth.oidc] block".into(),
                ));
            }
        }
        AuthMode::Local => {
            if cfg.auth.local.is_none() {
                return Err(Error::Validation(
                    "auth.mode = \"local\" but no [auth.local] block".into(),
                ));
            }
            engram::auth::local::assert_bind_is_safe(&cfg.server.bind, insecure_ok)?;
        }
    }
    Ok(())
}

/// Fail fast on anything that would otherwise surface much later as bad search
/// results. Inference probes are warnings only: ingest is designed to work
/// while the endpoints are down.
async fn startup_checks(core: &Core, cfg: &Config) -> Result<()> {
    core.vectors.ensure_collection(cfg.infer.embed.dim).await?;

    let reclaimed = core
        .store
        .reclaim_stuck(engram::jobs::STUCK_AFTER_SECS)
        .await?;
    if reclaimed > 0 {
        tracing::info!(
            reclaimed,
            "requeued jobs left running by a previous process"
        );
    }
    let purged = core.store.purge_expired_sessions().await?;
    if purged > 0 {
        tracing::info!(purged, "removed expired sessions");
    }

    engram::infer::openai::probe(
        "chunk",
        &cfg.infer.synthesize.base_url,
        cfg.infer.synthesize.api_key.as_deref(),
    )
    .await;
    engram::infer::openai::probe(
        "embed",
        &cfg.infer.embed.base_url,
        cfg.infer.embed.api_key.as_deref(),
    )
    .await;
    if let Some(r) = &cfg.infer.rerank {
        engram::infer::openai::probe("rerank", &r.base_url, r.api_key.as_deref()).await;
    } else {
        tracing::info!("rerank not configured; search returns vector order");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "engram=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    if let Some(pw) = &args.hash_password {
        println!("{}", engram::auth::local::hash_password(pw)?);
        return Ok(());
    }

    let cfg = Config::load(args.config.as_deref())?;
    if args.print_config {
        println!("{}", cfg.redacted());
        return Ok(());
    }

    if args.reindex {
        let vectors = engram::vector::qdrant::QdrantVectors::connect(&cfg.vector).await?;
        let target = vectors
            .reindex(cfg.infer.embed.dim, args.replace_legacy)
            .await?;
        println!("{} now serves `{}`", cfg.vector.collection, target);
        return Ok(());
    }

    if args.backfill_lifecycle {
        let store = engram::store::Store::connect(&cfg.store).await?;
        let vectors: Arc<dyn engram::vector::VectorStore> =
            Arc::new(engram::vector::qdrant::QdrantVectors::connect(&cfg.vector).await?);
        let core = Core::from_config(&cfg, vectors, store);
        let n = core.backfill_lifecycle().await?;
        println!("backfilled lifecycle fields for {n} artifacts");
        return Ok(());
    }

    if args.recompute_coverage {
        // No vector store and no inference: this only reads artifacts and
        // writes one number per corpus, so it must not need either to be up.
        let store = engram::store::Store::connect(&cfg.store).await?;
        let core = Core::from_config(
            &cfg,
            Arc::new(engram::vector::memory::MemoryVectors::new()),
            store,
        );
        let mut offset = 0;
        loop {
            let page = core.store.list_corpora(100, offset).await?;
            if page.is_empty() {
                break;
            }
            for c in &page {
                let before = c.coverage;
                let after = engram::jobs::synthesize::recompute_coverage(&core, &c.id).await?;
                println!(
                    "{}  {} -> {:.3}",
                    c.id,
                    before.map_or("none".to_string(), |b| format!("{b:.3}")),
                    after
                );
            }
            offset += page.len() as i64;
        }
        return Ok(());
    }

    validate_auth(&cfg, args.i_know_this_is_insecure)?;

    let store = engram::store::Store::connect(&cfg.store).await?;
    let vectors: Arc<dyn engram::vector::VectorStore> =
        Arc::new(engram::vector::qdrant::QdrantVectors::connect(&cfg.vector).await?);
    let core = Core::from_config(&cfg, vectors, store);
    startup_checks(&core, &cfg).await?;

    let oidc = match cfg.auth.mode {
        AuthMode::Oidc => {
            Some(engram::auth::oidc::OidcClient::discover(cfg.auth.oidc.as_ref().unwrap()).await?)
        }
        AuthMode::Local => None,
    };
    let secure_cookies = cfg
        .auth
        .oidc
        .as_ref()
        .map(|o| o.redirect_url.starts_with("https://"))
        .unwrap_or(false);

    let state = engram::web::state::AppState {
        core: core.clone(),
        auth: Arc::new(engram::web::state::AuthContext {
            mode: cfg.auth.mode,
            local: cfg.auth.local.clone(),
            oidc,
            pending: engram::auth::oidc::PendingStore::new(),
            secure_cookies,
        }),
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let background = core.background.clone();
    let ticker =
        engram::core::background::spawn_consolidation_ticker(core.clone(), shutdown_rx.clone());
    let mut handles = engram::jobs::Worker::spawn(core, cfg.server.workers, shutdown_rx);
    // Joined with the workers so shutdown waits for it too, rather than leaving
    // a task the runtime drops mid-enqueue.
    handles.push(ticker);

    let listener = tokio::net::TcpListener::bind(&cfg.server.bind).await?;
    tracing::info!(bind = %cfg.server.bind, mode = ?cfg.auth.mode, "engram listening");

    axum::serve(listener, engram::web::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;

    // Let in-flight jobs finish rather than orphaning `running` rows.
    let _ = shutdown_tx.send(true);
    for h in handles {
        let _ = h.await;
    }
    // The last searches served each left a write behind. The listener is
    // already closed, so this drains a bounded set rather than chasing new
    // work; bounded further in case one of them is stuck on a wedged Qdrant.
    if background.inflight() > 0 {
        tracing::info!(
            inflight = background.inflight(),
            "draining background writes"
        );
        if tokio::time::timeout(std::time::Duration::from_secs(10), background.wait_idle())
            .await
            .is_err()
        {
            tracing::warn!(
                inflight = background.inflight(),
                "gave up waiting for background writes"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use engram::config::*;

    fn test_config() -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:8080".into(),
                workers: 2,
            },
            store: StoreConfig {
                path: "engram.db".into(),
            },
            vector: VectorConfig {
                url: "http://localhost:6333".into(),
                collection: "chunks".into(),
                api_key: None,
                recency_weight: 0.05,
                recency_half_life_days: 180,
                pinned_boost: 0.15,
            },
            infer: InferConfig {
                synthesize: SynthesizeRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    max_output_tokens: 8192,
                    output_ratio: 1.4,
                    tokenizer_path: None,
                    timeout_secs: engram::config::DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    cooldown_secs: 0,
                },
                embed: EmbedRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "e".into(),
                    api_key: None,
                    dim: 1024,
                    max_input_tokens: 8192,
                    timeout_secs: engram::config::DEFAULT_TIMEOUT_SECS,
                },
                ask: AskRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    timeout_secs: engram::config::DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                },
                rerank: None,
            },
            auth: AuthConfig {
                mode: AuthMode::Local,
                oidc: None,
                local: Some(LocalConfig {
                    username: "dev".into(),
                    password_hash: "$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aaaa".into(),
                }),
            },
            consolidate: ConsolidateConfig::default(),
        }
    }

    #[test]
    fn oidc_mode_requires_an_oidc_block() {
        let mut cfg = test_config();
        cfg.auth.mode = AuthMode::Oidc;
        cfg.auth.oidc = None;
        assert!(validate_auth(&cfg, false).is_err());
    }

    #[test]
    fn local_mode_requires_a_local_block() {
        let mut cfg = test_config();
        cfg.auth.mode = AuthMode::Local;
        cfg.auth.local = None;
        assert!(validate_auth(&cfg, false).is_err());
    }

    #[test]
    fn local_mode_on_a_public_bind_is_refused() {
        let mut cfg = test_config();
        cfg.server.bind = "0.0.0.0:8080".into();
        assert!(validate_auth(&cfg, false).is_err());
        assert!(
            validate_auth(&cfg, true).is_ok(),
            "explicit override must be honoured"
        );
    }

    #[test]
    fn a_valid_local_config_passes() {
        assert!(validate_auth(&test_config(), false).is_ok());
    }
}
