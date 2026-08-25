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
    /// Which tenant a data command acts on. Required by --reindex,
    /// --export-eval and --recompute-coverage: there is no longer one base for
    /// them to mean.
    #[arg(long, value_name = "SUBJECT")]
    user: Option<String>,
    /// List the users this instance knows, with their slug and judge grant.
    #[arg(long)]
    list_users: bool,
    /// Let SUBJECT reach /ui/judge, which is also the only route that writes
    /// config.toml.
    #[arg(long, value_name = "SUBJECT")]
    grant_judge: Option<String>,
    /// Take that grant back.
    #[arg(long, value_name = "SUBJECT")]
    revoke_judge: Option<String>,
    /// Remove SUBJECT: the row, the database file, and the Qdrant alias. The
    /// queue rows go with the row, through ON DELETE CASCADE; the sessions and
    /// API tokens go with it too, or a token nobody revoked would provision the
    /// account straight back.
    #[arg(long, value_name = "SUBJECT")]
    delete_user: Option<String>,
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
///
/// No tenant is opened here, so startup time does not scale with how many
/// people have signed up. What used to be in this function and is not any more
/// is everything that was about a *collection* rather than an endpoint —
/// `ensure_collection` and the embedding-recipe check moved into provisioning
/// and a tenant's first open, because there is no longer one collection to
/// mean. Reclaiming stuck work and expiring sessions moved to the repair tick,
/// which now owns the control database's own housekeeping.
async fn startup_checks(cfg: &Config) -> Result<()> {
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

/// Take over a single-user installation, once.
///
/// Guarded on the `users` table being empty, so this cannot fire on a running
/// multi-user instance however the config is edited afterwards. The alias is
/// renamed rather than the collections behind it: nothing re-embeds, and the
/// generation history the reindex path depends on is preserved.
///
/// Everything after the user row is rolled back if a later step fails, and the
/// row with it. A half-adopted install that boots is worse than one that
/// refuses, because it presents as a base whose searches have gone empty
/// rather than as an error anybody can read — and worse than that, a user row
/// left behind is one that makes `users` non-empty, which is the guard above.
/// The next boot would then skip adoption in silence and start an empty base
/// beside the operator's real one, for ever. So the order below puts every
/// step that can fail for an ordinary reason — a directory that cannot be
/// made, a database that cannot be read — either before the row is written or
/// behind a rollback that removes it.
///
/// The rename is passed in rather than performed here so that this can be
/// tested without a Qdrant: the steps that decide whether adoption is safe are
/// the guard, the row, the file and what came out of it, and none of them needs
/// a vector store to be up in order to be wrong.
async fn adopt<F, Fut>(
    cfg: &Config,
    control: &engram::store::control::Control,
    rename_alias: F,
) -> Result<Option<engram::store::control::User>>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let Some(subject) = cfg.migrate.adopt_subject.as_deref() else {
        return Ok(None);
    };
    if !control.users().await?.is_empty() {
        return Ok(None);
    }
    let old = std::path::Path::new(&cfg.store.path);
    if !old.exists() {
        return Ok(None);
    }

    // Before the row, not after it: this fails on a read-only mount or a
    // permission, and a failure between the row and here left the row.
    std::fs::create_dir_all(&cfg.store.dir)
        .map_err(|e| Error::Store(format!("could not make {}: {e}", cfg.store.dir)))?;

    control.provision(subject, None).await?;
    // The operator adopting an installation is the person who has been using
    // it, and judging is the one thing they could do before and would silently
    // stop being able to do after.
    if let Err(e) = control.set_can_judge(subject, true).await {
        let _ = control.delete_user(subject).await;
        return Err(e);
    }
    let user = match control.user(subject).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            let _ = control.delete_user(subject).await;
            return Err(Error::Store("the user just provisioned is not there".into()));
        }
        Err(e) => {
            let _ = control.delete_user(subject).await;
            return Err(e);
        }
    };

    // Before the file moves, so a failure here has nothing to put back. What
    // the old database holds about the person using it does not travel with
    // the rename: those three tables live in the control plane now.
    let carried = match control.carry_over_single_user(&cfg.store.path, subject).await {
        Ok(c) => c,
        Err(e) => {
            control.discard_carried_over(subject).await;
            let _ = control.delete_user(subject).await;
            return Err(Error::Store(format!(
                "could not carry {} into the control database: {e}",
                cfg.store.path
            )));
        }
    };

    let new = std::path::Path::new(&cfg.store.dir).join(format!("{}.db", user.slug));
    if let Err(e) = std::fs::rename(old, &new) {
        control.discard_carried_over(subject).await;
        let _ = control.delete_user(subject).await;
        return Err(Error::Store(format!(
            "could not move {} to {}: {e}",
            old.display(),
            new.display()
        )));
    }
    // WAL and shared-memory sidecars travel with the file they belong to. A
    // -wal left behind is a committed write that never reaches the base it was
    // written for, which reads afterwards as data that was there yesterday.
    for ext in ["-wal", "-shm"] {
        let from = std::path::PathBuf::from(format!("{}{ext}", old.display()));
        if from.exists() {
            let _ = std::fs::rename(&from, format!("{}{ext}", new.display()));
        }
    }

    let alias = format!("{}_{}", cfg.vector.collection, user.slug);
    if let Err(e) = rename_alias(alias).await {
        // Put the file back before failing, or the next boot finds no base to
        // adopt and quietly starts an empty one.
        let _ = std::fs::rename(&new, old);
        for ext in ["-wal", "-shm"] {
            let _ = std::fs::rename(
                format!("{}{ext}", new.display()),
                format!("{}{ext}", old.display()),
            );
        }
        control.discard_carried_over(subject).await;
        let _ = control.delete_user(subject).await;
        return Err(e);
    }
    tracing::info!(subject, slug = %user.slug, "adopted the single-user base");
    if !carried.is_empty() {
        tracing::info!(
            api_tokens = carried.tokens,
            sessions = carried.sessions,
            jobs = carried.jobs,
            "carried the single-user auth and queue rows into the control database"
        );
    }
    Ok(Some(user))
}

