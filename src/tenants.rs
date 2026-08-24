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
}

impl Tenants {
    pub fn new(cfg: Arc<Config>, control: Control, vectors: Arc<dyn VectorFactory>) -> Tenants {
        Tenants {
            cfg,
            control,
            vectors,
            open: Mutex::new((HashMap::new(), Vec::new())),
            provisioning: tokio::sync::Mutex::new(()),
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

    pub fn open_count(&self) -> usize {
        self.open.lock().map(|g| g.0.len()).unwrap_or(0)
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
        Ok(Tenant { core, user })
    }

    fn cached(&self, subject: &str) -> Option<Tenant> {
        let mut g = self.open.lock().ok()?;
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
    use super::*;

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
