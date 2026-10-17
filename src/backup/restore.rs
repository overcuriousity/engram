//! `engram --restore DIR [--user S] [--reembed] [--force-version]`.
//!
//! Spec §4. `preflight` is every check, and writes nothing: not a file under
//! `store.dir`, not the lock file, not a sidecar of the live control
//! database. Only once it has passed does `apply` take the lock, stage the
//! databases beside their targets, load each tenant's snapshot into the next
//! generation and point the alias at it, then swap the databases in by
//! `rename` — tenants first, the control database last — and end with the
//! verification of §5, reconciling as it goes.
//!
//! The previous generation of every alias is left in Qdrant: it is the
//! rollback, and the report names it with the request that deletes it.

use super::generations::{AliasState, Generations, QdrantGenerations};
use super::manifest::{self, Backend, Manifest, TenantEntry, tenant_file, valid_alias, valid_slug};
use super::verify::{self, Verdict, VerifyReport};
use super::{lock, sqlite};
use crate::config::Config;
use crate::error::{Error, Result};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, Row};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct RestoreOpts {
    /// One tenant, by slug or subject. Never touches `control.db`.
    pub user: Option<String>,
    /// Restore the databases and skip the snapshots: every tenant gets an
    /// empty generation at the configured width and re-embeds.
    pub reembed: bool,
    /// Accept an archive written by a newer engram.
    pub force_version: bool,
}

/// Why a restore will not start. Nothing has been written when one of these
/// is returned.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// The manifest does not parse, or a file it names is absent or altered.
    Archive(String),
    /// The server lock is held.
    ServerRunning(String),
    VersionSkew {
        archive: String,
        binary: String,
    },
    RecipeMismatch {
        archive: String,
        configured: String,
    },
    DimMismatch {
        archive: usize,
        configured: usize,
    },
    MissingCollection {
        slug: String,
    },
    BackendMismatch {
        archive: Backend,
        build: Backend,
    },
    UnknownUser(String),
}

fn backend_name(b: Backend) -> &'static str {
    match b {
        Backend::Qdrant => "qdrant",
        Backend::Sqlite => "sqlite",
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Archive(e) => write!(f, "the archive is not intact: {e}"),
            Refusal::ServerRunning(e) => write!(f, "{e}"),
            Refusal::VersionSkew { archive, binary } => write!(
                f,
                "the archive was written by engram {archive}, newer than this binary \
                 ({binary}); its schema may not open here. Run a newer engram, or pass \
                 --force-version"
            ),
            Refusal::RecipeMismatch {
                archive,
                configured,
            } => write!(
                f,
                "the archive's embedding recipe is not the configured one (model {archive}, \
                 config {configured}): its vectors do not compare with queries rendered \
                 now. Put the old recipe back, or pass --reembed to restore the databases \
                 and re-embed every artifact"
            ),
            Refusal::DimMismatch {
                archive,
                configured,
            } => write!(
                f,
                "the archive's vectors are {archive} wide and the config says {configured}: \
                 a collection of the wrong width cannot be queried. Pass --reembed to \
                 restore the databases and re-embed at {configured}"
            ),
            Refusal::MissingCollection { slug } => write!(
                f,
                "the archive holds no vector snapshot for {slug}, and a Qdrant restore \
                 needs one per tenant"
            ),
            Refusal::BackendMismatch { archive, build } => write!(
                f,
                "the archive's vectors are in {}, this build keeps them in {}; there is no \
                 automatic conversion",
                backend_name(*archive),
                backend_name(*build)
            ),
            Refusal::UnknownUser(e) => write!(f, "{e}"),
        }
    }
}

/// What `preflight` agreed to restore.
#[derive(Debug, Clone)]
pub struct Plan {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// The tenants to restore: all of them, or the one `--user` named.
    pub tenants: Vec<TenantEntry>,
    /// Whether `control.db` is restored too (not under `--user`).
    pub control: bool,
    pub reembed: bool,
}

/// `x.y.z`, anything after a `-` or `+` ignored.
fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let t = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(t)
}

