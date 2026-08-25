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
    /// Write artifacts.json and pairs.json for the evaluation harness into DIR,
    /// then exit. Reads only SQLite: no inference, no vector store. The pairs
    /// are the searches you judged; the artifacts keep their production ids, so
    /// re-exporting does not invalidate them.
    #[arg(long, value_name = "DIR")]
    export_eval: Option<std::path::PathBuf>,
    /// Re-measure every corpus's coverage from the artifacts already stored,
    /// then exit. Local work over existing rows: no inference, no vector calls,
    /// nothing re-synthesised. Run it after upgrading past a change to how
    /// coverage is measured, since the figure is otherwise written once.
    #[arg(long)]
    recompute_coverage: bool,
}

/// The subject the single-user boot path runs as until adoption exists.
///
/// Interim, and deliberately not a real identity: Task 8 replaces this whole
/// path with the control-only boot plus `migrate.adopt_subject`. It is
/// provisioned on the way past so that the foreign key on `jobs.subject`
/// holds in the meantime.
const BOOT_SUBJECT: &str = "single-user";

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
        .control
        .reclaim_stuck(engram::jobs::STUCK_AFTER_SECS)
        .await?;
    if reclaimed > 0 {
        tracing::info!(
            reclaimed,
            "requeued jobs left running by a previous process"
        );
    }
    let purged = core.store.control.purge_expired_sessions().await?;
    if purged > 0 {
        tracing::info!(purged, "removed expired sessions");
    }

    // The two stores hold complementary halves of the same artifact and are
    // written separately, so either can end up with an entry the other lacks: a
    // crash between the two writes, a restore of one from a backup taken at a
    // different moment. Until something notices, one side's artifacts are simply
    // missing.
    let worker = core.clone();
    core.background.spawn(async move {
        if let Err(e) = worker.heal_store_drift().await {
            tracing::warn!(error = %e, "could not reconcile the two stores; the next sweep retries");
        }
    });

    if let Some(s) = &cfg.infer.synthesize {
        engram::infer::openai::probe("chunk", &s.base_url, s.api_key.as_deref()).await;
    } else {
        tracing::info!(
            "synthesize not configured; capture embeds verbatim and nothing is synthesized"
        );
    }
    engram::infer::openai::probe(
        "embed",
        &cfg.infer.embed.base_url,
        cfg.infer.embed.api_key.as_deref(),
    )
    .await;
    engram::tenants::embed_recipe_check(core, cfg).await?;
    if let Some(r) = &cfg.infer.rerank {
        engram::infer::openai::probe("rerank", &r.base_url, r.api_key.as_deref()).await;
        if !r.applies_to(engram::config::RerankApply::Search) {
            tracing::info!("rerank scoped to ask; search returns vector order");
        }
    } else {
        tracing::info!("rerank not configured; search returns vector order");
    }
    if let Some(v) = &cfg.infer.vision {
        let (base_url, api_key) = v.resolve(cfg.infer.synthesize.as_ref());
        engram::infer::openai::probe("vision", &base_url, api_key.as_deref()).await;
    } else {
        tracing::info!("vision not configured; the image door is closed");
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
        let target = vectors.reindex(cfg.infer.embed.dim).await?;
        println!("{} now serves `{}`", cfg.vector.collection, target);
        return Ok(());
    }

    if let Some(dir) = &args.export_eval {
        // SQLite only. The artifacts are already synthesised and the pairs are
        // already judged, so this costs nothing and needs neither Qdrant nor an
        // inference endpoint to be up.
        let control = engram::store::control::Control::connect(&cfg.store.control_path).await?;
        control.provision(BOOT_SUBJECT, None).await?;
        let store = engram::store::Store::connect(&cfg.store, control, BOOT_SUBJECT).await?;
        let (artifacts, pairs, questions) = engram::eval::export::export(&store, dir).await?;
        println!(
            "wrote {artifacts} artifacts, {pairs} pairs and {questions} questions to {}",
            dir.display()
        );
        if pairs == 0 {
            println!(
                "no judged searches yet — set learn.enabled, use the base, \
                 then judge what it recorded at /ui/judge"
            );
        }
        return Ok(());
    }

    if args.recompute_coverage {
        // No vector store and no inference: this only reads artifacts and
        // writes one number per corpus, so it must not need either to be up.
        let control = engram::store::control::Control::connect(&cfg.store.control_path).await?;
        control.provision(BOOT_SUBJECT, None).await?;
        let store = engram::store::Store::connect(&cfg.store, control, BOOT_SUBJECT).await?;
        let core = Core::from_config(
            &cfg,
            Arc::new(engram::vector::memory::MemoryVectors::new()),
            store,
        );
        // Keyset, for the same reason the reconcile sweep uses one: a capture
        // arriving while this runs must not push a corpus past the window.
        let mut cursor: Option<(i64, String)> = None;
        loop {
            let page = core.store.list_corpora_after(cursor.as_ref(), 100).await?;
            let Some(last) = page.last() else { break };
            cursor = Some((last.created_at, last.id.clone()));
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
        }
        return Ok(());
    }

    validate_auth(&cfg, args.i_know_this_is_insecure)?;

    let control_handle = engram::store::control::Control::connect(&cfg.store.control_path).await?;
    control_handle.provision(BOOT_SUBJECT, None).await?;
    let store =
        engram::store::Store::connect(&cfg.store, control_handle.clone(), BOOT_SUBJECT).await?;
    let vectors: Arc<dyn engram::vector::VectorStore> =
        Arc::new(engram::vector::qdrant::QdrantVectors::connect(&cfg.vector).await?);
    let cfg_arc = Arc::new(cfg.clone());
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

    // Interim, until Task 8 replaces this with the control-only boot: one
    // registry holding the single core this path built.
    let user = control_handle
        .user(BOOT_SUBJECT)
        .await?
        .expect("the boot subject was provisioned above");
    let tenants = Arc::new(engram::tenants::Tenants::single(
        cfg_arc.clone(),
        core.clone(),
        user,
    ));

    let state = engram::web::state::AppState {
        tenants: tenants.clone(),
        config: cfg_arc.clone(),
        auth: Arc::new(engram::web::state::AuthContext {
            mode: cfg.auth.mode,
            local: cfg.auth.local.clone(),
            oidc,
            pending: engram::auth::oidc::PendingStore::new(),
            secure_cookies,
        }),
        // The path `Config::load` was given, or the name it looks for when it
        // was given none.
        config_path: Arc::new(
            args.config
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("config.toml")),
        ),
        ask_handoff: Default::default(),
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let background = core.background.clone();
    // One ticker left. The five that queued the sweeps are gone: a sweep arms
    // itself an interval out when it finishes, so the queue holds the schedule
    // and `run_after` is the cursor. Repair stays outside it — it is what
    // recovers an interrupted schedule, including arming a sweep that died
    // between being claimed and re-arming itself, so it cannot be scheduled by
    // the thing it recovers.
    let repair =
        engram::core::background::spawn_repair_ticker(tenants.clone(), shutdown_rx.clone());
    let mut handles =
        engram::jobs::Worker::spawn(tenants.clone(), cfg.server.workers, shutdown_rx);
    // Joined with the workers so shutdown waits for it too, rather than
    // leaving a task the runtime drops mid-enqueue.
    handles.push(repair);

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


    #[test]
    fn oidc_mode_requires_an_oidc_block() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        cfg.auth.oidc = None;
        assert!(validate_auth(&cfg, false).is_err());
    }

    #[test]
    fn local_mode_requires_a_local_block() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Local;
        cfg.auth.local = None;
        assert!(validate_auth(&cfg, false).is_err());
    }

    #[test]
    fn local_mode_on_a_public_bind_is_refused() {
        let mut cfg = Config::test_default();
        cfg.server.bind = "0.0.0.0:8080".into();
        assert!(validate_auth(&cfg, false).is_err());
        assert!(
            validate_auth(&cfg, true).is_ok(),
            "explicit override must be honoured"
        );
    }

    #[test]
    fn a_valid_local_config_passes() {
        assert!(validate_auth(&Config::test_default(), false).is_ok());
    }
}
