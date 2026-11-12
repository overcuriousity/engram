//! The tenant registry: a subject in, a `Core` out. Provisioning spans three systems and is not
//! transactional, so every step is idempotent and logging in again recovers a crash.

use crate::config::Config;
use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::store::control::{Control, User};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Tenant {
    pub core: Core,
    pub user: User,
}

/// How a `Core` is built for a tenant; a trait so tests can use in-memory vectors.
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

/// The contained build's: vectors in the tenant's own database. Maps alias `<prefix>_<slug>` to
/// `<slug>.db`, and refuses an alias of any other form.
#[cfg(feature = "contained")]
pub struct SqliteFactory {
    pub dir: std::path::PathBuf,
    pub prefix: String,
    pub scoring: crate::vector::sqlite::Scoring,
}

#[cfg(feature = "contained")]
#[async_trait::async_trait]
impl VectorFactory for SqliteFactory {
    async fn open(&self, alias: &str, dim: usize) -> Result<Arc<dyn crate::vector::VectorStore>> {
        let slug = alias
            .strip_prefix(self.prefix.as_str())
            .and_then(|rest| rest.strip_prefix('_'))
            .filter(|slug| !slug.is_empty())
            .ok_or_else(|| {
                Error::Vector(format!(
                    "`{alias}` is not a collection of `{}`",
                    self.prefix
                ))
            })?;
        let path = self.dir.join(format!("{slug}.db"));
        let vectors: Arc<dyn crate::vector::VectorStore> =
            Arc::new(crate::vector::sqlite::SqliteVectors::connect(&path, self.scoring).await?);
        vectors.ensure_collection(dim).await?;
        Ok(vectors)
    }
}

pub struct Tenants {
    generation: Arc<std::sync::atomic::AtomicBool>,
    cfg: Arc<Config>,
    control: Control,
    vectors: Arc<dyn VectorFactory>,
    /// Open cores, and the order they were last used in.
    open: Mutex<(HashMap<String, Tenant>, Vec<String>)>,
    /// One provisioning at a time per subject, so racing first requests cannot both create the
    /// collection. Per subject since `open()` makes a Qdrant round trip; never removed.
    provisioning: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Subjects whose once-per-process opening work is done: `open` runs on every cache miss.
    first_opened: Mutex<HashSet<String>>,
    /// The working state each subject keeps across eviction, since `open` builds a fresh `Core` on
    /// every miss. Dropped once `Working::is_idle`; see `working_for`.
    working: Mutex<HashMap<String, crate::core::Working>>,
    live: Mutex<HashMap<String, crate::web::live::Live>>,
    /// Whether one tenant answers for every subject; see `Tenants::single`.
    solo: bool,
    #[cfg(feature = "contained")]
    local: Option<crate::infer::local::LocalModels>,
}

/// Who is opening a tenant, which decides how much rides along with it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Request,
    /// A worker claiming a unit, or the repair ticker walking the user table; nobody is waiting.
    Background,
}

