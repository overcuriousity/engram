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
use std::collections::{HashMap, HashSet};
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
    /// One provisioning at a time *per subject*, so two first requests racing
    /// cannot both create the same collection. `INSERT OR IGNORE` makes the row
    /// safe on its own; this is what makes the Qdrant call safe.
    ///
    /// Per subject and not one lock for the registry, because the section it
    /// guards contains `open()` and `open()` contains a Qdrant round trip: a
    /// single lock meant every worker's cache miss queued behind whichever
    /// unrelated tenant happened to be opening, on a path that runs on every
    /// eviction and once per user per repair tick. Two people opening two
    /// different bases were never the race this exists for.
    ///
    /// Entries are never removed. There is one per subject the process has
    /// opened, which is bounded by the user table, and dropping one the moment
    /// it looks unused is how two callers end up holding two different mutexes
    /// for the same subject.
    provisioning: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Subjects whose once-per-process opening work has already been done.
    ///
    /// `open` runs on every cache *miss* and not once per process: a registry
    /// at its cap evicts and reopens, and the repair ticker walks every
    /// registered user by name. Without this the full collection scroll behind
    /// `heal_store_drift` rode along with each of those.
    first_opened: Mutex<HashSet<String>>,
    /// The in-memory working state each subject keeps across eviction.
    ///
    /// Kept here and not on the `Core` it is handed to, because the `Core` is
    /// the thing that goes away: `open` builds a fresh one on every cache miss,
    /// so a sitting living there lasted exactly as long as its tenant's place
    /// in the LRU. Past `store.max_open_tenants` active users that meant search
    /// and ask carrying nothing for whoever was evicted between two requests,
    /// against a `config.example.toml` that says eviction is transparent. See
    /// `core::Working`.
    ///
    /// Unlike `provisioning` and `first_opened`, entries here *are* removed:
    /// `Working::is_idle` says when nobody is serving the subject and nothing
    /// warm is left to keep, and `working_for` drops those as it goes.
    working: Mutex<HashMap<String, crate::core::Working>>,
    /// Whether this registry holds one tenant that answers for every subject.
    /// See `Tenants::single`.
    solo: bool,
}

