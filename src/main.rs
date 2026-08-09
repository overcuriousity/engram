use clap::Parser;
use engram::config::{AuthMode, Config};
use engram::core::Core;
use engram::error::{Error, Result};
use engram::infer::budget::TokenCounter;
use engram::infer::openai::{HttpChunker, HttpCompleter, HttpEmbedder, HttpReranker};
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

fn build_core(
    cfg: &Config,
    vectors: Arc<dyn engram::vector::VectorStore>,
    store: engram::store::Store,
) -> Core {
    // Chunk size is capped by what the embedder accepts, with headroom for
    // token-count estimation error.
    let max_chunk_tokens = (cfg.infer.embed.max_input_tokens as f32 * 0.8) as usize;

    Core {
        store,
        vectors,
        chunker: Arc::new(
            HttpChunker::new(&cfg.infer.chunk).with_max_chunk_tokens(max_chunk_tokens),
        ),
        embedder: Arc::new(HttpEmbedder::new(&cfg.infer.embed)),
        reranker: cfg
            .infer
            .rerank
            .as_ref()
            .map(|r| Arc::new(HttpReranker::new(r)) as Arc<dyn engram::infer::Reranker>),
        completer: Arc::new(HttpCompleter::new(&cfg.infer.ask)),
        counter: Arc::new(TokenCounter::load(
            cfg.infer.chunk.tokenizer_path.as_deref(),
        )),
    }
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
        &cfg.infer.chunk.base_url,
        cfg.infer.chunk.api_key.as_deref(),
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

    validate_auth(&cfg, args.i_know_this_is_insecure)?;

    let store = engram::store::Store::connect(&cfg.store).await?;
    let vectors: Arc<dyn engram::vector::VectorStore> =
        Arc::new(engram::vector::qdrant::QdrantVectors::connect(&cfg.vector).await?);
    let core = build_core(&cfg, vectors, store);
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
    let handles = engram::jobs::Worker::spawn(core, cfg.server.workers, shutdown_rx);

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
                chunk: ChunkRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    max_output_tokens: 8192,
                    output_ratio: 1.4,
                    tokenizer_path: None,
                },
                embed: EmbedRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "e".into(),
                    api_key: None,
                    dim: 1024,
                    max_input_tokens: 8192,
                },
                ask: AskRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
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
