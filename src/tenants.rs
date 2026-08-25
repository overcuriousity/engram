//! The tenant registry: a subject in, a `Core` out.
//!
//! Provisioning is five steps and every one of them is idempotent, because a
//! crash part-way through has to be recoverable by logging in again rather
//! than by an operator with a shell. It is deliberately *not* transactional:
//! three systems are involved — the control database, a file, and Qdrant — and
//! nothing spans them. A Qdrant outage during a first login therefore fails
//! loudly at the door, since half-provisioning presents to the user as a base
//! whose searches come back empty.

use crate::config::Config;
use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::store::control::{Control, User};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One user's data: the core that reads and writes it, and who they are.
#[derive(Clone)]
pub struct Tenant {
    pub core: Core,
    pub user: User,
}

/// How a `Core` is built for a tenant.
///
/// The binary builds a Qdrant-backed one. The tests build an in-memory one,
/// which is the whole reason this is a trait rather than a call to
/// `QdrantVectors::connect` in the middle of `open`.
#[async_trait::async_trait]
pub trait VectorFactory: Send + Sync {
    async fn open(&self, alias: &str, dim: usize) -> Result<Arc<dyn crate::vector::VectorStore>>;
}

/// The real one: an alias per tenant over the configured Qdrant.
pub struct QdrantFactory {
    pub cfg: crate::config::VectorConfig,
}

#[async_trait::async_trait]
impl VectorFactory for QdrantFactory {
    async fn open(&self, alias: &str, dim: usize) -> Result<Arc<dyn crate::vector::VectorStore>> {
        let mut cfg = self.cfg.clone();
        cfg.collection = alias.to_string();
        let vectors: Arc<dyn crate::vector::VectorStore> =
            Arc::new(crate::vector::qdrant::QdrantVectors::connect(&cfg).await?);
        vectors.ensure_collection(dim).await?;
        Ok(vectors)
    }
}

pub struct Tenants {
    cfg: Arc<Config>,
    control: Control,
    vectors: Arc<dyn VectorFactory>,
    /// Open cores, and the order they were last used in.
    ///
    /// A map plus a recency vector rather than an LRU crate: the cap is in the
    /// tens, and a dependency for a linear scan over thirty-two entries is not
    /// a trade worth making.
    open: Mutex<(HashMap<String, Tenant>, Vec<String>)>,
    /// One provisioning at a time, so two first requests racing cannot both
    /// create the collection. `INSERT OR IGNORE` makes the row safe on its
    /// own; this is what makes the Qdrant call safe.
    provisioning: tokio::sync::Mutex<()>,
    /// Whether this registry holds one tenant that answers for every subject.
    /// See `Tenants::single`.
    solo: bool,
}

impl Tenants {
    pub fn new(cfg: Arc<Config>, control: Control, vectors: Arc<dyn VectorFactory>) -> Tenants {
        Tenants {
            cfg,
            control,
            vectors,
            open: Mutex::new((HashMap::new(), Vec::new())),
            provisioning: tokio::sync::Mutex::new(()),
            solo: false,
        }
    }

    pub fn control(&self) -> &Control {
        &self.control
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.cfg
    }

    pub fn db_path(&self, user: &User) -> std::path::PathBuf {
        std::path::Path::new(&self.cfg.store.dir).join(format!("{}.db", user.slug))
    }

    pub fn alias(&self, user: &User) -> String {
        format!("{}_{}", self.cfg.vector.collection, user.slug)
    }