/// Every check of spec §4 steps 0–1, in an order that reads nothing it does
/// not have to: the archive, then the lock, then the archive against this
/// build and config, and only then the live control database.
pub async fn preflight(
    cfg: &Config,
    dir: &Path,
    opts: &RestoreOpts,
    backend: Backend,
) -> std::result::Result<Plan, Refusal> {
    let archive = |e: Error| Refusal::Archive(e.to_string());
    let m = Manifest::load(dir).map_err(archive)?;
    // Names are rebuilt from validated slugs and aliases before any is joined
    // to `dir`; `verify` refuses the rest before it opens anything.
    for t in &m.tenants {
        let named = match &t.vector {
            Some(v) => valid_slug(&t.slug) && valid_alias(&v.alias),
            None => valid_slug(&t.slug),
        };
        if !named {
            break;
        }
        let present = t
            .vector
            .as_ref()
            .is_some_and(|v| dir.join(manifest::vector_file(&v.alias)).is_file());
        if m.vector_backend == Backend::Qdrant && !present {
            return Err(Refusal::MissingCollection {
                slug: t.slug.clone(),
            });
        }
    }
    m.verify(dir).map_err(archive)?;

    let control_path = Path::new(&cfg.store.control_path);
    lock::probe(control_path).map_err(|e| Refusal::ServerRunning(e.to_string()))?;

    let binary = env!("CARGO_PKG_VERSION");
    if !opts.force_version {
        let newer = match (semver(&m.engram_version), semver(binary)) {
            (Some(a), Some(b)) => a > b,
            _ => m.engram_version != binary,
        };
        if newer {
            return Err(Refusal::VersionSkew {
                archive: m.engram_version.clone(),
                binary: binary.into(),
            });
        }
    }

    if m.vector_backend != backend {
        return Err(Refusal::BackendMismatch {
            archive: m.vector_backend,
            build: backend,
        });
    }

    let configured = cfg.infer.embed.dim;
    if !opts.reembed {
        // The width is part of the fingerprint, so a width change is a recipe
        // change; it is named as such because it is the one that cannot be
        // queried at all. `--force-version` bypasses neither.
        let archived_dim = std::iter::once(m.embed.dim)
            .chain(
                m.tenants
                    .iter()
                    .filter_map(|t| t.vector.as_ref().map(|v| v.dim)),
            )
            .find(|d| *d != configured);
        if let Some(archive) = archived_dim {
            return Err(Refusal::DimMismatch {
                archive,
                configured,
            });
        }
        if m.embed.fingerprint != cfg.infer.embed.fingerprint() {
            return Err(Refusal::RecipeMismatch {
                archive: m.embed.model.clone(),
                configured: cfg.infer.embed.model.clone(),
            });
        }
    }

    let (tenants, control) = match &opts.user {
        None => (m.tenants.clone(), true),
        Some(u) => {
            let live = live_users(control_path).await.map_err(|e| {
                Refusal::UnknownUser(format!(
                    "--user {u}: cannot read the live control database: {e}"
                ))
            })?;
            let Some((slug, _)) = live.iter().find(|(slug, sub)| slug == u || sub == u) else {
                return Err(Refusal::UnknownUser(format!(
                    "--user {u} names no user in the live control database; a per-tenant \
                     restore needs the user to exist (it never touches control.db). Known: {}",
                    live.iter()
                        .map(|(s, _)| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            let Some(t) = m.tenants.iter().find(|t| &t.slug == slug) else {
                return Err(Refusal::UnknownUser(format!(
                    "--user {u} is {slug}, which this archive does not hold"
                )));
            };
            (vec![t.clone()], false)
        }
    };

    Ok(Plan {
        dir: dir.to_path_buf(),
        manifest: m,
        tenants,
        control,
        reembed: opts.reembed,
    })
}

/// `(slug, subject)` of every user in the live control database, read
/// `immutable`: no lock, and no `-wal` or `-shm` created beside a file this
/// command may be about to refuse to touch. The server is stopped (the lock
/// probe said so), so there is no writer to miss.
async fn live_users(control: &Path) -> Result<Vec<(String, String)>> {
    if !control.is_file() {
        return Err(Error::Store(format!(
            "{}: no such database",
            control.display()
        )));
    }
    let mut conn = SqliteConnectOptions::new()
        .filename(control)
        .read_only(true)
        .immutable(true)
        .connect()
        .await
        .map_err(|e| Error::Store(format!("{}: {e}", control.display())))?;
    let rows = sqlx::query("SELECT slug, subject FROM users ORDER BY slug")
        .fetch_all(&mut conn)
        .await?;
    let _ = conn.close().await;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("slug"), r.get::<String, _>("subject")))
        .collect())
}

/// What happened to one tenant's vectors.
#[derive(Debug, Clone)]
pub struct Moved {
    pub slug: String,
    pub alias: String,
    /// The generation the alias served before, now the rollback.
    pub previous: Option<String>,
    /// What it serves now, or why it still serves `previous`.
    pub current: std::result::Result<String, String>,
}

#[derive(Debug)]
pub struct Outcome {
    pub moved: Vec<Moved>,
    /// Tenants whose database was swapped in.
    pub swapped: Vec<String>,
    pub control_swapped: bool,
    pub reembed: bool,
    pub verify: VerifyReport,
}

impl Outcome {
    /// FAIL when any tenant was not restored, else the verification's word.
    pub fn verdict(&self) -> Verdict {
        if self.moved.iter().any(|m| m.current.is_err()) {
            Verdict::Fail
        } else {
            self.verify.verdict()
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.verdict().exit_code()
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for m in &self.moved {
            match &m.current {
                Ok(g) => writeln!(f, "{}  {} now serves {g}", m.slug, m.alias)?,
                Err(e) => writeln!(
                    f,
                    "{}  FAILED  {e}; {} still serves {}, and its database was not replaced",
                    m.slug,
                    m.alias,
                    m.previous.as_deref().unwrap_or("nothing")
                )?,
            }
        }
        if self.reembed {
            writeln!(
                f,
                "--reembed: the snapshots were skipped; every embedded artifact is queued again"
            )?;
        }
        if !self.control_swapped {
            writeln!(f, "control.db was not replaced")?;
        }
        let previous: Vec<&str> = self
            .moved
            .iter()
            .filter(|m| m.current.is_ok())
            .filter_map(|m| m.previous.as_deref())
            .collect();
        if !previous.is_empty() {
            writeln!(
                f,
                "previous generations were left in place as the rollback; once satisfied, delete each:"
            )?;
            for g in previous {
                writeln!(f, "  DELETE /collections/{g}")?;
            }
        }
        writeln!(f, "{}", self.verify)
    }
}

/// `--restore` against the configured Qdrant.
pub async fn restore(
    cfg: &Config,
    dir: &Path,
    opts: &RestoreOpts,
) -> std::result::Result<Outcome, RestoreError> {
    let gens = QdrantGenerations {
        cfg: cfg.vector.clone(),
    };
    restore_with(cfg, dir, opts, Backend::Qdrant, &gens).await
}

/// A refusal (nothing written) or a failure part way (see the message).
#[derive(Debug)]
pub enum RestoreError {
    Refused(Refusal),
    Failed(Error),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreError::Refused(r) => write!(f, "refused, nothing was changed: {r}"),
            RestoreError::Failed(e) => write!(f, "restore failed: {e}"),
        }
    }
}

pub async fn restore_with(
    cfg: &Config,
    dir: &Path,
    opts: &RestoreOpts,
    backend: Backend,
    gens: &dyn Generations,
) -> std::result::Result<Outcome, RestoreError> {
    let plan = preflight(cfg, dir, opts, backend)
        .await
        .map_err(RestoreError::Refused)?;
    apply(cfg, &plan, gens).await.map_err(RestoreError::Failed)
}

/// Spec §4 steps 2–6.
pub async fn apply(cfg: &Config, plan: &Plan, gens: &dyn Generations) -> Result<Outcome> {
    let control_path = Path::new(&cfg.store.control_path);
    let _lock = lock::acquire(control_path)?;
    stage_all(cfg, plan)?;
    let moved = move_vectors(cfg, plan, gens).await;

    // A tenant whose vectors did not move keeps its old database too: the
    // old pair agrees with itself, a new database over old vectors would not.
    let mut swapped = vec![];
    for (t, m) in plan.tenants.iter().zip(&moved) {
        let target = tenant_db(cfg, &t.slug);
        let staged = sqlite::restoring(&target);
        if m.current.is_ok() {
            sqlite::swap(&staged, &target)?;
            swapped.push(t.slug.clone());
        } else {
            let _ = std::fs::remove_file(&staged);
        }
    }
    let staged_control = sqlite::restoring(control_path);
    let control_swapped =
        plan.control && moved.iter().all(|m| m.current.is_ok()) && staged_control.exists();
    if control_swapped {
        sqlite::swap(&staged_control, control_path)?;
    } else if plan.control {
        let _ = std::fs::remove_file(&staged_control);
    }

    let scope = (!plan.control).then(|| plan.tenants[0].slug.as_str());
    let verify = verify::verify(cfg, scope, true, plan.reembed, gens).await?;
    Ok(Outcome {
        moved,
        swapped,
        control_swapped,
        reembed: plan.reembed,
        verify,
    })
}

fn tenant_db(cfg: &Config, slug: &str) -> PathBuf {
    Path::new(&cfg.store.dir).join(format!("{slug}.db"))
}

/// Step 3: every database copied beside its target and re-hashed.
fn stage_all(cfg: &Config, plan: &Plan) -> Result<()> {
    std::fs::create_dir_all(&cfg.store.dir)
        .map_err(|e| Error::Store(format!("{}: {e}", cfg.store.dir)))?;
    for t in &plan.tenants {
        sqlite::stage(
            &plan.dir.join(tenant_file(&t.slug)),
            &sqlite::restoring(&tenant_db(cfg, &t.slug)),
            &t.db.sha256,
        )?;
    }
    if plan.control {
        let control_path = Path::new(&cfg.store.control_path);
        if let Some(parent) = control_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Store(format!("{}: {e}", parent.display())))?;
        }
        sqlite::stage(
            &plan.dir.join(manifest::CONTROL_FILE),
            &sqlite::restoring(control_path),
            &plan.manifest.control.sha256,
        )?;
    }
    Ok(())
}

