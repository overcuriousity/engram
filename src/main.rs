#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use clap::Parser;
use engram::config::{AuthMode, Config};
use engram::core::Core;
use engram::error::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "engram",
    version = env!("ENGRAM_VERSION"),
    about = "engram: a self-hosted personal knowledge base.\n\nWith no verb flag it is the server. With -c, -s, -a, --show or --status it is a client of one."
)]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print the effective config with secrets redacted, then exit.
    #[arg(long)]
    print_config: bool,
    /// Print an argon2id hash for a password, for auth.local.password_hash.
    #[arg(long)]
    hash_password: Option<String>,
    /// Write an empty database with both schemas, for `cargo sqlx prepare`.
    #[arg(long, hide = true, value_name = "PATH")]
    dev_schema: Option<PathBuf>,
    /// Copy every vector into a fresh collection generation, swap the alias onto it, then exit.
    #[arg(long)]
    reindex: bool,
    /// Re-measure every corpus's coverage from stored artifacts (no inference), then exit.
    #[arg(long)]
    recompute_coverage: bool,
    /// Fold too-short stored passages into their neighbour and re-embed, then exit. Idempotent.
    #[arg(long)]
    merge_orphans: bool,
    /// With --merge-orphans: list what would change and write nothing.
    #[arg(long, requires = "merge_orphans")]
    dry_run: bool,
    /// The tenant a data command acts on; required by --reindex and the other per-base commands.
    #[arg(long, value_name = "SUBJECT")]
    user: Option<String>,
    /// List the users this instance knows, with their slug and email.
    #[arg(long)]
    list_users: bool,
    /// Remove SUBJECT: its row, database file, Qdrant alias, sessions and API tokens.
    #[arg(long, value_name = "SUBJECT")]
    delete_user: Option<String>,
    /// Write the whole instance (databases, Qdrant snapshots, manifest) into DIR, then exit.
    #[arg(long, value_name = "DIR")]
    backup: Option<PathBuf>,
    /// With --backup: refuse unless the server is stopped, so the archive is one point in time.
    #[arg(long, requires = "backup")]
    offline: bool,
    /// Check that every file a backup's manifest names is present and intact, then exit.
    #[arg(long, value_name = "DIR")]
    backup_check: Option<PathBuf>,
    /// Put back an archive --backup wrote (refused while the server runs), then verify the stores.
    #[arg(long, value_name = "DIR")]
    restore: Option<PathBuf>,
    /// With --restore: restore the databases only and queue every artifact for embedding again.
    #[arg(long, requires = "restore")]
    reembed: bool,
    /// With --restore: accept an archive written by a newer engram.
    #[arg(long, requires = "restore")]
    force_version: bool,
    /// Check that the control database, tenant databases and vector store agree, then exit.
    #[arg(long)]
    verify_stores: bool,
    /// The client half: `-c`, `-s`, `-a`.
    #[command(flatten)]
    cli: engram::cli::args::CliArgs,
}

fn validate_auth(cfg: &Config) -> Result<()> {
    match cfg.auth.mode {
        AuthMode::Oidc => {
            let Some(oidc) = &cfg.auth.oidc else {
                return Err(Error::Validation(
                    "auth.mode = \"oidc\" but no [auth.oidc] block".into(),
                ));
            };
            // Refused at startup: "everyone" lets any provider account provision a tenant,
            // and "nobody" would lock a working deployment out on upgrade; neither is guessed.
            let listed = !oidc.allowed_subs.is_empty() || !oidc.allowed_groups.is_empty();
            if !listed && !oidc.open_registration {
                return Err(Error::Validation(
                    "[auth.oidc] names nobody; list allowed_subs or allowed_groups, or set open_registration = true."
                        .into(),
                ));
            }
            if !listed {
                tracing::warn!(
                    "auth.oidc.open_registration is set: every subject the identity provider \
                     authenticates gets a tenant provisioned on first request"
                );
            }
        }
        AuthMode::Local => {
            if cfg.auth.local.is_none() {
                return Err(Error::Validation(
                    "auth.mode = \"local\" but no [auth.local] block".into(),
                ));
            }
            engram::auth::local::assert_bind_is_safe(&cfg.server.bind)?;
        }
    }
    Ok(())
}

