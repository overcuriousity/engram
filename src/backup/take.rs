//! `engram --backup DIR [--offline]` and `engram --backup-check DIR`.
//!
//! Order, from the spec §2: the control database first; the users enumerated
//! from that copy, so a user provisioned meanwhile is absent from both halves;
//! then per tenant the vectors before the database, so anything that lands in
//! the gap is a row without a point — the skew `heal_store_drift` repairs —
//! and the manifest last, so a directory without one is an aborted backup.

use super::manifest::{
    self, Backend, DbEntry, EmbedRecipe, FileEntry, Manifest, Mode, TenantEntry, VectorEntry,
    tenant_file, valid_slug, vector_file,
};
use super::{create_private, create_private_dir, lock, sqlite};
use crate::config::{Config, VectorConfig};
use crate::error::{Error, Result};
use crate::tenants::VectorFactory;
use crate::vector::{VectorExport, VectorStore};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Opens a tenant's alias without creating it. `QdrantFactory` calls
/// `ensure_collection`, which would answer a missing collection by making an
/// empty one and backing that up; a backup has to report it instead.
pub struct QdrantSource {
    pub cfg: VectorConfig,
}

#[async_trait::async_trait]
impl VectorFactory for QdrantSource {
    async fn open(&self, alias: &str, _dim: usize) -> Result<Arc<dyn VectorStore>> {
        let cfg = VectorConfig {
            collection: alias.to_string(),
            ..self.cfg.clone()
        };
        Ok(Arc::new(
            crate::vector::qdrant::QdrantVectors::connect(&cfg).await?,
        ))
    }
}

/// What a backup did, one line per tenant.
#[derive(Debug)]
pub struct Report {
    pub dir: PathBuf,
    pub mode: Mode,
    pub tenants: Vec<(String, std::result::Result<TenantEntry, String>)>,
}

impl Report {
    /// Whether any tenant the control copy names is missing from the archive.
    pub fn partial(&self) -> bool {
        self.tenants.iter().any(|(_, r)| r.is_err())
    }

    pub fn exit_code(&self) -> i32 {
        if self.partial() { 1 } else { 0 }
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (slug, r) in &self.tenants {
            match r {
                Ok(t) => {
                    write!(
                        f,
                        "{slug}  ok  artifacts {}  embedded {}",
                        t.db.artifacts, t.db.embedded
                    )?;
                    if let Some(v) = &t.vector {
                        write!(f, "  points {} ({})", v.points, v.generation)?;
                    }
                    writeln!(f)?;
                }
                Err(e) => writeln!(f, "{slug}  FAILED  {e}")?,
            }
        }
        let failed = self.tenants.iter().filter(|(_, r)| r.is_err()).count();
        if failed > 0 {
            write!(
                f,
                "partial: {failed} of {} tenants are not in {}",
                self.tenants.len(),
                self.dir.display()
            )
        } else {
            write!(
                f,
                "backup written to {} ({})",
                self.dir.display(),
                match self.mode {
                    Mode::Online => "online: reconcilable, not point-in-time",
                    Mode::Offline => "offline: point-in-time",
                }
            )
        }
    }
}

/// `--backup`, against the configured Qdrant.
pub async fn take(cfg: &Config, dir: &Path, offline: bool) -> Result<Report> {
    let source = QdrantSource {
        cfg: cfg.vector.clone(),
    };
    take_with(cfg, dir, offline, Backend::Qdrant, &source).await
}