    /// Every tenant currently held open, for shutdown to drain.
    pub fn open_tenants(&self) -> Vec<Tenant> {
        self.open
            .lock()
            .map(|g| g.0.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn open_count(&self) -> usize {
        self.open.lock().map(|g| g.0.len()).unwrap_or(0)
    }

    /// A registry that serves exactly one already-open tenant, whoever asks.
    ///
    /// What `auth.mode = "local"` is: one account, one base, no provisioning —
    /// and what every test written against the single-user app gets, with a
    /// real router and a real extractor in front of it. It answers for any
    /// subject because in local mode there is only ever one person behind the
    /// door, and the local username is not the same string as the subject the
    /// data was written under.
    ///
    /// Isolation is not tested through this. `test_support::test_tenants`
    /// builds a real registry, and that is what the cross-tenant tests use.
    pub fn single(cfg: Arc<Config>, core: Core, user: User) -> Tenants {
        struct NoVectors;
        #[async_trait::async_trait]
        impl VectorFactory for NoVectors {
            async fn open(
                &self,
                _alias: &str,
                _dim: usize,
            ) -> Result<Arc<dyn crate::vector::VectorStore>> {
                Err(Error::NotFound)
            }
        }
        let control = core.store.control.clone();
        let subject = user.subject.clone();
        let mut t = Tenants::new(cfg, control, Arc::new(NoVectors));
        t.solo = true;
        t.remember(Tenant { core, user });
        debug_assert!(t.cached(&subject).is_some());
        t
    }

    /// The web door: an authenticated subject, provisioned on first sight.
    pub async fn get_or_provision(&self, subject: &str, email: Option<&str>) -> Result<Tenant> {
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let _guard = self.provisioning.lock().await;
        // Checked again under the lock: the racing caller may have finished
        // while this one waited for it.
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self.control.provision(subject, email).await?;
        let tenant = self.open(user).await?;
        self.remember(tenant.clone());
        Ok(tenant)
    }

    /// The worker door: a subject read off a claimed queue row.
    ///
    /// Never provisions. A subject that is not in `users` is a deleted user or
    /// a bug, and inventing a tenant for it would create a database nobody
    /// asked for and nobody will ever look at.
    pub async fn get(&self, subject: &str) -> Result<Tenant> {
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let _guard = self.provisioning.lock().await;
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self.control.user(subject).await?.ok_or(Error::NotFound)?;
        let tenant = self.open(user).await?;
        self.remember(tenant.clone());
        Ok(tenant)
    }

    async fn open(&self, user: User) -> Result<Tenant> {
        std::fs::create_dir_all(&self.cfg.store.dir)
            .map_err(|e| Error::Store(format!("could not make {}: {e}", self.cfg.store.dir)))?;
        let store_cfg = crate::config::StoreConfig {
            path: self.db_path(&user).to_string_lossy().to_string(),
            ..self.cfg.store.clone()
        };
        let store = Store::connect(&store_cfg, self.control.clone(), &user.subject).await?;
        let vectors = self
            .vectors
            .open(&self.alias(&user), self.cfg.infer.embed.dim)
            .await?;
        let core = Core::from_config(&self.cfg, vectors, store);
        self.on_first_open(&core);
        Ok(Tenant { core, user })
    }

    /// What used to happen at boot, for the one base there was.
    ///
    /// Both of these are about a *collection* rather than an endpoint, so
    /// neither can be answered once for the instance any more. They run here,
    /// lazily, rather than for every registered user at startup: a hundred
    /// users would otherwise mean a hundred full collection scrolls before the
    /// port opens.
    ///
    /// On the background queue, not awaited. A first request must not wait out
    /// a scroll of the whole collection, and a base that cannot be reconciled
    /// is still a base its owner can read.
    fn on_first_open(&self, core: &Core) {
        let core = core.clone();
        let cfg = self.cfg.clone();
        core.background.clone().spawn(async move {
            if let Err(e) = embed_recipe_check(&core, &cfg).await {
                tracing::warn!(error = %e, "could not check the embedding recipe");
            }
            // The two stores hold complementary halves of the same artifact and
            // are written separately, so either can end up with an entry the
            // other lacks: a crash between the two writes, or a restore of one
            // from a backup taken at a different moment. Until something
            // notices, one side's artifacts are simply missing.
            if let Err(e) = core.heal_store_drift().await {
                tracing::warn!(error = %e, "could not reconcile the two stores; the next pass retries");
            }
        });
    }

    fn cached(&self, subject: &str) -> Option<Tenant> {
        let mut g = self.open.lock().ok()?;
        if self.solo {
            return g.0.values().next().cloned();
        }
        let t = g.0.get(subject).cloned()?;
        let (_, order) = &mut *g;
        order.retain(|s| s != subject);
        order.push(subject.to_string());
        Some(t)
    }

    fn remember(&self, tenant: Tenant) {
        let Ok(mut g) = self.open.lock() else { return };
        let subject = tenant.user.subject.clone();
        let (map, order) = &mut *g;
        map.insert(subject.clone(), tenant);
        order.retain(|s| *s != subject);
        order.push(subject);
        while map.len() > self.cfg.store.max_open_tenants.max(1) {
            let Some(oldest) = order.first().cloned() else {
                break;
            };
            order.remove(0);
            map.remove(&oldest);
        }
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// A registry over in-memory vectors and a temporary directory.
    ///
    /// The `TempDir` comes back with it: dropping it removes every tenant
    /// database, so a test that keeps the registry keeps its files.
    pub async fn test_tenants(cap: usize) -> (Arc<Tenants>, tempfile::TempDir) {
        struct MemoryFactory;
        #[async_trait::async_trait]
        impl VectorFactory for MemoryFactory {
            async fn open(
                &self,
                _alias: &str,
                _dim: usize,
            ) -> Result<Arc<dyn crate::vector::VectorStore>> {
                Ok(Arc::new(crate::vector::memory::MemoryVectors::new()))
            }
        }

        let dir = tempfile::tempdir().expect("scratch tenant dir");
        let mut cfg = Config::test_default();
        cfg.store.dir = dir.path().to_string_lossy().to_string();
        cfg.store.max_open_tenants = cap;
        let control = Control::memory().await.unwrap();
        (
            Arc::new(Tenants::new(Arc::new(cfg), control, Arc::new(MemoryFactory))),
            dir,
        )
    }

    /// Two provisioned tenants over one queue.
    pub async fn two_tenants() -> (Arc<Tenants>, Tenant, Tenant, tempfile::TempDir) {
        let (tenants, dir) = test_tenants(8).await;
        let a = tenants.get_or_provision("sub-a", None).await.unwrap();
        let b = tenants.get_or_provision("sub-b", None).await.unwrap();
        (tenants, a, b, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;

    #[tokio::test]
    async fn a_first_request_provisions_and_a_second_reuses() {
        let (t, _dir) = test_tenants(8).await;
        let a = t.get_or_provision("sub-1", None).await.unwrap();
        let b = t.get_or_provision("sub-1", None).await.unwrap();
        assert_eq!(a.user.slug, b.user.slug);
        assert_eq!(t.open_count(), 1, "the second request reused the open core");
    }

    #[tokio::test]
    async fn racing_first_requests_provision_once() {
        let (t, _dir) = test_tenants(8).await;
        let (one, two) = tokio::join!(
            {
                let t = t.clone();
                async move { t.get_or_provision("sub-1", None).await }
            },
            {
                let t = t.clone();
                async move { t.get_or_provision("sub-1", None).await }
            },
        );
        assert_eq!(one.unwrap().user.slug, two.unwrap().user.slug);
        assert_eq!(t.control().users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_subjects_get_two_slugs_two_files_and_two_aliases() {
        let (t, _dir) = test_tenants(8).await;
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        let b = t.get_or_provision("sub-b", None).await.unwrap();
        assert_ne!(a.user.slug, b.user.slug);
        assert!(t.db_path(&a.user).exists());
        assert!(t.db_path(&b.user).exists());
        assert_ne!(t.alias(&a.user), t.alias(&b.user));
    }

    #[tokio::test]
    async fn the_worker_path_refuses_to_provision_from_a_queue_row() {
        let (t, _dir) = test_tenants(8).await;
        assert!(t.get("never-seen").await.is_err());
        assert!(t.control().users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn opening_past_the_cap_evicts_the_least_recently_used() {
        let (t, _dir) = test_tenants(2).await;
        t.get_or_provision("sub-a", None).await.unwrap();
        t.get_or_provision("sub-b", None).await.unwrap();
        t.get_or_provision("sub-c", None).await.unwrap();
        assert_eq!(t.open_count(), 2);

        // Reopening is transparent: the same slug, the same file.
        let again = t.get_or_provision("sub-a", None).await.unwrap();
        assert_eq!(again.user.slug, crate::store::control::slug_for("sub-a"));
        assert_eq!(t.control().users().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_tenants_data_goes_in_its_own_file() {
        let (_t, a, b, _dir) = two_tenants().await;
        a.core
            .store
            .insert_corpus("only in a", "test", None)
            .await
            .unwrap();
        assert_eq!(a.core.store.list_corpora(10, 0).await.unwrap().len(), 1);
        assert!(b.core.store.list_corpora(10, 0).await.unwrap().is_empty());
    }
}

/// Say it out loud when the embedding recipe changed under a base that already
/// has vectors in it.
///
/// `model`, `dim` and the three templates together decide what a stored vector
/// means (`EmbedRole::fingerprint`). Change any of them and the vectors already
/// in the collection describe the old recipe while every new query is rendered
/// through the new one — a base that answers worse for no visible reason, with
/// nothing in any log tying it to the config edit that caused it.
///
/// A warning and not a refusal: the operator may be mid-migration, and a base
/// that will not open is worse than one that says what is wrong with it. The
/// fingerprint is stored either way, so the warning is printed once rather than
/// on every open.
///
/// Here rather than at startup because `meta` is per tenant, which is the whole
/// reason it stayed out of the control database: it also holds the sweep
/// cursors, and one shared row would have one tenant's recipe describing
/// everybody's collection.
pub async fn embed_recipe_check(core: &Core, cfg: &Config) -> Result<()> {
    const KEY: &str = "embed.recipe";
    let now = cfg.infer.embed.fingerprint();
    if let Some(before) = core.store.meta_get(KEY).await?
        && before != now
    {
        tracing::warn!(
            model = %cfg.infer.embed.model,
            "the embedding recipe changed — model, dim or a template. Vectors stored under the \
             old one do not compare with queries rendered through the new one: drop the \
             collection and re-capture, or put the old recipe back"
        );
    }
    core.store.meta_set(KEY, &now).await?;
    Ok(())
}
