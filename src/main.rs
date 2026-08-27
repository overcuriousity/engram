use clap::Parser;
use engram::config::{AuthMode, Config};
use engram::core::Core;
use engram::error::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "engram",
    about = "engram: a self-hosted personal knowledge base.\n\nWith no verb flag it is the server. With -c, -s or -a it is a client of one."
)]
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
    /// The client half — `-c`, `-s`, `-a`. One parser for both, so `--help`
    /// lists the server's flags and the client's together and there is one
    /// binary to build, ship and version.
    #[command(flatten)]
    cli: engram::cli::args::CliArgs,
}

fn validate_auth(cfg: &Config, insecure_ok: bool) -> Result<()> {
    match cfg.auth.mode {
        AuthMode::Oidc => {
            let Some(oidc) = &cfg.auth.oidc else {
                return Err(Error::Validation(
                    "auth.mode = \"oidc\" but no [auth.oidc] block".into(),
                ));
            };
            // Refused at startup rather than answered either way at the first
            // login. An empty allowlist used to mean "everyone", and everyone
            // is not a small word here: the first request from a subject
            // engram has never seen provisions a tenant, so against a provider
            // with open self-registration that is a stranger creating a
            // database and a vector collection, with no cap anywhere on the
            // path. Silently closing it instead would lock a working
            // deployment out of its own instance on upgrade, which is why
            // neither reading is guessed.
            let listed = !oidc.allowed_subs.is_empty()
                || !oidc.allowed_emails.is_empty()
                || !oidc.allowed_groups.is_empty();
            if !listed && !oidc.open_registration {
                return Err(Error::Validation(
                    "[auth.oidc] names nobody who may sign in. List the people this instance is \
                     for in `allowed_subs`, `allowed_emails` or `allowed_groups` — or, if the \
                     identity provider is the only gate you want, set `open_registration = true`. \
                     Be aware that an open instance provisions a tenant for every subject the \
                     provider authenticates, so a provider that allows self-registration allows \
                     strangers to create databases here."
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
    let path = std::path::Path::new(&cfg.store.dir).join(format!("{}.db", user.slug));
    engram::store::Store::connect(&path.to_string_lossy(), control.clone(), &user.subject).await
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
            // Not a warning. The alias name is a pure function of the subject,
            // so a collection left behind is not merely orphaned: the next time
            // that person signs in, `ensure_collection` finds the surviving
            // alias and adopts it, and the deleted account comes back with
            // every vector it had behind a fresh, empty database. Leaving the
            // user row in place is the recoverable outcome — the operator fixes
            // Qdrant and runs the command again.
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
    let mut handles = engram::jobs::Worker::spawn(tenants.clone(), cfg.server.workers, shutdown_rx);
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
    fn oidc_mode_refuses_a_config_that_names_nobody() {
        // Neither reading is guessed. Reading it as "everyone" hands a tenant
        // — a control row, a database file and a vector collection — to every
        // subject the provider authenticates; reading it as "nobody" locks a
        // working deployment out of its own instance on upgrade. So it is a
        // startup error until an operator says which one they meant.
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        cfg.auth.oidc = Some(oidc_cfg());
        assert!(validate_auth(&cfg, false).is_err());
    }

    #[test]
    fn oidc_mode_passes_once_somebody_is_named() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        let mut oidc = oidc_cfg();
        oidc.allowed_emails = vec!["me@example.com".into()];
        cfg.auth.oidc = Some(oidc);
        assert!(validate_auth(&cfg, false).is_ok());
    }

    #[test]
    fn oidc_mode_passes_when_the_open_door_is_asked_for_in_writing() {
        let mut cfg = Config::test_default();
        cfg.auth.mode = AuthMode::Oidc;
        let mut oidc = oidc_cfg();
        oidc.open_registration = true;
        cfg.auth.oidc = Some(oidc);
        assert!(validate_auth(&cfg, false).is_ok());
    }

    fn oidc_cfg() -> engram::config::OidcConfig {
        engram::config::OidcConfig {
            issuer_url: "https://idp.example".into(),
            client_id: "engram".into(),
            client_secret: Some("s".into()),
            redirect_url: "https://engram.example/auth/callback".into(),
            scopes: vec!["openid".into()],
            open_registration: false,
            allowed_subs: vec![],
            allowed_emails: vec![],
            allowed_groups: vec![],
        }
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