/// `take`, over any vector store: the tests use in-memory ones.
pub async fn take_with(
    cfg: &Config,
    dir: &Path,
    offline: bool,
    backend: Backend,
    vectors: &dyn VectorFactory,
) -> Result<Report> {
    let control_path = Path::new(&cfg.store.control_path);
    // Before anything is created: a refusal leaves no trace.
    let _lock = if offline {
        Some(lock::acquire(control_path)?)
    } else {
        None
    };
    create_private_dir(dir)?;
    create_private_dir(&dir.join("tenants"))?;
    if backend == Backend::Qdrant {
        create_private_dir(&dir.join("vectors"))?;
    }

    let control_copy = dir.join(manifest::CONTROL_FILE);
    sqlite::vacuum_into(control_path, &control_copy).await?;
    let (sha256, bytes) = manifest::sha256_file(&control_copy)?;
    let slugs = sqlite::users_in(&control_copy).await?;

    let mut tenants = Vec::with_capacity(slugs.len());
    for slug in slugs {
        let r = tenant(cfg, dir, &slug, backend, vectors)
            .await
            .map_err(|e| e.to_string());
        tenants.push((slug, r));
    }

    let mode = if offline { Mode::Offline } else { Mode::Online };
    let m = Manifest {
        format: manifest::FORMAT,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
        engram_version: env!("ENGRAM_VERSION").to_string(),
        mode,
        embed: EmbedRecipe {
            model: cfg.infer.embed.model.clone(),
            dim: cfg.infer.embed.dim,
            fingerprint: cfg.infer.embed.fingerprint(),
        },
        vector_backend: backend,
        control: FileEntry {
            file: manifest::CONTROL_FILE.to_string(),
            sha256,
            bytes,
            users: tenants.len() as u64,
        },
        tenants: tenants
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok().cloned())
            .collect(),
        skipped: tenants
            .iter()
            .filter(|(_, r)| r.is_err())
            .map(|(s, _)| s.clone())
            .collect(),
    };
    write_manifest(dir, &m)?;
    Ok(Report {
        dir: dir.to_path_buf(),
        mode,
        tenants,
    })
}

/// One tenant: vectors, then the database, then the counts from the copies.
/// On failure nothing of this tenant is left in the archive.
async fn tenant(
    cfg: &Config,
    dir: &Path,
    slug: &str,
    backend: Backend,
    vectors: &dyn VectorFactory,
) -> Result<TenantEntry> {
    if !valid_slug(slug) {
        return Err(Error::Validation(format!("{slug:?} is not a slug")));
    }
    let alias = format!("{}_{slug}", cfg.vector.collection);
    let db_file = tenant_file(slug);
    let vec_file = vector_file(&alias);
    let r = async {
        let vector = match backend {
            Backend::Sqlite => None,
            Backend::Qdrant => {
                let store = vectors.open(&alias, cfg.infer.embed.dim).await?;
                match store.export_snapshot(&dir.join(&vec_file)).await? {
                    VectorExport::File {
                        generation,
                        sha256,
                        bytes,
                        points,
                        dim,
                    } => Some(VectorEntry {
                        alias: alias.clone(),
                        generation,
                        file: vec_file.clone(),
                        sha256,
                        bytes,
                        points,
                        dim,
                    }),
                    VectorExport::Embedded => {
                        return Err(Error::Vector(format!(
                            "{alias} keeps its vectors in the database, but this archive is for Qdrant"
                        )));
                    }
                }
            }
        };
        let live = Path::new(&cfg.store.dir).join(format!("{slug}.db"));
        let copy = dir.join(&db_file);
        sqlite::vacuum_into(&live, &copy).await?;
        let (artifacts, embedded) = sqlite::counts(&copy).await?;
        let (sha256, bytes) = manifest::sha256_file(&copy)?;
        Ok(TenantEntry {
            slug: slug.to_string(),
            db: DbEntry {
                file: db_file.clone(),
                sha256,
                bytes,
                artifacts,
                embedded,
            },
            vector,
        })
    }
    .await;
    if r.is_err() {
        let _ = std::fs::remove_file(dir.join(&vec_file));
        let _ = std::fs::remove_file(dir.join(&db_file));
    }
    r
}

/// Written beside, synced, then renamed into place: `manifest.json` either
/// exists whole or not at all.
fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    let tmp = dir.join(".manifest.json.tmp");
    let io = |e: std::io::Error| Error::Store(format!("{}: {e}", tmp.display()));
    let mut f = create_private(&tmp)?;
    serde_json::to_writer_pretty(&mut f, m).map_err(|e| Error::Store(e.to_string()))?;
    f.write_all(b"\n").map_err(io)?;
    f.sync_all().map_err(io)?;
    std::fs::rename(&tmp, dir.join(manifest::FILE)).map_err(io)
}