/// Resolve `--user`, or refuse with the list rather than picking one.
///
/// A default here is how the wrong collection gets reindexed: the operator
/// meant one tenant and the flag silently meant another. Refusing costs one
/// re-run; guessing costs a rebuild of somebody else's base.
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

/// Open one tenant's store, for the commands that only read SQLite.
///
/// Deliberately not through `Tenants`: these need neither a vector store nor an
/// inference endpoint to be up, and going through the registry would make them
/// wait on both.
async fn tenant_store(
    cfg: &Config,
    control: &engram::store::control::Control,
    user: &engram::store::control::User,
) -> Result<engram::store::Store> {
    let store_cfg = engram::config::StoreConfig {
        path: std::path::Path::new(&cfg.store.dir)
            .join(format!("{}.db", user.slug))
            .to_string_lossy()
            .to_string(),
        ..cfg.store.clone()
    };
    engram::store::Store::connect(&store_cfg, control.clone(), &user.subject).await
}

/// The account subcommands. Returns whether one of them ran, since each is an
/// exit rather than a step on the way to serving.
///
/// There is no admin role behind these: they are the operator at a shell, which
/// is the only place account changes belong on an instance whose accounts are
/// owned by an identity provider.
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
                "{}  {}  {}{}",
                u.subject,
                u.slug,
                u.email.as_deref().unwrap_or("-"),
                if u.can_judge { "  judge" } else { "" }
            );
        }
        return Ok(true);
    }
    if let Some(subject) = &args.grant_judge {
        if !control.set_can_judge(subject, true).await? {
            return Err(Error::Validation(format!("no such user: {subject}")));
        }
        // No restart, and no wait for a cache to turn over: the judge gate
        // reads this column on every request rather than the copy of the row
        // the registry is holding. See `web::tenant::CanJudge`.
        println!("{subject} may now judge, and write config.toml through /ui/judge");
        return Ok(true);
    }
    if let Some(subject) = &args.revoke_judge {
        if !control.set_can_judge(subject, false).await? {
            return Err(Error::Validation(format!("no such user: {subject}")));
        }
        // Takes effect on their next request, for the reason above: the gate
        // reads the column and not the registry's copy of it.
        println!("{subject} may no longer judge");
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
        // The vectors first: it is the one step that can fail for a reason
        // outside this machine, and a row deleted before it would leave a
        // collection nothing names and nobody will ever look for. The alias
        // and its generations go together — an alias per tenant is the whole
        // point, so nothing else is pointing at them.
        match engram::vector::qdrant::QdrantVectors::connect(&engram::config::VectorConfig {
            collection: alias.clone(),
            ..cfg.vector.clone()
        })
        .await
        {
            Ok(v) => v.drop_collection().await?,
            Err(e) => tracing::warn!(error = %e, "could not reach Qdrant to drop the alias"),
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

    let cfg = Config::load(args.config.as_deref())?;
    if args.print_config {
        println!("{}", cfg.redacted());
        return Ok(());
    }

    // Every command below reaches the control database, and none of them can
    // say which base they mean without it.
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

    if let Some(dir) = &args.export_eval {
        // SQLite only. The artifacts are already synthesised and the pairs are
        // already judged, so this costs nothing and needs neither Qdrant nor an
        // inference endpoint to be up.
        let user = require_user(&control, args.user.as_deref()).await?;
        let store = tenant_store(&cfg, &control, &user).await?;
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
        let user = require_user(&control, args.user.as_deref()).await?;
        let store = tenant_store(&cfg, &control, &user).await?;
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

    // One-time, and guarded on `users` being empty. The alias is renamed onto
    // the adopting user's name rather than the collections behind it moving,
    // so nothing re-embeds.
    let vector_cfg = cfg.vector.clone();
    adopt(&cfg, &control, |alias| async move {
        engram::vector::qdrant::QdrantVectors::connect(&vector_cfg)
            .await?
            .rename_alias(&alias)
            .await
    })
    .await?;

    let cfg_arc = Arc::new(cfg.clone());
    let tenants = Arc::new(engram::tenants::Tenants::new(
        cfg_arc.clone(),
        control.clone(),
        Arc::new(engram::tenants::QdrantFactory {
            cfg: cfg.vector.clone(),
        }),
    ));
    // No tenant is opened here, so startup does not scale with how many people
    // have signed up. The first request from each opens theirs.
    startup_checks(&cfg).await?;

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
    // The last searches served each left a write behind, one queue per tenant.
    // The listener is already closed, so this drains a bounded set rather than
    // chasing new work; bounded further in case one of them is stuck on a
    // wedged Qdrant. Only the open ones: a tenant nobody touched this run has
    // nothing in flight by construction.
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

    /// Adoption without the Qdrant half. The alias rename is passed in so this
    /// can run without a vector store; what is being tested here is the row,
    /// the file and the guard, and the rename's own failure has its own test.
    async fn adopt_dry(cfg: &Config, control: &Control) -> Result<Option<engram::store::control::User>> {
        adopt(cfg, control, |_alias| async { Ok(()) }).await
    }

    /// A config naming a single-user database in `dir`, with `dir/users` as the
    /// tenant directory.
    fn adopting_config(dir: &std::path::Path, subject: Option<&str>) -> Config {
        let mut cfg = Config::test_default();
        cfg.store.path = dir.join("engram.db").to_string_lossy().into();
        cfg.store.dir = dir.join("users").to_string_lossy().into();
        cfg.migrate.adopt_subject = subject.map(String::from);
        cfg
    }

    /// A real single-user base: connecting once puts the schema in the file.
    async fn single_user_base(cfg: &Config, control: &Control) {
        engram::store::Store::connect(&cfg.store, control.clone(), "unused")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn adoption_claims_the_single_user_database_once() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let cfg = adopting_config(dir.path(), Some("sub-1"));
        single_user_base(&cfg, &control).await;

        let user = adopt_dry(&cfg, &control).await.unwrap().expect("adopted");
        assert!(user.can_judge, "the adopting operator keeps the judge");
        assert!(
            !std::path::Path::new(&cfg.store.path).exists(),
            "the old file was moved, not copied"
        );
        assert!(
            dir.path()
                .join("users")
                .join(format!("{}.db", user.slug))
                .exists()
        );

        // Second boot is a no-op: the users table is no longer empty.
        assert!(adopt_dry(&cfg, &control).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn adoption_does_nothing_without_a_subject_to_adopt_for() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let cfg = adopting_config(dir.path(), None);
        single_user_base(&cfg, &control).await;
        assert!(adopt_dry(&cfg, &control).await.unwrap().is_none());
        assert!(
            std::path::Path::new(&cfg.store.path).exists(),
            "nothing was moved"
        );
    }

    #[tokio::test]
    async fn adoption_does_nothing_when_there_is_no_base_to_adopt() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let cfg = adopting_config(dir.path(), Some("sub-1"));
        assert!(adopt_dry(&cfg, &control).await.unwrap().is_none());
        assert!(control.users().await.unwrap().is_empty(), "no row was left behind");
    }

    /// A half-adopted install that boots is worse than one that refuses: it
    /// presents as a base whose searches have gone empty, with the file it
    /// should be reading sitting under a name nothing looks at.
    #[tokio::test]
    async fn a_failed_alias_rename_puts_the_file_back() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let cfg = adopting_config(dir.path(), Some("sub-1"));
        single_user_base(&cfg, &control).await;

        let err = adopt(&cfg, &control, |_alias| async {
            Err(Error::Vector("qdrant is down".into()))
        })
        .await;
        assert!(err.is_err());
        assert!(
            std::path::Path::new(&cfg.store.path).exists(),
            "the base was left where the next boot cannot find it"
        );
        assert!(
            control.users().await.unwrap().is_empty(),
            "a user row was left with no base behind it"
        );
    }

    /// The row is the guard. A step that fails after it is written and does
    /// not take it back out makes `users` non-empty for ever, and the next
    /// boot then skips adoption in silence: an empty base beside the
    /// operator's real one, with nothing anywhere saying why.
    #[tokio::test]
    async fn a_failure_before_the_file_moves_leaves_no_user_row() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let mut cfg = adopting_config(dir.path(), Some("sub-1"));
        single_user_base(&cfg, &control).await;
        // A tenant directory that cannot be made, because its parent is a file.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        cfg.store.dir = blocker.join("users").to_string_lossy().into();

        assert!(adopt_dry(&cfg, &control).await.is_err());
        assert!(
            control.users().await.unwrap().is_empty(),
            "a user row was left behind, and it is what turns adoption off"
        );
        assert!(
            std::path::Path::new(&cfg.store.path).exists(),
            "nothing should have moved"
        );
    }

    /// What the rename does not carry: the three tables that moved to the
    /// control plane. Without this the upgrade quietly invalidates every API
    /// token and drops whatever was queued when the old process stopped.
    #[tokio::test]
    async fn adoption_carries_the_tokens_sessions_and_queued_work_over() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let cfg = adopting_config(dir.path(), Some("sub-1"));
        single_user_base(&cfg, &control).await;
        legacy_auth_rows(&cfg.store.path).await;

        adopt_dry(&cfg, &control).await.unwrap().expect("adopted");

        let tokens = control.active_tokens().await.unwrap();
        assert_eq!(tokens.len(), 1, "the extension's token stopped working");
        assert_eq!(tokens[0].subject, "sub-1");
        assert!(
            control.get_session("sid-1").await.unwrap().is_some(),
            "the browser that was open got signed out"
        );
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE subject = 'sub-1'")
                .fetch_one(&control.pool)
                .await
                .unwrap();
        assert_eq!(queued, 1, "work queued at shutdown was dropped");
    }

    /// A token, a session and a queued unit, written the way the single-user
    /// build wrote them: in the tenant database, with no `subject` on the job.
    async fn legacy_auth_rows(path: &str) {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str(&format!("sqlite://{path}")).unwrap())
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE sessions (
               id TEXT PRIMARY KEY, subject TEXT NOT NULL, email TEXT,
               expires_at INTEGER NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE api_tokens (
               id TEXT PRIMARY KEY, name TEXT NOT NULL, token_hash TEXT NOT NULL,
               subject TEXT NOT NULL, created_at INTEGER NOT NULL,
               last_used_at INTEGER, revoked_at INTEGER, user_agent TEXT);
             CREATE TABLE jobs (
               id INTEGER PRIMARY KEY AUTOINCREMENT, stage TEXT NOT NULL,
               target_kind TEXT NOT NULL, target_id TEXT NOT NULL,
               state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
               run_after INTEGER NOT NULL DEFAULT 0, last_error TEXT, claimed_at INTEGER,
               created_at INTEGER NOT NULL DEFAULT 0, seq INTEGER NOT NULL DEFAULT 0,
               class INTEGER NOT NULL DEFAULT 0, UNIQUE(stage, target_id));
             INSERT INTO api_tokens (id, name, token_hash, subject, created_at)
               VALUES ('tok-1', 'extension', 'hash', 'dev', 10);
             INSERT INTO jobs (stage, target_kind, target_id)
               VALUES ('synthesize', 'corpus', 'c-1');",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, subject, email, expires_at, created_at)
             VALUES ('sid-1', 'dev', NULL, ?, 10)",
        )
        .bind(engram::store::now() + 3600)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    /// A second instance, or an operator editing the file after the fact.
    #[tokio::test]
    async fn adoption_refuses_once_anyone_is_registered() {
        let dir = tempfile::tempdir().unwrap();
        let control = Control::memory().await.unwrap();
        let cfg = adopting_config(dir.path(), Some("sub-1"));
        single_user_base(&cfg, &control).await;
        control.provision("someone-else", None).await.unwrap();

        assert!(adopt_dry(&cfg, &control).await.unwrap().is_none());
        assert!(
            std::path::Path::new(&cfg.store.path).exists(),
            "a live instance's single-user file was moved out from under it"
        );
    }

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