/// Step 4, per tenant: the next generation, filled and checked, then adopted.
async fn move_vectors(cfg: &Config, plan: &Plan, gens: &dyn Generations) -> Vec<Moved> {
    let mut out = Vec::with_capacity(plan.tenants.len());
    for t in &plan.tenants {
        let alias = format!("{}_{}", cfg.vector.collection, t.slug);
        let previous = match gens.state(&alias).await {
            Ok(AliasState::Live { generation, .. }) => Some(generation),
            _ => None,
        };
        let current = one_tenant(cfg, plan, t, &alias, gens)
            .await
            .map_err(|e| e.to_string());
        out.push(Moved {
            slug: t.slug.clone(),
            alias,
            previous,
            current,
        });
    }
    out
}

async fn one_tenant(
    cfg: &Config,
    plan: &Plan,
    t: &TenantEntry,
    alias: &str,
    gens: &dyn Generations,
) -> Result<String> {
    let dim = cfg.infer.embed.dim;
    let target = gens.next(alias).await?;
    match (&t.vector, plan.reembed) {
        (_, true) => gens.create_empty(alias, &target, dim).await?,
        (Some(v), false) => {
            let file = plan.dir.join(manifest::vector_file(&v.alias));
            gens.import(alias, &file, &target, dim).await?;
            let got = gens.count(alias, &target).await?;
            if got != v.points {
                let _ = gens.discard(alias, &target).await;
                return Err(Error::Vector(format!(
                    "{target} holds {got} points after the upload, the manifest says {}; \
                     it was dropped",
                    v.points
                )));
            }
        }
        // The vectors are in the database itself; nothing to move.
        (None, false) => return Ok(String::new()),
    }
    gens.adopt(alias, &target).await?;
    Ok(target)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backup::generations::tests::MemGens;
    use crate::backup::sqlite::tests::tenant_with_three;
    use crate::backup::take::take_with;
    use crate::store::control::Control;
    use crate::vector::memory::MemoryVectors;
    use crate::vector::{VectorPayload, VectorPoint, VectorStore};
    use std::collections::BTreeMap;

    pub(crate) struct Base {
        _tmp: tempfile::TempDir,
        pub cfg: Config,
        pub archive: PathBuf,
        pub store_dir: PathBuf,
        pub slugs: Vec<String>,
        pub subjects: Vec<String>,
        pub gens: MemGens,
        /// Per slug, the one artifact the base calls embedded.
        pub embedded: Vec<String>,
    }

    impl Base {
        pub fn alias(&self, i: usize) -> String {
            format!("{}_{}", self.cfg.vector.collection, self.slugs[i])
        }
        pub fn control(&self) -> PathBuf {
            PathBuf::from(&self.cfg.store.control_path)
        }
        pub fn db(&self, i: usize) -> PathBuf {
            self.store_dir.join(format!("{}.db", self.slugs[i]))
        }
    }

    pub(crate) fn point(id: &str) -> VectorPoint {
        VectorPoint {
            vector: vec![1.0, 0.0],
            sparse: Default::default(),
            payload: VectorPayload {
                artifact_id: id.into(),
                ..Default::default()
            },
        }
    }

    /// Two tenants, three artifacts each with one embedded and its point
    /// served from `{alias}_v1`, the queue empty, every pool closed (the
    /// server is stopped), and a backup of all of it in `archive`.
    pub(crate) async fn backed_up() -> Base {
        backed_up_with(|id| vec![point(id)]).await
    }

    /// `backed_up`, with `points(embedded_id)` the points each tenant's
    /// generation holds when the backup is taken, to put a skew into it.
    pub(crate) async fn backed_up_with(points: impl Fn(&str) -> Vec<VectorPoint>) -> Base {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("store");
        std::fs::create_dir(&store_dir).unwrap();
        let mut cfg = Config::test_default();
        cfg.infer.embed.dim = 2;
        cfg.store.dir = store_dir.to_string_lossy().into();
        cfg.store.control_path = store_dir.join("control.db").to_string_lossy().into();
        let control = Control::connect(&cfg.store.control_path).await.unwrap();
        let gens = MemGens::default();
        let mut users = vec![];
        for sub in ["sub-a", "sub-b"] {
            users.push(control.provision(sub, None).await.unwrap());
        }
        users.sort_by(|a, b| a.slug.cmp(&b.slug));
        let mut embedded = vec![];
        for u in &users {
            let s = tenant_with_three(&store_dir.join(format!("{}.db", u.slug))).await;
            let id: String =
                sqlx::query_scalar("SELECT id FROM artifacts WHERE embed_state = 'embedded'")
                    .fetch_one(&s.pool)
                    .await
                    .unwrap();
            s.meta_set("embed.recipe", &cfg.infer.embed.fingerprint())
                .await
                .unwrap();
            s.pool.close().await;
            let m = MemoryVectors::new();
            m.upsert(points(&id)).await.unwrap();
            gens.serve(&format!("{}_{}", cfg.vector.collection, u.slug), m, 2);
            embedded.push(id);
        }
        sqlx::query("DELETE FROM jobs")
            .execute(&control.pool)
            .await
            .unwrap();
        control.pool.close().await;
        let archive = tmp.path().join("archive");
        let report = take_with(&cfg, &archive, false, Backend::Qdrant, &gens)
            .await
            .unwrap();
        assert_eq!(report.exit_code(), 0, "{report}");
        Base {
            archive,
            store_dir,
            slugs: users.iter().map(|u| u.slug.clone()).collect(),
            subjects: users.iter().map(|u| u.subject.clone()).collect(),
            _tmp: tmp,
            cfg,
            gens,
            embedded,
        }
    }

    /// Every file under `dir`, by relative path, with its SHA-256.
    pub(crate) fn hash_dir(dir: &Path) -> BTreeMap<PathBuf, String> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let (h, _) = manifest::sha256_file(&p).unwrap();
                    out.insert(p.strip_prefix(dir).unwrap().to_path_buf(), h);
                }
            }
        }
        out
    }

    fn sha(p: &Path) -> String {
        manifest::sha256_file(p).unwrap().0
    }

    /// Run `preflight`, expect a refusal, and prove it wrote nothing.
    async fn refused(base: &Base, opts: &RestoreOpts, backend: Backend) -> Refusal {
        let before = hash_dir(&base.store_dir);
        let control = sha(&base.control());
        let r = preflight(&base.cfg, &base.archive, opts, backend)
            .await
            .unwrap_err();
        assert_eq!(hash_dir(&base.store_dir), before, "store.dir changed: {r}");
        assert_eq!(sha(&base.control()), control, "control.db changed: {r}");
        r
    }

    fn edit_manifest(base: &Base, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = base.archive.join(manifest::FILE);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        edit(&mut v);
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_running_server_is_refused() {
        let base = backed_up().await;
        let _server = lock::acquire(&base.control()).unwrap();
        let r = refused(&base, &RestoreOpts::default(), Backend::Qdrant).await;
        assert!(matches!(r, Refusal::ServerRunning(_)), "{r:?}");
        assert!(r.to_string().contains("engram is running (lock at"), "{r}");
        assert!(r.to_string().contains("stop it first"), "{r}");
    }

    #[tokio::test]
    async fn an_archive_from_a_newer_engram_is_refused_unless_forced() {
        let base = backed_up().await;
        edit_manifest(&base, |v| v["engram_version"] = "99.0.0".into());
        let r = refused(&base, &RestoreOpts::default(), Backend::Qdrant).await;
        assert!(matches!(r, Refusal::VersionSkew { .. }), "{r:?}");
        assert!(r.to_string().contains("engram 99.0.0, newer than"), "{r}");
        let forced = RestoreOpts {
            force_version: true,
            ..Default::default()
        };
        preflight(&base.cfg, &base.archive, &forced, Backend::Qdrant)
            .await
            .unwrap();

        // Older is allowed: the schema files migrate forward.
        edit_manifest(&base, |v| v["engram_version"] = "0.0.1".into());
        preflight(
            &base.cfg,
            &base.archive,
            &RestoreOpts::default(),
            Backend::Qdrant,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn another_embedding_recipe_is_refused_unless_reembedding() {
        let mut base = backed_up().await;
        base.cfg.infer.embed.model = "another-model".into();
        let r = refused(&base, &RestoreOpts::default(), Backend::Qdrant).await;
        assert!(matches!(r, Refusal::RecipeMismatch { .. }), "{r:?}");
        assert!(r.to_string().contains("embedding recipe"), "{r}");
        assert!(r.to_string().contains("--reembed"), "{r}");
        let forced = RestoreOpts {
            force_version: true,
            ..Default::default()
        };
        assert!(matches!(
            refused(&base, &forced, Backend::Qdrant).await,
            Refusal::RecipeMismatch { .. }
        ));
        let reembed = RestoreOpts {
            reembed: true,
            ..Default::default()
        };
        preflight(&base.cfg, &base.archive, &reembed, Backend::Qdrant)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn another_width_is_refused_and_force_version_does_not_bypass_it() {
        let mut base = backed_up().await;
        base.cfg.infer.embed.dim = 3;
        for opts in [
            RestoreOpts::default(),
            RestoreOpts {
                force_version: true,
                ..Default::default()
            },
        ] {
            let r = refused(&base, &opts, Backend::Qdrant).await;
            assert_eq!(
                r,
                Refusal::DimMismatch {
                    archive: 2,
                    configured: 3
                }
            );
            assert!(
                r.to_string().contains("2 wide and the config says 3"),
                "{r}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_snapshot_is_refused() {
        let base = backed_up().await;
        std::fs::remove_file(base.archive.join(manifest::vector_file(&base.alias(1)))).unwrap();
        let r = refused(&base, &RestoreOpts::default(), Backend::Qdrant).await;
        assert_eq!(
            r,
            Refusal::MissingCollection {
                slug: base.slugs[1].clone()
            }
        );
        assert!(r.to_string().contains("no vector snapshot"), "{r}");
    }

    #[tokio::test]
    async fn another_vector_backend_is_refused() {
        let base = backed_up().await;
        let r = refused(&base, &RestoreOpts::default(), Backend::Sqlite).await;
        assert_eq!(
            r,
            Refusal::BackendMismatch {
                archive: Backend::Qdrant,
                build: Backend::Sqlite
            }
        );
        assert!(r.to_string().contains("no automatic conversion"), "{r}");
    }

    #[tokio::test]
    async fn an_unknown_user_is_refused() {
        let base = backed_up().await;
        let opts = RestoreOpts {
            user: Some("nobody".into()),
            ..Default::default()
        };
        let r = refused(&base, &opts, Backend::Qdrant).await;
        assert!(matches!(r, Refusal::UnknownUser(_)), "{r:?}");
        assert!(r.to_string().contains("--user nobody names no user"), "{r}");

        // Live, but not in the archive.
        let control = Control::connect(&base.cfg.store.control_path)
            .await
            .unwrap();
        let late = control.provision("sub-late", None).await.unwrap();
        control.pool.close().await;
        let opts = RestoreOpts {
            user: Some(late.slug.clone()),
            ..Default::default()
        };
        let r = refused(&base, &opts, Backend::Qdrant).await;
        assert!(r.to_string().contains("this archive does not hold"), "{r}");
    }

    #[tokio::test]
    async fn a_manifest_slug_with_a_path_separator_is_refused() {
        let base = backed_up().await;
        for evil in ["../evil", "a/b", ".."] {
            edit_manifest(&base, |v| v["tenants"][0]["slug"] = evil.into());
            let r = refused(&base, &RestoreOpts::default(), Backend::Qdrant).await;
            assert!(matches!(r, Refusal::Archive(_)), "{evil}: {r:?}");
            assert!(r.to_string().contains("which is not a slug"), "{evil}: {r}");
        }
    }

    #[tokio::test]
    async fn an_altered_database_in_the_archive_is_refused() {
        let base = backed_up().await;
        let path = base.archive.join(tenant_file(&base.slugs[0]));
        let mut body = std::fs::read(&path).unwrap();
        let mid = body.len() / 2;
        body[mid] ^= 1;
        std::fs::write(&path, body).unwrap();
        let r = refused(&base, &RestoreOpts::default(), Backend::Qdrant).await;
        assert!(matches!(r, Refusal::Archive(_)), "{r:?}");
    }

    fn wipe(base: &Base) {
        for e in std::fs::read_dir(&base.store_dir).unwrap() {
            std::fs::remove_file(e.unwrap().path()).unwrap();
        }
        for i in 0..2 {
            base.gens.wipe(&base.alias(i));
        }
    }

    async fn run(base: &Base, opts: &RestoreOpts) -> Outcome {
        restore_with(&base.cfg, &base.archive, opts, Backend::Qdrant, &base.gens)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_restore_into_a_wiped_base_passes() {
        let base = backed_up().await;
        wipe(&base);
        let out = run(&base, &RestoreOpts::default()).await;
        assert_eq!(out.exit_code(), 0, "{out}");
        assert!(out.control_swapped);
        assert_eq!(out.swapped, base.slugs);
        for i in 0..2 {
            assert_eq!(
                base.gens.served(&base.alias(i)),
                Some(format!("{}_v1", base.alias(i)))
            );
            assert_eq!(
                sqlite::counts(&base.db(i)).await.unwrap(),
                (3, 1),
                "the database is back"
            );
        }
        assert!(
            !std::fs::read_dir(&base.store_dir).unwrap().any(|e| e
                .unwrap()
                .path()
                .to_string_lossy()
                .ends_with(".restoring")),
            "nothing staged is left"
        );
        assert!(out.to_string().ends_with("PASS\n"), "{out}");
    }

    #[tokio::test]
    async fn a_restore_over_a_live_base_keeps_the_previous_generation_and_says_how_to_delete_it() {
        let base = backed_up().await;
        let out = run(&base, &RestoreOpts::default()).await;
        assert_eq!(out.exit_code(), 0, "{out}");
        let text = out.to_string();
        for i in 0..2 {
            let a = base.alias(i);
            assert_eq!(base.gens.served(&a), Some(format!("{a}_v2")));
            assert!(
                base.gens.names().contains(&format!("{a}_v1")),
                "rollback kept"
            );
            assert!(
                text.contains(&format!("DELETE /collections/{a}_v1")),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn an_embedded_row_without_a_point_is_rearmed_and_the_verdict_is_repair() {
        // The skew an online backup can carry: a row embedded after its
        // tenant's vectors were captured. The snapshot lacks the point.
        let base = backed_up_with(|_| vec![]).await;
        // An empty memory store cannot know its width and exports 0; Qdrant
        // reports the collection's own.
        edit_manifest(&base, |v| {
            for t in v["tenants"].as_array_mut().unwrap() {
                t["vector"]["dim"] = 2.into();
            }
        });
        wipe(&base);
        let out = run(&base, &RestoreOpts::default()).await;
        assert_eq!(out.verdict(), Verdict::Repair(2), "{out}");
        assert_eq!(out.exit_code(), 2);
        let control = Control::connect(&base.cfg.store.control_path)
            .await
            .unwrap();
        for (i, id) in base.embedded.iter().enumerate() {
            let state: String = sqlx::query_scalar(
                "SELECT state FROM jobs WHERE subject = ? AND stage = 'embed' AND target_id = ?",
            )
            .bind(&base.subjects[i])
            .bind(id)
            .fetch_one(&control.pool)
            .await
            .unwrap();
            assert_eq!(state, "pending", "{id} re-armed");
        }
    }

    #[tokio::test]
    async fn an_orphan_point_is_pruned_and_the_verdict_is_pass() {
        // The other skew: a row deleted between the two captures leaves its
        // point in the snapshot.
        let base = backed_up_with(|id| vec![point(id), point("deleted-meanwhile")]).await;
        wipe(&base);
        let out = run(&base, &RestoreOpts::default()).await;
        assert_eq!(out.exit_code(), 0, "{out}");
        for i in 0..2 {
            let v = Generations::open(&base.gens, &base.alias(i)).await.unwrap();
            assert_eq!(
                v.all_artifact_ids().await.unwrap(),
                vec![base.embedded[i].clone()],
                "the orphan is gone"
            );
        }
        assert!(out.to_string().contains("0 after pruning 1"), "{out}");
    }

    #[tokio::test]
    async fn a_restore_interrupted_after_the_alias_swap_completes_on_a_rerun() {
        let base = backed_up().await;
        let plan = preflight(
            &base.cfg,
            &base.archive,
            &RestoreOpts::default(),
            Backend::Qdrant,
        )
        .await
        .unwrap();
        // The first run dies after step 4: aliases moved, nothing renamed.
        {
            let _lock = lock::acquire(&base.control()).unwrap();
            stage_all(&base.cfg, &plan).unwrap();
            let moved = move_vectors(&base.cfg, &plan, &base.gens).await;
            assert!(moved.iter().all(|m| m.current.is_ok()));
        }
        assert!(sqlite::restoring(&base.db(0)).exists(), "left staged");

        let out = run(&base, &RestoreOpts::default()).await;
        assert_eq!(out.exit_code(), 0, "{out}");
        assert!(out.control_swapped);
        for (i, m) in out.moved.iter().enumerate() {
            let a = base.alias(i);
            assert_eq!(m.previous.as_deref(), Some(format!("{a}_v2").as_str()));
            assert_eq!(m.current.as_deref(), Ok(format!("{a}_v3").as_str()));
            let names = base.gens.names();
            for g in [format!("{a}_v1"), format!("{a}_v2"), format!("{a}_v3")] {
                assert!(names.contains(&g), "{g} in {names:?}");
            }
        }
        assert!(!sqlite::restoring(&base.db(0)).exists());
    }

    #[tokio::test]
    async fn a_per_tenant_restore_never_touches_control_db() {
        let base = backed_up().await;
        wipe(&base);
        // The live base: control.db and the other tenant as they are now.
        let (ctl, other) = {
            let control = Control::connect(&base.cfg.store.control_path)
                .await
                .unwrap();
            for sub in &base.subjects {
                control.provision(sub, None).await.unwrap();
            }
            control.pool.close().await;
            let s = tenant_with_three(&base.db(1)).await;
            s.pool.close().await;
            (sha(&base.control()), sha(&base.db(1)))
        };
        let opts = RestoreOpts {
            user: Some(base.subjects[0].clone()),
            ..Default::default()
        };
        let plan = preflight(&base.cfg, &base.archive, &opts, Backend::Qdrant)
            .await
            .unwrap();
        assert!(!plan.control);
        let out = apply(&base.cfg, &plan, &base.gens).await.unwrap();
        assert!(!out.control_swapped);
        assert_eq!(out.swapped, vec![base.slugs[0].clone()]);
        assert_eq!(out.verify.tenants.len(), 1, "verified that tenant only");
        assert_eq!(sqlite::counts(&base.db(0)).await.unwrap(), (3, 1));
        assert_eq!(sha(&base.db(1)), other, "the other tenant is untouched");
        assert_eq!(base.gens.served(&base.alias(1)), None);
        assert_eq!(sha(&base.control()), ctl, "control.db is byte-identical");
        assert!(!sqlite::restoring(&base.control()).exists(), "never staged");
    }

    #[tokio::test]
    async fn reembed_creates_an_empty_generation_and_rearms_exactly_the_embedded_rows() {
        let mut base = backed_up().await;
        base.cfg.infer.embed.model = "another-model".into();
        base.cfg.infer.embed.dim = 3;
        let opts = RestoreOpts {
            reembed: true,
            ..Default::default()
        };
        let out = run(&base, &opts).await;
        let embedded: u64 = 2;
        assert_eq!(out.verdict(), Verdict::Repair(embedded), "{out}");
        assert_eq!(out.exit_code(), 2);
        for i in 0..2 {
            let a = base.alias(i);
            assert_eq!(
                base.gens.state(&a).await.unwrap(),
                AliasState::Live {
                    generation: format!("{a}_v2"),
                    dim: 3
                },
                "an empty generation at the configured width"
            );
            assert_eq!(base.gens.count(&a, &format!("{a}_v2")).await.unwrap(), 0);
        }
        let control = Control::connect(&base.cfg.store.control_path)
            .await
            .unwrap();
        let mut armed: Vec<String> =
            sqlx::query_scalar("SELECT target_id FROM jobs WHERE stage = 'embed'")
                .fetch_all(&control.pool)
                .await
                .unwrap();
        armed.sort();
        let mut want = base.embedded.clone();
        want.sort();
        assert_eq!(armed, want, "exactly the embedded rows, nothing else");
        assert!(out.to_string().contains("--reembed"), "{out}");
    }
}