/// `--backup-check`: the manifest parses and every file it names is intact.
pub fn check(dir: &Path) -> Result<Manifest> {
    let m = Manifest::load(dir)?;
    m.verify(dir)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::sqlite::tests::tenant_with_three;
    use crate::store::control::Control;
    use crate::vector::memory::MemoryVectors;
    use crate::vector::{VectorPayload, VectorPoint};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Memory stores by alias; any other alias fails to open, the way an
    /// absent Qdrant alias fails to export. Records, per open, whether a
    /// manifest was already in the archive.
    struct Fixture {
        stores: HashMap<String, Arc<dyn VectorStore>>,
        archive: PathBuf,
        manifest_seen: Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl VectorFactory for Fixture {
        async fn open(&self, alias: &str, _dim: usize) -> Result<Arc<dyn VectorStore>> {
            self.manifest_seen
                .lock()
                .unwrap()
                .push(self.archive.join(manifest::FILE).exists());
            self.stores
                .get(alias)
                .cloned()
                .ok_or_else(|| Error::Vector(format!("alias `{alias}` does not exist")))
        }
    }

    struct Base {
        _tmp: tempfile::TempDir,
        cfg: Config,
        archive: PathBuf,
        slugs: Vec<String>,
        _stores: Vec<crate::store::Store>,
    }

    /// Two tenants with three artifacts each, the control database on disk.
    async fn two_tenants() -> Base {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("store");
        std::fs::create_dir(&store_dir).unwrap();
        let mut cfg = Config::test_default();
        cfg.store.dir = store_dir.to_string_lossy().into();
        cfg.store.control_path = store_dir.join("control.db").to_string_lossy().into();
        let control = Control::connect(&cfg.store.control_path).await.unwrap();
        let mut slugs = vec![];
        let mut stores = vec![];
        for (sub, email) in [("sub-a", "a@example.com"), ("sub-b", "b@example.com")] {
            let u = control.provision(sub, Some(email)).await.unwrap();
            stores.push(tenant_with_three(&store_dir.join(format!("{}.db", u.slug))).await);
            slugs.push(u.slug);
        }
        slugs.sort();
        Base {
            archive: tmp.path().join("archive"),
            _tmp: tmp,
            cfg,
            slugs,
            _stores: stores,
        }
    }

    async fn vectors_for(base: &Base, slugs: &[String]) -> Fixture {
        let mut stores: HashMap<String, Arc<dyn VectorStore>> = HashMap::new();
        for slug in slugs {
            let m = MemoryVectors::new();
            m.upsert(vec![VectorPoint {
                vector: vec![1.0, 0.0],
                sparse: Default::default(),
                payload: VectorPayload {
                    artifact_id: format!("{slug}-1"),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();
            stores.insert(
                format!("{}_{slug}", base.cfg.vector.collection),
                Arc::new(m),
            );
        }
        Fixture {
            stores,
            archive: base.archive.clone(),
            manifest_seen: Mutex::new(vec![]),
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn a_backup_has_the_spec_layout_and_private_modes() {
        let base = two_tenants().await;
        let fx = vectors_for(&base, &base.slugs).await;
        let report = take_with(&base.cfg, &base.archive, false, Backend::Qdrant, &fx)
            .await
            .unwrap();
        assert_eq!(report.exit_code(), 0, "{report}");

        let a = &base.archive;
        for d in [a.clone(), a.join("tenants"), a.join("vectors")] {
            assert_eq!(mode(&d), 0o700, "{}", d.display());
        }
        let mut files = vec![a.join("manifest.json"), a.join("control.db")];
        for slug in &base.slugs {
            files.push(a.join(format!("tenants/{slug}.db")));
            files.push(a.join(format!(
                "vectors/{}_{slug}.snapshot",
                base.cfg.vector.collection
            )));
        }
        for f in &files {
            assert_eq!(mode(f), 0o600, "{}", f.display());
        }
        let count = |d: &str| std::fs::read_dir(a.join(d)).unwrap().count();
        assert_eq!((count("."), count("tenants"), count("vectors")), (4, 2, 2));

        let m = check(a).unwrap();
        assert_eq!(m.mode, Mode::Online);
        assert_eq!(m.control.users, 2);
        assert!(m.skipped.is_empty());
        for t in &m.tenants {
            assert_eq!(
                (t.db.artifacts, t.db.embedded),
                (3, 1),
                "counted on the copy"
            );
            assert_eq!(t.vector.as_ref().unwrap().points, 1);
        }
        let text = std::fs::read_to_string(a.join("manifest.json")).unwrap();
        for secret in ["sub-a", "a@example.com", "http"] {
            assert!(!text.contains(secret), "manifest holds {secret:?}: {text}");
        }
    }

    #[tokio::test]
    async fn backup_check_fails_after_one_byte_of_control_db_is_flipped() {
        let base = two_tenants().await;
        let fx = vectors_for(&base, &base.slugs).await;
        take_with(&base.cfg, &base.archive, false, Backend::Qdrant, &fx)
            .await
            .unwrap();
        check(&base.archive).unwrap();

        let path = base.archive.join("control.db");
        let mut body = std::fs::read(&path).unwrap();
        let mid = body.len() / 2;
        body[mid] ^= 1;
        std::fs::write(&path, body).unwrap();
        let e = check(&base.archive).unwrap_err().to_string();
        assert!(e.contains("control.db"), "{e}");
    }

    #[tokio::test]
    async fn a_populated_directory_is_refused() {
        let base = two_tenants().await;
        let fx = vectors_for(&base, &base.slugs).await;
        std::fs::create_dir(&base.archive).unwrap();
        std::fs::write(base.archive.join("keep"), b"mine").unwrap();
        let e = take_with(&base.cfg, &base.archive, false, Backend::Qdrant, &fx)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not empty"), "{e}");
        assert_eq!(std::fs::read_dir(&base.archive).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_tenant_without_an_alias_makes_the_archive_partial_and_the_rest_is_kept() {
        let base = two_tenants().await;
        // Only the first tenant has vectors; the second's alias is absent.
        let fx = vectors_for(&base, &base.slugs[..1]).await;
        let report = take_with(&base.cfg, &base.archive, false, Backend::Qdrant, &fx)
            .await
            .unwrap();
        assert!(report.partial());
        assert_eq!(report.exit_code(), 1);
        let text = report.to_string();
        assert!(
            text.contains(&format!("{}  FAILED", base.slugs[1])),
            "{text}"
        );
        assert!(text.contains("partial"), "{text}");

        assert_eq!(
            *fx.manifest_seen.lock().unwrap(),
            vec![false, false],
            "the manifest is written after every tenant, not before"
        );
        let m = check(&base.archive).unwrap();
        assert_eq!(m.tenants.len(), 1);
        assert_eq!(m.tenants[0].slug, base.slugs[0]);
        assert_eq!(m.skipped, vec![base.slugs[1].clone()]);
        assert!(
            !base
                .archive
                .join(format!("tenants/{}.db", base.slugs[1]))
                .exists(),
            "nothing of the failed tenant is left behind"
        );
    }

    #[tokio::test]
    async fn offline_is_refused_under_a_held_lock_and_online_is_not() {
        let base = two_tenants().await;
        let fx = vectors_for(&base, &base.slugs).await;
        let _server = lock::acquire(Path::new(&base.cfg.store.control_path)).unwrap();

        let e = take_with(&base.cfg, &base.archive, true, Backend::Qdrant, &fx)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("is running"), "{e}");
        assert!(!base.archive.exists(), "a refusal creates nothing");

        let report = take_with(&base.cfg, &base.archive, false, Backend::Qdrant, &fx)
            .await
            .unwrap();
        assert_eq!(report.exit_code(), 0, "{report}");
    }

    #[tokio::test]
    async fn offline_takes_the_lock_and_says_so() {
        let base = two_tenants().await;
        let fx = vectors_for(&base, &base.slugs).await;
        take_with(&base.cfg, &base.archive, true, Backend::Qdrant, &fx)
            .await
            .unwrap();
        assert_eq!(check(&base.archive).unwrap().mode, Mode::Offline);
        assert!(
            lock::acquire(Path::new(&base.cfg.store.control_path)).is_ok(),
            "the lock is released when the backup ends"
        );
    }
}