impl Tenants {
    pub fn new(cfg: Arc<Config>, control: Control, vectors: Arc<dyn VectorFactory>) -> Tenants {
        Tenants {
            cfg,
            control,
            vectors,
            open: Mutex::new((HashMap::new(), Vec::new())),
            first_opened: Mutex::new(HashSet::new()),
            provisioning: Mutex::new(HashMap::new()),
            working: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            solo: false,
            #[cfg(feature = "contained")]
            local: None,
            generation: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Whether this instance can generate now: always on a server; a phone gates it.
    pub fn generation(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.generation.clone()
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

    #[cfg(feature = "contained")]
    pub fn with_local_models(mut self, models: crate::infer::local::LocalModels) -> Tenants {
        self.local = Some(models);
        self
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

    #[cfg(test)]
    pub fn is_open(&self, subject: &str) -> bool {
        self.open
            .lock()
            .map(|g| g.0.contains_key(subject))
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn working_count(&self) -> usize {
        self.working.lock().map(|m| m.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn working_holds(&self, subject: &str) -> bool {
        self.working
            .lock()
            .map(|m| m.contains_key(subject))
            .unwrap_or(false)
    }

    /// A registry that serves one already-open tenant whoever asks: `auth.mode = "local"`, where
    /// the local username differs from the subject the data was written under.
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

    /// The lock that serialises opening this subject; a poisoned map yields a private mutex.
    fn provisioning_lock(&self, subject: &str) -> Arc<tokio::sync::Mutex<()>> {
        match self.provisioning.lock() {
            Ok(mut m) => m.entry(subject.to_string()).or_default().clone(),
            Err(_) => Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The web door: an authenticated subject, provisioned on first sight.
    pub async fn get_or_provision(&self, subject: &str, email: Option<&str>) -> Result<Tenant> {
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let lock = self.provisioning_lock(subject);
        let _guard = lock.lock().await;
        // Checked again under the lock: the racing caller may have finished meanwhile.
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self.control.provision(subject, email).await?;
        let tenant = self.open(user, Trigger::Request).await?;
        self.remember(tenant.clone());
        Ok(tenant)
    }

    /// The worker door: never provisions, since a subject missing from `users` is deleted or a bug.
    pub async fn get(&self, subject: &str) -> Result<Tenant> {
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let lock = self.provisioning_lock(subject);
        let _guard = lock.lock().await;
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self.control.user(subject).await?.ok_or(Error::NotFound)?;
        let tenant = self.open(user, Trigger::Background).await?;
        self.remember(tenant.clone());
        Ok(tenant)
    }

    /// The same base for a pass over every user: an open tenant is reused untouched, a closed one
    /// is not kept, so the walk neither evicts the LRU nor reorders it.
    pub async fn get_transient(&self, subject: &str) -> Result<Tenant> {
        if let Some(t) = self.cached_quiet(subject) {
            return Ok(t);
        }
        let lock = self.provisioning_lock(subject);
        let _guard = lock.lock().await;
        if let Some(t) = self.cached_quiet(subject) {
            return Ok(t);
        }
        let user = self.control.user(subject).await?.ok_or(Error::NotFound)?;
        self.open(user, Trigger::Background).await
    }

    async fn open(&self, user: User, trigger: Trigger) -> Result<Tenant> {
        std::fs::create_dir_all(&self.cfg.store.dir)
            .map_err(|e| Error::Store(format!("could not make {}: {e}", self.cfg.store.dir)))?;
        let store = Store::connect(
            &self.db_path(&user).to_string_lossy(),
            self.control.clone(),
            &user.subject,
        )
        .await?;
        let vectors = self
            .vectors
            .open(&self.alias(&user), self.cfg.infer.embed.dim)
            .await?;
        // Before the core: building one parses the tokenizer, which `TokenCounter::warm` memoizes.
        crate::infer::budget::TokenCounter::warm(
            self.cfg.infer.tokenizer.as_deref(),
            std::path::Path::new(&self.cfg.store.dir),
        )
        .await;
        let mut core =
            Core::from_config_with(&self.cfg, vectors, store, self.working_for(&user.subject));
        core.live = self.live_for(&user.subject);
        #[cfg(feature = "contained")]
        let core = match &self.local {
            Some(models) => core.with_local_models(&self.cfg, models),
            None => core,
        };
        // Before serving: `weak_below` reads an unmeasured line as the configured floor, low
        // enough to call unrelated things related. A failure leaves the floor standing.
        if let Err(e) = core.adopt_measured_line().await {
            tracing::warn!(
                subject = %user.subject,
                error = %e,
                "could not read the stored weak line; the floor stands until the next pass"
            );
        }
        // On every open, not only the first: a core started under file params while the live
        // generation supersedes them never corrects itself (`FILE_PARAMS_SEEN`).
        if let Err(e) = generation_check(&core, &self.cfg).await {
            tracing::warn!(
                subject = %user.subject,
                error = %e,
                "could not name this base's generation; it serves under the file until the next open"
            );
        }
        self.on_first_open(&core, &user.subject, trigger);
        Ok(Tenant { core, user })
    }

    /// This subject's working memory across eviction; a poisoned map yields fresh state. Dropped
    /// only when no `Core` holds it and no sitting is warm.
    fn working_for(&self, subject: &str) -> crate::core::Working {
        let Ok(mut map) = self.working.lock() else {
            return crate::core::Working::default();
        };
        map.retain(|s, w| s == subject || !w.is_idle());
        map.entry(subject.to_string()).or_default().clone()
    }

    fn live_for(&self, subject: &str) -> crate::web::live::Live {
        self.live
            .lock()
            .map(|mut m| m.entry(subject.to_string()).or_default().clone())
            .unwrap_or_default()
    }

    /// Whether this is the first open of `subject` in this process; `true` exactly once.
    fn claim_first_open(&self, subject: &str) -> bool {
        self.first_opened
            .lock()
            .map(|mut seen| seen.insert(subject.to_string()))
            .unwrap_or(false)
    }

    /// Once-per-process collection work, lazily in the background: guarded by `claim_first_open`
    /// (`open` runs per miss) and by `trigger`, so the boot repair tick scrolls nothing.
    fn on_first_open(&self, core: &Core, subject: &str, trigger: Trigger) {
        if !self.claim_first_open(subject) {
            return;
        }
        let core = core.clone();
        let cfg = self.cfg.clone();
        let reconcile = trigger == Trigger::Request;
        core.background.clone().spawn(async move {
            if let Err(e) = embed_recipe_check(&core, &cfg).await {
                tracing::warn!(error = %e, "could not check the embedding recipe");
            }
            if !reconcile {
                return;
            }
            // The two stores are written separately, so a crash or a partial restore can leave
            // either side with entries the other lacks.
            if let Err(e) = core.heal_store_drift().await {
                tracing::warn!(error = %e, "could not reconcile the two stores; the next pass retries");
            }
        });
    }

    /// `cached`, without the recency bump; see `get_transient`.
    fn cached_quiet(&self, subject: &str) -> Option<Tenant> {
        let g = self.open.lock().ok()?;
        if self.solo {
            return g.0.values().next().cloned();
        }
        g.0.get(subject).cloned()
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

    /// A registry over in-memory vectors; keep the `TempDir`, which holds every tenant database.
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
            Arc::new(Tenants::new(
                Arc::new(cfg),
                control,
                Arc::new(MemoryFactory),
            )),
            dir,
        )
    }

    /// A registry whose vector store will not open (a Qdrant outage); provision via `control()`.
    pub async fn unopenable_tenants() -> (Arc<Tenants>, tempfile::TempDir) {
        struct BrokenFactory;
        #[async_trait::async_trait]
        impl VectorFactory for BrokenFactory {
            async fn open(
                &self,
                _alias: &str,
                _dim: usize,
            ) -> Result<Arc<dyn crate::vector::VectorStore>> {
                Err(Error::Vector("qdrant is down".into()))
            }
        }

        let dir = tempfile::tempdir().expect("scratch tenant dir");
        let mut cfg = Config::test_default();
        cfg.store.dir = dir.path().to_string_lossy().to_string();
        let control = Control::memory().await.unwrap();
        (
            Arc::new(Tenants::new(
                Arc::new(cfg),
                control,
                Arc::new(BrokenFactory),
            )),
            dir,
        )
    }

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

    #[cfg(feature = "contained")]
    #[tokio::test]
    async fn the_vectors_live_in_the_tenants_own_database_file() {
        use crate::vector::sqlite::Scoring;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::test_default();
        cfg.store.dir = dir.path().to_string_lossy().to_string();
        let prefix = cfg.vector.collection.clone();
        let cfg = std::sync::Arc::new(cfg);
        let control = crate::store::control::Control::memory().await.unwrap();
        let tenants = super::Tenants::new(
            cfg.clone(),
            control,
            std::sync::Arc::new(super::SqliteFactory {
                dir: dir.path().into(),
                prefix,
                scoring: Scoring::off(),
            }),
        );
        let t = tenants.get_or_provision("phone", None).await.unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".db"))
            .collect();
        assert_eq!(files, [format!("{}.db", t.user.slug)], "one base, one file");
        let tables: Vec<String> = sqlx::query_scalar!(
            r#"SELECT name AS "name!" FROM sqlite_master WHERE name LIKE 'vec_%' AND type='table'"#
        )
        .fetch_all(&t.core.store.pool)
        .await
        .unwrap();
        assert!(tables.contains(&"vec_points".to_string()), "{tables:?}");
    }

    #[cfg(feature = "contained")]
    #[tokio::test]
    async fn an_alias_the_registry_did_not_build_is_refused() {
        use super::VectorFactory;
        use crate::vector::sqlite::Scoring;
        let f = super::SqliteFactory {
            dir: "/tmp".into(),
            prefix: "artifacts".into(),
            scoring: Scoring::off(),
        };
        assert!(f.open("something_else", 8).await.is_err());
    }

    #[tokio::test]
    async fn a_first_request_provisions_and_a_second_reuses() {
        let (t, _dir) = test_tenants(8).await;
        let a = t.get_or_provision("sub-1", None).await.unwrap();
        let b = t.get_or_provision("sub-1", None).await.unwrap();
        assert_eq!(a.user.slug, b.user.slug);
        assert_eq!(t.open_count(), 1, "the second request reused the open core");
    }

    #[tokio::test]
    async fn a_reopened_tenant_serves_what_the_base_adopted_and_not_the_file() {
        use crate::store::generations::{GenerationParams, NewGeneration};
        // One resident tenant, so opening a second evicts the first.
        let (t, _dir) = test_tenants(1).await;
        let first = t.get_or_provision("sub-1", None).await.unwrap();
        let file = GenerationParams::from(
            *first
                .core
                .ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let live = first
            .core
            .store
            .live_generation()
            .await
            .unwrap()
            .expect("opening the base names a generation");
        assert_eq!(live.params, file, "nothing has moved yet");

        let moved = GenerationParams {
            recency_weight: file.recency_weight + 0.2,
            ..file
        };
        first
            .core
            .store
            .adopt_generation_lived(
                &NewGeneration {
                    params: moved,
                    embed_recipe: live.embed_recipe.clone(),
                    chat_model: live.chat_model.clone(),
                    parent_id: Some(live.id.clone()),
                },
                0.1,
            )
            .await
            .unwrap();
        drop(first);

        // A whole-table pass, as the repair ticker runs: it must not consume the first-open claim.
        t.get_transient("sub-1").await.unwrap();
        t.get_or_provision("sub-2", None).await.unwrap();

        let back = t.get("sub-1").await.unwrap();
        assert_eq!(
            GenerationParams::from(
                *back
                    .core
                    .ranking
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            ),
            moved,
            "the reopened core serves what the base adopted"
        );
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

        let again = t.get_or_provision("sub-a", None).await.unwrap();
        assert_eq!(again.user.slug, crate::store::control::slug_for("sub-a"));
        assert_eq!(t.control().users().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn an_evicted_tenant_comes_back_mid_sitting() {
        let (t, _dir) = test_tenants(1).await;
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        a.core.sittings.queried("sid-1", "how to mount", 100, 1800);

        t.get_or_provision("sub-b", None).await.unwrap();
        assert_eq!(t.open_count(), 1);

        let again = t.get_or_provision("sub-a", None).await.unwrap();
        assert_eq!(
            again.core.sittings.read("sid-1", 200, 1800).queries,
            vec!["how to mount".to_string()],
            "the sitting went out with the tenant's place in the LRU"
        );
    }

    #[tokio::test]
    async fn a_subject_with_nothing_warm_left_is_dropped_from_the_working_map() {
        let (t, _dir) = test_tenants(1).await;
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        a.core.sittings.queried("sid-1", "how to mount", 100, 1800);
        drop(a);

        t.get_or_provision("sub-b", None).await.unwrap();
        assert!(t.working_holds("sub-a"), "a live sitting is not idle");

        // `read` is what expires the sitting.
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        assert!(
            a.core
                .sittings
                .read("sid-1", 100_000, 1800)
                .queries
                .is_empty()
        );
        drop(a);

        t.get_or_provision("sub-b", None).await.unwrap();
        t.get_or_provision("sub-c", None).await.unwrap();
        assert!(
            !t.working_holds("sub-a"),
            "a subject nobody is serving, with nothing warm left, was kept"
        );
        assert_eq!(t.working_count(), 2);
    }

    #[tokio::test]
    async fn the_first_open_work_is_claimed_once_per_process_however_often_a_tenant_reopens() {
        let (t, _dir) = test_tenants(1).await;
        t.get_or_provision("sub-a", None).await.unwrap();
        t.get_or_provision("sub-b", None).await.unwrap();
        assert_eq!(t.open_count(), 1);
        t.get_or_provision("sub-a", None).await.unwrap();

        assert!(
            !t.claim_first_open("sub-a"),
            "reopening an evicted tenant asked for the collection scroll again"
        );
        assert!(
            !t.claim_first_open("sub-b"),
            "the other tenant's first open was not recorded either"
        );
        assert!(t.claim_first_open("sub-c"), "a tenant never opened");
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

pub async fn generation_check(core: &Core, cfg: &Config) -> Result<()> {
    let params = crate::store::generations::GenerationParams::from(
        *core
            .ranking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let chat_model = format!(
        "ask={};synth={}",
        cfg.infer.ask.as_ref().map_or("none", |a| a.model.as_str()),
        cfg.infer
            .synthesize
            .as_ref()
            .map_or("none", |s| s.model.as_str()),
    );
    let live = crate::store::generations::boot_generation(
        &core.store,
        params,
        &cfg.infer.embed.fingerprint(),
        &chat_model,
        cfg.evolve.autonomous.moves_ranking(),
    )
    .await?;
    // Shared by every clone of this core, so requests in flight move with it.
    *core
        .ranking
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = live.params.into();
    Ok(())
}

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