/// Who is opening a tenant, which decides how much rides along with it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// Somebody is at the door waiting for their own base.
    Request,
    /// A worker claiming a unit, or the repair ticker walking the whole user
    /// table. Nobody is waiting, and there may be a hundred of these in a row.
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

    /// Whether this subject is one the registry currently holds open. For the
    /// test that says a whole-table pass does not change that; see
    /// `get_transient`.
    #[cfg(test)]
    pub fn is_open(&self, subject: &str) -> bool {
        self.open
            .lock()
            .map(|g| g.0.contains_key(subject))
            .unwrap_or(false)
    }

    /// How many subjects the registry is holding working memory for. For the
    /// test that says that map is bounded; see `working_for`.
    #[cfg(test)]
    pub fn working_count(&self) -> usize {
        self.working.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Whether this subject's working memory is still being held.
    #[cfg(test)]
    pub fn working_holds(&self, subject: &str) -> bool {
        self.working
            .lock()
            .map(|m| m.contains_key(subject))
            .unwrap_or(false)
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

    /// The lock that serialises opening this one subject. See `provisioning`.
    ///
    /// A poisoned map is not a reason to open the same collection twice, so a
    /// caller that finds one gets a private mutex and proceeds alone — which is
    /// the same exclusion it would have had if it were the only caller, and the
    /// only thing left to offer once the map cannot be read.
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
        // Checked again under the lock: the racing caller may have finished
        // while this one waited for it.
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self.control.provision(subject, email).await?;
        let tenant = self.open(user, Trigger::Request).await?;
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

    /// The same base, opened for a pass that walks *every* registered user.
    ///
    /// Two things `get` does that a whole-table walk must not. It holds the
    /// tenant in the registry, and the registry is a fixed-size LRU: a pass
    /// over five hundred users ends with the last `max_open_tenants` of them
    /// resident — the last by `created_at`, which is an arbitrary set — and
    /// every tenant somebody was actually using evicted behind it. And it bumps
    /// recency for the ones already open, so even the survivors come out of the
    /// pass ordered by the walk rather than by use.
    ///
    /// So: an open tenant is reused exactly as it is, without being touched,
    /// and a closed one is opened and handed over without being kept. The
    /// caller holds it for as long as it takes to repair that one base and then
    /// drops it, which is what keeps the number of live SQLite pools and vector
    /// clients at one rather than at one per registered user.
    ///
    /// The price is that a pass over a cold base opens and closes it every
    /// tick. That is the cost every cache miss already paid, and the
    /// alternative is letting an hourly repair decide what the instance keeps
    /// in memory.
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
        // Before the core, because building one parses the tokenizer and this
        // is an `async fn`: see `TokenCounter::warm`. Memoized, so every open
        // after the first is a lookup.
        crate::infer::budget::TokenCounter::warm(
            self.cfg.infer.tokenizer.as_deref(),
            std::path::Path::new(&self.cfg.store.dir),
        )
        .await;
        let core =
            Core::from_config_with(&self.cfg, vectors, store, self.working_for(&user.subject));
        self.on_first_open(&core, &user.subject, trigger);
        Ok(Tenant { core, user })
    }

    /// This subject's working memory, surviving however many times the registry
    /// has opened and evicted them.
    ///
    /// A poisoned map costs the sitting and nothing else: the caller gets fresh
    /// state, which is what it would have had before this existed.
    ///
    /// Departed subjects are dropped on the way past. Without that this map is
    /// a row per subject the process has ever opened — small rows, but the cap
    /// exists precisely because an instance may have far more users than it
    /// holds bases for. A row is dropped only when no `Core` is still holding
    /// it and no sitting in it is still warm, so the eviction this whole field
    /// exists to survive never takes one.
    fn working_for(&self, subject: &str) -> crate::core::Working {
        let Ok(mut map) = self.working.lock() else {
            return crate::core::Working::default();
        };
        map.retain(|s, w| s == subject || !w.is_idle());
        map.entry(subject.to_string()).or_default().clone()
    }

    /// Whether this is the first time this process has opened `subject`.
    /// Answers `true` exactly once per subject per process.
    fn claim_first_open(&self, subject: &str) -> bool {
        self.first_opened
            .lock()
            .map(|mut seen| seen.insert(subject.to_string()))
            .unwrap_or(false)
    }

    /// What used to happen at boot, for the one base there was.
    ///
    /// Both of these are about a *collection* rather than an endpoint, so
    /// neither can be answered once for the instance any more. They run here,
    /// lazily, rather than for every registered user at startup: a hundred
    /// users would otherwise mean a hundred full collection scrolls before the
    /// port opens.
    ///
    /// Two guards keep that promise, and the name of this function is the
    /// first of them. `open` is a cache *miss*, not a first sight: a registry
    /// at its cap reopens the tenant it evicted an hour ago, so without
    /// `claim_first_open` this would be per eviction rather than per process.
    /// The second is `trigger`. The repair ticker opens every registered user
    /// by name, and its first tick fires at boot — precisely the hundred
    /// scrolls this is written to avoid, arriving through the back door. Only
    /// a request scrolls; the ticker has `reconcile_stores_once` on its own
    /// much longer period for the same work, over the same users.
    ///
    /// On the background queue, not awaited. A first request must not wait out
    /// a scroll of the whole collection, and a base that cannot be reconciled
    /// is still a base its owner can read.
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

    /// `cached`, without the recency bump. For a caller that is passing
    /// through rather than serving somebody — see `get_transient`.
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
            Arc::new(Tenants::new(
                Arc::new(cfg),
                control,
                Arc::new(MemoryFactory),
            )),
            dir,
        )
    }

    /// A registry whose vector store will not open.
    ///
    /// What a Qdrant outage looks like from inside the process: the row is
    /// there, the file is there, and `open` fails anyway. Registered users are
    /// the caller's to provision through `control()`, because provisioning
    /// through the registry is one of the things that cannot work here.
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

    /// The sitting is what "eviction is transparent" has to mean to be worth
    /// saying: stored data was never at risk, and the working memory search and
    /// ask carry was, because `open` built a fresh `Core` on every cache miss.
    #[tokio::test]
    async fn an_evicted_tenant_comes_back_mid_sitting() {
        let (t, _dir) = test_tenants(1).await;
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        a.core.sittings.queried("sid-1", "how to mount", 100, 1800);

        // Somebody else's request takes the only slot.
        t.get_or_provision("sub-b", None).await.unwrap();
        assert_eq!(t.open_count(), 1);

        let again = t.get_or_provision("sub-a", None).await.unwrap();
        assert_eq!(
            again.core.sittings.read("sid-1", 200, 1800).queries,
            vec!["how to mount".to_string()],
            "the sitting went out with the tenant's place in the LRU"
        );
    }

    /// And the bound on that, so holding working memory across eviction is not
    /// a row per subject the process has ever served.
    ///
    /// A subject is dropped one open later than it becomes idle, and that is
    /// deliberate rather than missed: `working_for` runs while the registry
    /// still holds the tenant it is about to evict, so the first open after an
    /// eviction still sees the outgoing `Core` holding its own state. The next
    /// one sweeps it. The map is bounded either way, which is all this is for.
    #[tokio::test]
    async fn a_subject_with_nothing_warm_left_is_dropped_from_the_working_map() {
        let (t, _dir) = test_tenants(1).await;
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        a.core.sittings.queried("sid-1", "how to mount", 100, 1800);
        drop(a);

        t.get_or_provision("sub-b", None).await.unwrap();
        assert!(t.working_holds("sub-a"), "a live sitting is not idle");

        // The sitting goes cold. `read` is what expires it, as it would on any
        // request that carried it.
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

    /// `open` is a cache miss, not a first sight. Past the cap it runs again
    /// for a tenant this process has already opened — and what rides along
    /// with it is a scroll of the whole collection.
    #[tokio::test]
    async fn the_first_open_work_is_claimed_once_per_process_however_often_a_tenant_reopens() {
        let (t, _dir) = test_tenants(1).await;
        t.get_or_provision("sub-a", None).await.unwrap();
        // Evict it, then open it again.
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