/// Fail fast on what would surface later as bad search results; inference probes only warn,
/// since ingest works while endpoints are down. Opens no tenant.
async fn startup_checks(cfg: &Config) -> Result<()> {
    match &cfg.infer.synthesize {
        Some(s) => {
            engram::infer::openai::probe("chunk", &s.base_url, s.api_key.as_deref()).await;
        }
        None => tracing::info!(
            "synthesize not configured; captures stay verbatim, and reminders, the journal's \
             moments and promotion are off"
        ),
    }
    engram::infer::openai::probe(
        "embed",
        &cfg.infer.embed.base_url,
        cfg.infer.embed.api_key.as_deref(),
    )
    .await;
    if let Some(r) = &cfg.infer.rerank {
        engram::infer::openai::probe_path(
            "rerank",
            &r.base_url,
            r.style.probe_path(),
            r.api_key.as_deref(),
        )
        .await;
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
    // Named but not probed: whisper.cpp-style servers serve only the transcription route.
    match &cfg.infer.transcribe {
        Some(t) => tracing::info!(model = %t.model, "speech to text configured"),
        None => tracing::info!("speech to text not configured; the search box has no microphone"),
    }
    Ok(())
}

/// The live ranking per base, under the file: an adopted generation overrides an unchanged file.
/// Best-effort, since `--print-config` must answer before the database exists.
async fn print_live_ranking(cfg: &Config) {
    println!("# ── what is actually running ──");
    println!("# The keys above are where each base starts. Where a base has a");
    println!("# live generation, that is what retrieval uses instead — an idle");
    println!("# pass may have moved a knob since, and an unchanged file does not");
    println!("# take it back. Turning evolve.autonomous off returns the base to");
    println!("# the file at the next start.");
    if let Err(e) = live_ranking(cfg).await {
        println!("# (no base could be read from here: {e})");
    }
}

async fn live_ranking(cfg: &Config) -> Result<()> {
    let control = engram::store::control::Control::connect(&cfg.store.control_path).await?;
    let users = control.users().await?;
    if users.is_empty() {
        println!("# no base yet — the first login provisions one, and it starts from the file");
    }
    for u in users {
        let store = tenant_store(cfg, &control, &u).await?;
        match store.live_generation().await? {
            Some(g) => {
                println!("# {} — generation {} ({})", u.subject, g.id, g.state);
                println!("#   {:?}", g.params);
            }
            None => println!("# {} — no generation yet; the file is what runs", u.subject),
        }
    }
    Ok(())
}

/// A command that works on the data and exits; read by both `server_lock` and the client split.
fn data_command(args: &Args) -> bool {
    args.reindex
        || args.recompute_coverage
        || args.merge_orphans
        || args.list_users
        || args.delete_user.is_some()
        || args.restore.is_some()
        || args.verify_stores
}

/// The data commands that must not run beside a server: its writes during the swap or delete
/// would be lost or orphaned.
fn exclusive_data_command(args: &Args) -> bool {
    args.reindex || args.delete_user.is_some()
}

/// The server lock, for the server and for `exclusive_data_command`s.
fn server_lock(args: &Args, cfg: &Config) -> Result<Option<engram::backup::lock::ServerLock>> {
    if data_command(args) && !exclusive_data_command(args) {
        return Ok(None);
    }
    engram::backup::lock::acquire(std::path::Path::new(&cfg.store.control_path)).map(Some)
}

/// Resolve `--user`, or refuse with the list: a guessed default rebuilds somebody else's base.
async fn require_user(
    control: &engram::store::control::Control,
    subject: Option<&str>,
) -> Result<engram::store::control::User> {
    let known = control.users().await?;
    match subject.and_then(|s| known.iter().find(|u| u.subject == s)) {
        Some(u) => Ok(u.clone()),
        None => Err(Error::Validation(format!(
            "--user is required, and must name one of: {}",
            if known.is_empty() {
                "nobody yet — no user has signed in".to_string()
            } else {
                known
                    .iter()
                    .map(|u| u.subject.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ))),
    }
}

/// Open one tenant's store without `Tenants`, so SQLite-only commands need no vector or inference.
async fn tenant_store(
    cfg: &Config,
    control: &engram::store::control::Control,
    user: &engram::store::control::User,
) -> Result<engram::store::Store> {
    let path = std::path::Path::new(&cfg.store.dir).join(format!("{}.db", user.slug));
    engram::store::Store::connect(&path.to_string_lossy(), control.clone(), &user.subject).await
}

/// The account subcommands; returns whether one ran. The operator at a shell, no admin role.
async fn run_account_command(
    args: &Args,
    cfg: &Config,
    control: &engram::store::control::Control,
) -> Result<bool> {
    if args.list_users {
        let users = control.users().await?;
        if users.is_empty() {
            println!("no users yet — the first OIDC login provisions one");
        }
        for u in users {
            println!(
                "{}  {}  {}",
                u.subject,
                u.slug,
                u.email.as_deref().unwrap_or("-"),
            );
        }
        return Ok(true);
    }
    if let Some(subject) = &args.delete_user {
        let user = require_user(control, Some(subject.as_str())).await?;
        let db = std::path::Path::new(&cfg.store.dir).join(format!("{}.db", user.slug));
        let alias = format!("{}_{}", cfg.vector.collection, user.slug);
        println!("this removes, permanently:");
        println!("  the user row for {subject}, and every queued job with it");
        println!("  every session and API token that subject holds");
        println!("  {}", db.display());
        println!("  the Qdrant alias {alias}, and every generation behind it");
        print!("type yes to go ahead: ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| Error::Validation(format!("could not read an answer: {e}")))?;
        if answer.trim() != "yes" {
            println!("left alone");
            return Ok(true);
        }
        // The vectors first: the step that can fail outside this machine, and a row
        // deleted before it would leave a collection nothing names.
        match engram::vector::qdrant::QdrantVectors::connect(&engram::config::VectorConfig {
            collection: alias.clone(),
            ..cfg.vector.clone()
        })
        .await
        {
            Ok(v) => v.drop_collection().await?,
            // Not a warning: the alias derives from the subject, so a surviving one brings
            // the vectors back at next sign-in. Keeping the row lets the operator rerun.
            Err(e) => {
                return Err(Error::Validation(format!(
                    "could not reach Qdrant to drop {alias}: {e}. \
                     Nothing was deleted; try again once Qdrant is reachable."
                )));
            }
        }
        control.delete_user(subject).await?;
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
        }
        println!("{subject} is gone");
        return Ok(true);
    }
    Ok(false)
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
    if let Some(path) = &args.dev_schema {
        engram::store::dev_schema(path).await?;
        return Ok(());
    }

    // The client half needs no config or database. Server flags are passed so `verb` stands down
    // before it reads stdin: `echo yes | engram --delete-user x` is not a note.
    let server_command = args.print_config
        || args.backup.is_some()
        || args.backup_check.is_some()
        || data_command(&args)
        || args.user.is_some();
    use std::io::IsTerminal;
    if let Some(verb) = engram::cli::args::verb(
        &args.cli,
        server_command,
        !std::io::stdin().is_terminal(),
        || {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        },
    )? {
        let code = engram::cli::run(verb, &args.cli).await;
        std::process::exit(code);
    }

    if let Some(dir) = &args.backup_check {
        match engram::backup::take::check(dir) {
            Ok(m) => {
                println!(
                    "{} intact: control database and {} tenants{}",
                    dir.display(),
                    m.tenants.len(),
                    if m.skipped.is_empty() {
                        String::new()
                    } else {
                        format!(" (partial: {} not archived)", m.skipped.join(", "))
                    }
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!("{}: {e}", dir.display());
                std::process::exit(1);
            }
        }
    }

    let cfg = Config::load(args.config.as_deref())?;
    if args.print_config {
        println!("{}", cfg.redacted());
        print_live_ranking(&cfg).await;
        return Ok(());
    }

    // Before `Control::connect`, which migrates: a backup must not write to the live files.
    if let Some(dir) = &args.backup {
        let report = engram::backup::take::take(&cfg, dir, args.offline).await?;
        println!("{report}");
        std::process::exit(report.exit_code());
    }

    // Before the lock and `Control::connect`: a restore takes the lock only after every refusal.
    if let Some(dir) = &args.restore {
        let opts = engram::backup::restore::RestoreOpts {
            user: args.user.clone(),
            reembed: args.reembed,
            force_version: args.force_version,
        };
        match engram::backup::restore::restore(&cfg, dir, &opts).await {
            Ok(out) => {
                println!("{out}");
                std::process::exit(out.exit_code());
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
    if args.verify_stores {
        let gens = engram::backup::generations::QdrantGenerations {
            cfg: cfg.vector.clone(),
        };
        let report =
            engram::backup::verify::verify(&cfg, args.user.as_deref(), false, false, &gens).await?;
        println!("{report}");
        std::process::exit(report.verdict().exit_code());
    }

    // Held until `main` returns, so `--backup --offline` and a restore can tell a server is up.
    let _server_lock = server_lock(&args, &cfg)?;

    let control = engram::store::control::Control::connect(&cfg.store.control_path).await?;

    if run_account_command(&args, &cfg, &control).await? {
        return Ok(());
    }

    if args.reindex {
        let user = require_user(&control, args.user.as_deref()).await?;
        let alias = format!("{}_{}", cfg.vector.collection, user.slug);
        let vectors =
            engram::vector::qdrant::QdrantVectors::connect(&engram::config::VectorConfig {
                collection: alias.clone(),
                ..cfg.vector.clone()
            })
            .await?;
        let target = vectors.reindex(cfg.infer.embed.dim).await?;
        println!("{alias} now serves `{target}`");
        return Ok(());
    }

    if args.recompute_coverage {
        // Reads artifacts and writes one number per corpus: needs no vectors or inference.
        let user = require_user(&control, args.user.as_deref()).await?;
        let store = tenant_store(&cfg, &control, &user).await?;
        let core = Core::from_config(
            &cfg,
            Arc::new(engram::vector::memory::MemoryVectors::new()),
            store,
        );
        // Keyset: a capture arriving meanwhile must not push a corpus past the window.
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

    if args.merge_orphans {
        let user = require_user(&control, args.user.as_deref()).await?;
        let store = tenant_store(&cfg, &control, &user).await?;
        // A dry run reads SQLite only; a real one deletes the orphans' points.
        let vectors: Arc<dyn engram::vector::VectorStore> = if args.dry_run {
            Arc::new(engram::vector::memory::MemoryVectors::new())
        } else {
            Arc::new(
                engram::vector::qdrant::QdrantVectors::connect(&engram::config::VectorConfig {
                    collection: format!("{}_{}", cfg.vector.collection, user.slug),
                    ..cfg.vector.clone()
                })
                .await?,
            )
        };
        let core = Core::from_config(&cfg, vectors, store);
        let mut cursor: Option<(i64, String)> = None;
        let mut total = 0usize;
        loop {
            let page = core.store.list_corpora_after(cursor.as_ref(), 100).await?;
            let Some(last) = page.last() else { break };
            cursor = Some((last.created_at, last.id.clone()));
            for c in &page {
                for m in engram::jobs::passages::merge_orphan_passages(&core, &c.id, args.dry_run)
                    .await?
                {
                    println!(
                        "{}  {} -> {}  {:?}",
                        c.id, m.orphan_id, m.into_id, m.orphan_text
                    );
                    total += 1;
                }
            }
        }
        let verb = if args.dry_run {
            "would merge"
        } else {
            "merged"
        };
        println!("{verb} {total} orphan passage(s)");
        return Ok(());
    }

    validate_auth(&cfg)?;
    let deployment = cfg
        .deployment()
        .map_err(|e| Error::Validation(e.to_string()))?;

    let cfg_arc = Arc::new(cfg.clone());
    let tenants = Arc::new(engram::tenants::Tenants::new(
        cfg_arc.clone(),
        control.clone(),
        Arc::new(engram::tenants::QdrantFactory {
            cfg: cfg.vector.clone(),
        }),
    ));
    // No tenant is opened here; each opens on its first request.
    startup_checks(&cfg).await?;

    let oidc = match cfg.auth.mode {
        AuthMode::Oidc => {
            let oidc_cfg = cfg.auth.oidc.as_ref().ok_or_else(|| {
                Error::Validation("auth.mode = \"oidc\" but no [auth.oidc] block".into())
            })?;
            Some(engram::auth::oidc::OidcClient::discover(oidc_cfg, &deployment.base()).await?)
        }
        AuthMode::Local => None,
    };
    if let Some(o) = &oidc {
        let local = cfg.auth.local.as_ref().map(|l| l.username.as_str());
        let n = control.backfill_issuer(o.issuer(), local).await?;
        if n > 0 {
            tracing::info!(users = n, "bound existing users to the configured issuer");
        }
    }

    let state = engram::web::state::AppState {
        tenants: tenants.clone(),
        config: cfg_arc.clone(),
        auth: Arc::new(engram::web::state::AuthContext {
            mode: cfg.auth.mode,
            local: cfg.auth.local.clone(),
            oidc,
            pending: engram::auth::oidc::PendingStore::new(),
            deployment: deployment.clone(),
        }),
        ask_handoff: Default::default(),
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // Sweeps re-arm themselves in the queue; repair recovers that schedule, so it stays outside.
    let repair =
        engram::core::background::spawn_repair_ticker(tenants.clone(), shutdown_rx.clone());
    let mut handles = engram::jobs::Worker::spawn(tenants.clone(), cfg.server.workers, shutdown_rx);
    // Joined with the workers, so shutdown does not drop it mid-enqueue.
    handles.push(repair);

    let listener = tokio::net::TcpListener::bind(&cfg.server.bind).await?;
    tracing::info!(bind = %listener.local_addr()?, mode = ?cfg.auth.mode, "engram listening");
    // Both are no-ops without NOTIFY_SOCKET, i.e. outside a Type=notify unit.
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    if let Some(every) = sd_notify::watchdog_enabled() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every / 2);
            loop {
                tick.tick().await;
                let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
            }
        });
    }

    axum::serve(
        listener,
        engram::web::app(state, &deployment)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
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
    // Drain the deferred search writes of open tenants, bounded in case Qdrant is wedged.
    for t in tenants.open_tenants() {
        let background = t.core.background.clone();
        if background.inflight() == 0 {
            continue;
        }
        tracing::info!(
            subject = %t.user.subject,
            inflight = background.inflight(),
            "draining background writes"
        );
        if tokio::time::timeout(std::time::Duration::from_secs(10), background.wait_idle())
            .await
            .is_err()
        {
            tracing::warn!(
                subject = %t.user.subject,
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

    use engram::store::control::Control;

    #[tokio::test]
    async fn a_data_command_names_its_tenant_or_says_who_it_could_have_meant() {
        let control = Control::memory().await.unwrap();
        control.provision("sub-a", None).await.unwrap();
        control.provision("sub-b", None).await.unwrap();

        assert_eq!(
            require_user(&control, Some("sub-b")).await.unwrap().subject,
            "sub-b"
        );
        let e = require_user(&control, None).await.unwrap_err().to_string();
        assert!(e.contains("sub-a") && e.contains("sub-b"), "{e}");
        assert!(
            require_user(&control, Some("nobody")).await.is_err(),
            "an unknown subject is not a silent default"
        );
    }

    #[test]
    fn the_server_holds_the_lock_and_so_do_reindex_and_delete_user() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::test_default();
        cfg.store.control_path = dir.path().join("control.db").to_string_lossy().into();
        let control = std::path::Path::new(&cfg.store.control_path);

        let server = server_lock(&Args::parse_from(["engram"]), &cfg).unwrap();
        assert!(server.is_some());
        let e = engram::backup::lock::acquire(control)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is running"), "{e}");
        let e = server_lock(&Args::parse_from(["engram"]), &cfg)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is running"), "a second server is refused: {e}");
        assert!(
            server_lock(&Args::parse_from(["engram", "--list-users"]), &cfg)
                .unwrap()
                .is_none(),
            "a data command runs beside the server"
        );
        for flags in [
            &["engram", "--reindex"][..],
            &["engram", "--delete-user", "sub@idp"],
        ] {
            let e = server_lock(&Args::parse_from(flags), &cfg)
                .unwrap_err()
                .to_string();
            assert!(e.contains("stop it first"), "{flags:?} is refused: {e}");
        }
        for flags in [
            &["engram", "--recompute-coverage"][..],
            &["engram", "--merge-orphans"],
            &["engram", "--restore", "/x"],
            &["engram", "--verify-stores"],
        ] {
            assert!(
                server_lock(&Args::parse_from(flags), &cfg)
                    .unwrap()
                    .is_none(),
                "{flags:?} runs beside the server"
            );
        }
        drop(server);
        let held = server_lock(&Args::parse_from(["engram", "--reindex"]), &cfg).unwrap();
        assert!(
            held.is_some(),
            "--reindex holds the lock once the server is gone"
        );
        drop(held);
        assert!(engram::backup::lock::acquire(control).is_ok());
    }

    #[test]
    fn reembed_and_force_version_mean_nothing_without_restore() {
        for f in ["--reembed", "--force-version"] {
            assert!(Args::try_parse_from(["engram", f]).is_err(), "{f}");
            assert!(Args::try_parse_from(["engram", "--restore", "/x", f]).is_ok());
        }
    }

    #[test]
    fn offline_means_nothing_without_backup() {
        assert!(Args::try_parse_from(["engram", "--offline"]).is_err());
        assert!(Args::try_parse_from(["engram", "--backup", "/x", "--offline"]).is_ok());
    }

    #[test]
    fn oidc_mode_requires_an_oidc_block() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        cfg.auth.oidc = None;
        assert!(validate_auth(&cfg).is_err());
    }

    #[test]
    fn oidc_mode_refuses_a_config_that_names_nobody() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        cfg.auth.oidc = Some(oidc_cfg());
        assert!(validate_auth(&cfg).is_err());
    }

    #[test]
    fn oidc_mode_passes_once_somebody_is_named() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        let mut oidc = oidc_cfg();
        oidc.allowed_subs = vec!["sub-1".into()];
        cfg.auth.oidc = Some(oidc);
        assert!(validate_auth(&cfg).is_ok());
    }

    #[test]
    fn oidc_mode_passes_when_the_open_door_is_asked_for_in_writing() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        let mut oidc = oidc_cfg();
        oidc.open_registration = true;
        cfg.auth.oidc = Some(oidc);
        assert!(validate_auth(&cfg).is_ok());
    }

    fn oidc_cfg() -> engram::config::OidcConfig {
        engram::config::OidcConfig {
            issuer_url: "https://idp.example".into(),
            client_id: "engram".into(),
            client_secret: Some("s".into()),
            scopes: vec!["openid".into()],
            open_registration: false,
            allowed_subs: vec![],
            allowed_groups: vec![],
        }
    }

    #[test]
    fn local_mode_requires_a_local_block() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Local;
        cfg.auth.local = None;
        assert!(validate_auth(&cfg).is_err());
    }

    #[test]
    fn local_mode_on_a_public_bind_is_refused() {
        let mut cfg = Config::test_default();
        cfg.server.bind = "0.0.0.0:8080".into();
        assert!(validate_auth(&cfg).is_err());
    }

    #[test]
    fn a_valid_local_config_passes() {
        assert!(validate_auth(&Config::test_default()).is_ok());
    }
}
