# Multi-user tenancy implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every user their own SQLite database and Qdrant collection, served by one instance-wide worker pool and one instance-wide `config.toml`.

**Architecture:** The data plane splits per tenant — one SQLite file and one Qdrant alias each. The compute plane stays unified: a new control database holds identity and a single job queue with a `subject` column, and `server.workers` remains the admission point in front of the shared inference endpoints. Handlers reach data through a `Tenant` extractor rather than a global `Core`.

**Tech Stack:** Rust 2024 (rust-version 1.94), axum 0.8, sqlx 0.9 (SQLite, WAL), Qdrant REST, tokio, argon2/openidconnect for auth, clap for the CLI.

**Spec:** `docs/superpowers/specs/2026-08-24-multi-user-tenancy-design.md`

## Global Constraints

- Rust edition 2024, `rust-version = "1.94"`. No new dependencies: `sha2`, `hex`, `sqlx`, `tokio`, `clap` are all already in `Cargo.toml`.
- All settings in `config.toml` are instance-wide. Nothing in this plan adds a per-user setting.
- New config keys, and only these: `store.control_path` (default `engram-control.db`), `store.dir` (default `data/users`), `store.max_open_tenants` (default 32), `migrate.adopt_subject` (default none), `schedule.backoff_max_hours` (default 24).
- `store.path` is retained and read by adoption alone.
- `src/store/schema.sql` is the per-tenant schema and stays a single `IF NOT EXISTS` statement of what the schema *is*, per the doctrine in `Store::migrate`. The new control schema follows the same rule in its own file.
- No admin role, no cross-user views, no per-user config, no quotas, no sharing between users.
- `meta` stays in the per-tenant schema. It holds sweep cursors (`EVENTS_AFTER`, `JUDGED_AFTER`, `PURSUIT_AFTER`); sharing it would corrupt them.
- Run `cargo test` before every commit. Run `cargo clippy --all-targets -- -D warnings` before the commit in every task.
- Commit messages: conventional prefix, present tense, ending with the trailer `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.

---

## File Structure

**Created:**
- `src/store/control_schema.sql` — control-plane schema: `users`, `sessions`, `api_tokens`, `jobs`.
- `src/store/control.rs` — `Control`: the control pool, the `users` table, and the job queue. One responsibility: everything that is about people or scheduling rather than knowledge.
- `src/tenants.rs` — `Tenants` registry: slug derivation, provisioning, the LRU of open `Core`s. Used by both the web layer and the workers, so it lives at the crate root rather than under `web/`.
- `src/web/tenant.rs` — the `Tenant` and `CanJudge` extractors. Web-only, so it stays under `web/`.
- `tests/multi_tenant.rs` — cross-tenant isolation over the real router.

**Modified:**
- `src/store/schema.sql` — `sessions`, `api_tokens` and `jobs` removed.
- `src/store/mod.rs` — `Store` gains `control: Control` and `subject: String`.
- `src/store/jobs.rs` — queries move to `impl Control` taking `subject`; `impl Store` keeps thin delegates.
- `src/store/auth.rs` — session and token queries move to `Control`.
- `src/web/state.rs` — `core` field removed, `tenants` added.
- `src/web/{api,ui,judge,insights,workspace,vbg,pair,corpus_view,lineage_view,extension,markdown}.rs` — `st.core` becomes `t.core`.
- `src/jobs/mod.rs` — worker claims globally, dispatches by subject; `rearm_periodic` gains backoff.
- `src/core/background.rs` — repair ticker iterates tenants.
- `src/main.rs` — control-only boot, adoption, new CLI flags.
- `src/config.rs` — the five new keys.
- `README.md`, `config.example.toml` — the operator-facing half.

---

### Task 1: The control database and the `users` table

**Files:**
- Create: `src/store/control_schema.sql`
- Create: `src/store/control.rs`
- Modify: `src/store/mod.rs:1-18` (add `pub mod control;`)
- Test: in `src/store/control.rs` under `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn slug_for(subject: &str) -> String`
  - `pub struct User { pub subject: String, pub email: Option<String>, pub slug: String, pub can_judge: bool, pub created_at: i64, pub last_seen_at: i64 }`
  - `pub struct Control { pub pool: sqlx::SqlitePool }`
  - `Control::connect(path: &str) -> Result<Control>`
  - `Control::memory() -> Result<Control>`
  - `Control::provision(&self, subject: &str, email: Option<&str>) -> Result<User>`
  - `Control::user(&self, subject: &str) -> Result<Option<User>>`
  - `Control::users(&self) -> Result<Vec<User>>`
  - `Control::set_can_judge(&self, subject: &str, on: bool) -> Result<bool>`
  - `Control::delete_user(&self, subject: &str) -> Result<bool>`
  - `Control::touch(&self, subject: &str) -> Result<()>`

- [ ] **Step 1: Write `src/store/control_schema.sql`**

Only the `users` table for now. Tasks 2 and 3 add the rest, so that each move is its own reviewable change.

```sql
-- The control plane: who exists, and what work is queued for them.
--
-- Separate from `schema.sql` because these tables are about people and
-- scheduling rather than knowledge. Every knowledge table lives in a
-- per-tenant database and never learns that other tenants exist.
CREATE TABLE IF NOT EXISTS users (
  subject      TEXT PRIMARY KEY,
  email        TEXT,
  -- Filesystem- and collection-safe tenant key. Derived once from `subject`
  -- and stored, not recomputed: an OIDC subject may contain anything, an
  -- email can change, and the mapping has to survive a change to how the
  -- derivation works.
  slug         TEXT NOT NULL UNIQUE,
  -- Whether this user may reach /ui/judge, which is also the only route that
  -- writes config.toml. Granted out of band with `engram --grant-judge`.
  can_judge    INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
);
```

- [ ] **Step 2: Write the failing tests**

Create `src/store/control.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_stable_hex_and_safe_in_a_filename() {
        let a = slug_for("https://idp.example/sub|1234");
        assert_eq!(a, slug_for("https://idp.example/sub|1234"));
        assert_ne!(a, slug_for("https://idp.example/sub|1235"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn provisioning_twice_makes_one_user() {
        let c = Control::memory().await.unwrap();
        let first = c.provision("sub-1", Some("a@example.org")).await.unwrap();
        let again = c.provision("sub-1", Some("a@example.org")).await.unwrap();
        assert_eq!(first.slug, again.slug);
        assert_eq!(first.created_at, again.created_at);
        assert_eq!(c.users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_new_user_may_not_judge() {
        let c = Control::memory().await.unwrap();
        assert!(!c.provision("sub-1", None).await.unwrap().can_judge);
    }

    #[tokio::test]
    async fn granting_and_revoking_judge_is_visible_immediately() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        assert!(c.set_can_judge("sub-1", true).await.unwrap());
        assert!(c.user("sub-1").await.unwrap().unwrap().can_judge);
        assert!(c.set_can_judge("sub-1", false).await.unwrap());
        assert!(!c.user("sub-1").await.unwrap().unwrap().can_judge);
    }

    #[tokio::test]
    async fn granting_to_an_unknown_subject_says_so_rather_than_inventing_one() {
        let c = Control::memory().await.unwrap();
        assert!(!c.set_can_judge("nobody", true).await.unwrap());
        assert!(c.users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_user_removes_the_row() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        assert!(c.delete_user("sub-1").await.unwrap());
        assert!(c.user("sub-1").await.unwrap().is_none());
        assert!(!c.delete_user("sub-1").await.unwrap());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib store::control`
Expected: FAIL to compile — `cannot find function slug_for`, `cannot find type Control`.

- [ ] **Step 4: Write the implementation**

Above the test module in `src/store/control.rs`:

```rust
//! The control plane: identity and scheduling.
//!
//! One database for the whole instance, holding what is about people rather
//! than knowledge. Every knowledge table lives in a per-tenant database that
//! never learns other tenants exist, which is what makes isolation structural:
//! there is no query anywhere that could be written without a tenant filter,
//! because no tenant filter exists.

use crate::error::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

/// The tenant key: a hex SHA-256 prefix of the OIDC subject.
///
/// Not derived from the email, which can change, and not the subject itself,
/// which may contain anything at all — including characters that are neither a
/// legal filename nor a legal Qdrant collection name. Sixteen hex digits is 64
/// bits: this is a naming scheme, not a secret, and the `UNIQUE` on `slug`
/// turns the collision nobody will see into an error rather than a silently
/// shared database.
pub fn slug_for(subject: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(subject.as_bytes());
    hex::encode(&digest[..8])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub subject: String,
    pub email: Option<String>,
    pub slug: String,
    pub can_judge: bool,
    pub created_at: i64,
    pub last_seen_at: i64,
}

impl User {
    fn from_row(r: &sqlx::sqlite::SqliteRow) -> User {
        User {
            subject: r.get("subject"),
            email: r.get("email"),
            slug: r.get("slug"),
            can_judge: r.get::<i64, _>("can_judge") != 0,
            created_at: r.get("created_at"),
            last_seen_at: r.get("last_seen_at"),
        }
    }
}

#[derive(Clone)]
pub struct Control {
    pub pool: sqlx::SqlitePool,
}

impl Control {
    pub async fn connect(path: &str) -> Result<Control> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        let control = Control { pool };
        control.migrate().await?;
        Ok(control)
    }

    /// Fresh in-memory control database, for the tests. One connection, for
    /// the reason `Store::memory` gives: every `sqlite::memory:` connection is
    /// a separate database.
    pub async fn memory() -> Result<Control> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let control = Control { pool };
        control.migrate().await?;
        Ok(control)
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("control_schema.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Idempotent by construction. Two concurrent first requests for the same
    /// unseen subject both run this; `INSERT OR IGNORE` means one row, and the
    /// `SELECT` afterwards means both callers get it.
    pub async fn provision(&self, subject: &str, email: Option<&str>) -> Result<User> {
        let now = super::now();
        sqlx::query(
            "INSERT OR IGNORE INTO users (subject, email, slug, can_judge, created_at, last_seen_at)
             VALUES (?, ?, ?, 0, ?, ?)",
        )
        .bind(subject)
        .bind(email)
        .bind(slug_for(subject))
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.user(subject)
            .await?
            .ok_or_else(|| crate::error::Error::Store(format!("could not provision `{subject}`")))
    }

    pub async fn user(&self, subject: &str) -> Result<Option<User>> {
        Ok(sqlx::query("SELECT * FROM users WHERE subject = ?")
            .bind(subject)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| User::from_row(&r)))
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        Ok(sqlx::query("SELECT * FROM users ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(User::from_row)
            .collect())
    }

    /// `false` when no such subject — the grant CLI says so rather than
    /// reporting success on a typo'd subject nobody will ever log in as.
    pub async fn set_can_judge(&self, subject: &str, on: bool) -> Result<bool> {
        Ok(sqlx::query("UPDATE users SET can_judge = ? WHERE subject = ?")
            .bind(i64::from(on))
            .bind(subject)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    pub async fn delete_user(&self, subject: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM users WHERE subject = ?")
            .bind(subject)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Last seen, for `--list-users`. Not a policy input: dormancy is handled
    /// by the sweeps backing off, not by a cutoff on this column.
    pub async fn touch(&self, subject: &str) -> Result<()> {
        sqlx::query("UPDATE users SET last_seen_at = ? WHERE subject = ?")
            .bind(super::now())
            .bind(subject)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
```

Add `pub mod control;` to the module list at the top of `src/store/mod.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib store::control`
Expected: PASS, six tests.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add src/store/control.rs src/store/control_schema.sql src/store/mod.rs
git commit -m "feat(store): a control database with a users table

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Move sessions and API tokens into the control database

**Files:**
- Modify: `src/store/control_schema.sql` (add `sessions`, `api_tokens`)
- Modify: `src/store/schema.sql:623-645` (remove `sessions`, `api_tokens`)
- Modify: `src/store/auth.rs` (queries move to `impl Control`)
- Modify: `src/store/mod.rs` (`Store` gains `control`)
- Modify: `src/auth/mod.rs:74-90`, `src/auth/tokens.rs` (call through `Control`)
- Test: existing tests in `src/store/auth.rs`, `src/auth/tokens.rs`

**Interfaces:**
- Consumes: `Control` from Task 1.
- Produces:
  - `Store { pub control: Control, .. }` — every `Store` carries the control handle.
  - `Store::connect(cfg: &StoreConfig, control: Control) -> Result<Store>` — Task 3 adds the `subject` argument, so expect this signature to change once more
  - `Store::memory()` unchanged in signature: it builds its own in-memory `Control` and a fixed test subject.
  - Session and token methods move verbatim from `impl Store` to `impl Control`, same names, same signatures.

- [ ] **Step 1: Copy the two table definitions into the control schema**

Move the `sessions` and `api_tokens` blocks from `src/store/schema.sql:623-645` into `src/store/control_schema.sql` unchanged, comments included, and delete them from `schema.sql`.

- [ ] **Step 2: Write the failing test**

In `src/store/control.rs` tests:

```rust
#[tokio::test]
async fn a_session_survives_in_the_control_database() {
    let c = Control::memory().await.unwrap();
    c.put_session("sid-1", "sub-1", Some("a@example.org"), 3600)
        .await
        .unwrap();
    let s = c.get_session("sid-1").await.unwrap().expect("session");
    assert_eq!(s.subject, "sub-1");
}

#[tokio::test]
async fn a_tenant_store_carries_the_control_handle() {
    let store = crate::store::Store::memory().await.unwrap();
    store
        .control
        .provision("sub-1", None)
        .await
        .unwrap();
    assert_eq!(store.control.users().await.unwrap().len(), 1);
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib store::control`
Expected: FAIL — `no method named put_session found for struct Control`, `no field control on struct Store`.

- [ ] **Step 4: Move the queries and add the field**

In `src/store/auth.rs`, change each `impl Store` block to `impl Control`. The bodies are unchanged: they already use `&self.pool`, and `Control` has a `pool` field of the same type.

In `src/store/mod.rs`, add the field and thread it through both constructors:

```rust
#[derive(Clone)]
pub struct Store {
    pub pool: sqlx::SqlitePool,
    /// The instance-wide control database: identity, and the job queue.
    ///
    /// Held by every tenant `Store` rather than reached through the registry,
    /// because the enqueue paths are deep inside capture and must not have to
    /// carry a second handle down with them.
    pub control: control::Control,
    capture: std::sync::Arc<tokio::sync::Mutex<()>>,
}
```

`Store::connect` takes the control handle from its caller:

```rust
    pub async fn connect(cfg: &StoreConfig, control: control::Control) -> Result<Store> {
        // ...opts unchanged...
        let pool = SqlitePoolOptions::new()
            // Four rather than eight: a hundred open tenants at eight
            // connections each is a file-descriptor problem, and no single
            // tenant needs eight.
            .max_connections(4)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            control,
            capture: Default::default(),
        };
        store.migrate().await?;
        Ok(store)
    }
```

`Store::memory` keeps its signature — this is the seam that keeps all 264 existing call sites compiling:

```rust
    /// Fresh in-memory database with the schema applied, for the tests.
    ///
    /// Builds its own in-memory control database too, so a test that only
    /// wants a `Store` does not have to know that a control plane exists.
    pub async fn memory() -> Result<Store> {
        Store::memory_with(control::Control::memory().await?).await
    }

    /// The same, over a control database the caller already has — for tests
    /// that need two tenants sharing one queue.
    pub async fn memory_with(control: control::Control) -> Result<Store> {
        // ...existing body, with `control` in the struct literal...
    }
```

Update `src/auth/mod.rs:79-86` and `src/auth/tokens.rs` to call `state.core.store.control.get_session(..)` / `.extend_session(..)` and the token equivalents.

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: PASS. Fix any call site the compiler names; they are all `store.get_session` → `store.control.get_session` and the same for `put_session`, `extend_session`, `delete_session`, `purge_expired_sessions`, and the token methods.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add -A src/store src/auth
git commit -m "refactor(store): sessions and tokens move to the control database

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: One job queue, keyed by subject

**Files:**
- Modify: `src/store/control_schema.sql` (add `jobs` with `subject`)
- Modify: `src/store/schema.sql:212-257` (remove `jobs` and its indexes)
- Modify: `src/store/jobs.rs` (queries to `impl Control`, delegates on `Store`)
- Modify: `src/store/mod.rs` (`Store` gains `subject`)
- Test: the existing test module in `src/store/jobs.rs`

**Interfaces:**
- Consumes: `Control`, `Store::memory_with` from Task 2.
- Produces:
  - `Store { pub subject: String, .. }`, set to `"test-subject"` by `Store::memory`.
  - On `Control`, every former `Store` job method with `subject: &str` as its first argument: `enqueue`, `enqueue_seq`, `arm_periodic`, `arm_now`, `live_job`, `complete_job`, `fail_job`, `reclaim_stuck`, `age_background`.
  - `Control::claim_job(&self) -> Result<Option<(String, Job)>>` — instance-wide, returning the subject alongside the job.
  - On `Store`, the same method names with the original signatures, delegating with `&self.subject`. Every existing call site is untouched.
  - `pub(crate) async fn enqueue_with<'e>(exec, subject: &str, stage, target_kind, target_id)`.

- [ ] **Step 1: Add the table to the control schema, remove it from the tenant schema**

```sql
CREATE TABLE IF NOT EXISTS jobs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Whose work this is. The queue is instance-wide because the inference
  -- endpoints are: `server.workers` is the admission point in front of a
  -- single GPU, and it must stay one number however many people sign up.
  subject     TEXT NOT NULL REFERENCES users(subject) ON DELETE CASCADE,
  stage       TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending',
  attempts    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER NOT NULL DEFAULT 0,
  last_error  TEXT,
  claimed_at  INTEGER,
  created_at  INTEGER NOT NULL DEFAULT 0,
  seq         INTEGER NOT NULL DEFAULT 0,
  class       INTEGER NOT NULL DEFAULT 0,
  -- How many consecutive runs of this periodic unit found nothing to do. Read
  -- by `rearm_periodic` to widen the wait; reset by any run that did work.
  empty_runs  INTEGER NOT NULL DEFAULT 0,
  UNIQUE(subject, stage, target_id)
);
-- Claim order is unchanged from the single-user index, and `subject` is
-- deliberately not in it: claiming is instance-wide, and `seq` already
-- interleaves batches, so one user's ingest cannot drain ahead of another's.
CREATE INDEX IF NOT EXISTS idx_jobs_claim3  ON jobs(state, class, attempts, seq, id, run_after);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);
```

Delete `jobs`, `idx_jobs_claim3`, `idx_jobs_created` and the `DROP INDEX IF EXISTS idx_jobs_claim2` line from `src/store/schema.sql`. Keep `sweep_runs` where it is: it is per-tenant history.

- [ ] **Step 2: Write the failing tests**

In the test module of `src/store/jobs.rs`:

```rust
#[tokio::test]
async fn two_tenants_do_not_see_each_others_jobs() {
    let control = crate::store::control::Control::memory().await.unwrap();
    control.provision("sub-a", None).await.unwrap();
    control.provision("sub-b", None).await.unwrap();
    let a = crate::store::Store::memory_with(control.clone()).await.unwrap();
    let a = a.for_subject("sub-a");
    let b = crate::store::Store::memory_with(control.clone()).await.unwrap();
    let b = b.for_subject("sub-b");

    a.enqueue(Stage::Embed, "corpus", "shared-id").await.unwrap();
    assert!(a.live_job(Stage::Embed, "shared-id").await.unwrap());
    assert!(!b.live_job(Stage::Embed, "shared-id").await.unwrap());
}

#[tokio::test]
async fn the_same_target_id_in_two_tenants_is_two_jobs() {
    let control = crate::store::control::Control::memory().await.unwrap();
    control.provision("sub-a", None).await.unwrap();
    control.provision("sub-b", None).await.unwrap();
    let a = crate::store::Store::memory_with(control.clone()).await.unwrap().for_subject("sub-a");
    let b = crate::store::Store::memory_with(control.clone()).await.unwrap().for_subject("sub-b");

    a.enqueue(Stage::Embed, "corpus", "same").await.unwrap();
    b.enqueue(Stage::Embed, "corpus", "same").await.unwrap();

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&control.pool)
        .await
        .unwrap();
    assert_eq!(n, 2, "UNIQUE is per subject, not global");
}

#[tokio::test]
async fn claiming_says_whose_job_it_is() {
    let control = crate::store::control::Control::memory().await.unwrap();
    control.provision("sub-a", None).await.unwrap();
    let a = crate::store::Store::memory_with(control.clone()).await.unwrap().for_subject("sub-a");
    a.enqueue(Stage::Embed, "corpus", "c1").await.unwrap();

    let (subject, job) = control.claim_job().await.unwrap().expect("a job");
    assert_eq!(subject, "sub-a");
    assert_eq!(job.target_id, "c1");
}

#[tokio::test]
async fn deleting_a_user_takes_their_queue_with_them() {
    let control = crate::store::control::Control::memory().await.unwrap();
    control.provision("sub-a", None).await.unwrap();
    let a = crate::store::Store::memory_with(control.clone()).await.unwrap().for_subject("sub-a");
    a.enqueue(Stage::Embed, "corpus", "c1").await.unwrap();

    control.delete_user("sub-a").await.unwrap();
    assert!(control.claim_job().await.unwrap().is_none());
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --lib store::jobs`
Expected: FAIL — `no method named for_subject`, `no method named claim_job found for struct Control`.

- [ ] **Step 4: Move the queries**

In `src/store/mod.rs`, add the field and the builder:

```rust
pub struct Store {
    pub pool: sqlx::SqlitePool,
    pub control: control::Control,
    /// Whose database this is. Bound into every queue query, and the only
    /// place a tenant identity appears below the web layer.
    pub subject: String,
    capture: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Store {
    /// The same database under a different subject. Used by the registry when
    /// it opens a tenant, and by the tests.
    pub fn for_subject(&self, subject: &str) -> Store {
        Store { subject: subject.to_string(), ..self.clone() }
    }
}
```

`Store::connect` takes `subject: &str` and stores it. `Store::memory` uses `"test-subject"` and provisions it, so that the foreign key on `jobs.subject` is satisfied in every existing test:

```rust
    pub async fn memory_with(control: control::Control) -> Result<Store> {
        // ...pool as before...
        control.provision(TEST_SUBJECT, None).await?;
        let store = Store {
            pool,
            control,
            subject: TEST_SUBJECT.to_string(),
            capture: Default::default(),
        };
        store.migrate().await?;
        Ok(store)
    }
```

with `pub const TEST_SUBJECT: &str = "test-subject";` beside it.

In `src/store/jobs.rs`: change each `impl Store` block to `impl Control`, give every method `subject: &str` as its first parameter, and add `AND subject = ?` (or a `subject` column and bind, for the inserts) to every statement. `arm_job!` becomes `ON CONFLICT(subject, stage, target_id)`. `claim_job` stays without a subject filter and returns the subject:

```rust
    /// Instance-wide. The claim order is unchanged: `seq` already interleaves
    /// batches, so one tenant's ingest cannot drain ahead of another's, and no
    /// per-user weighting is needed to make that true.
    pub async fn claim_job(&self) -> Result<Option<(String, Job)>> {
        let row = sqlx::query(
            "UPDATE jobs
                SET state = 'running', claimed_at = ?, attempts = attempts + 1
              WHERE id = (
                SELECT id FROM jobs
                 WHERE state = 'pending' AND run_after <= ?
                 ORDER BY class, attempts, seq, id
                 LIMIT 1
              )
              RETURNING *",
        )
        // ...binds as before...
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get("subject"), Job::from_row(&r))))
    }
```

Then add the delegates at the bottom of `src/store/jobs.rs`, one per moved method, so that no existing call site changes:

```rust
/// Every job method, as it was, bound to this store's subject.
///
/// The queue lives in the control database now, but the callers are deep
/// inside capture and search and have no business knowing that. They keep
/// calling `store.enqueue(..)`; the subject comes from the store they are
/// already holding.
impl Store {
    pub async fn enqueue(&self, stage: Stage, target_kind: &str, target_id: &str) -> Result<()> {
        self.control.enqueue(&self.subject, stage, target_kind, target_id).await
    }

    pub async fn enqueue_seq(&self, stage: Stage, target_kind: &str, target_id: &str, seq: i64) -> Result<()> {
        self.control.enqueue_seq(&self.subject, stage, target_kind, target_id, seq).await
    }

    pub async fn arm_periodic(&self, stage: Stage, target_kind: &str, target_id: &str, run_after: i64) -> Result<()> {
        self.control.arm_periodic(&self.subject, stage, target_kind, target_id, run_after).await
    }

    pub async fn arm_now(&self, stage: Stage, target_kind: &str, target_id: &str) -> Result<()> {
        self.control.arm_now(&self.subject, stage, target_kind, target_id).await
    }

    pub async fn live_job(&self, stage: Stage, target_id: &str) -> Result<bool> {
        self.control.live_job(&self.subject, stage, target_id).await
    }

    pub async fn complete_job(&self, id: i64) -> Result<()> {
        self.control.complete_job(id).await
    }
}
```

Repeat for `fail_job`, `reclaim_stuck`, `age_background`, and any other method the compiler names. `complete_job` and `fail_job` take a row id, which is already unique instance-wide, so they need no subject.

The three call sites at `src/store/corpora.rs:316,373,592` change to pass the subject:

```rust
            super::jobs::enqueue_with(&mut *tx, &self.subject, stage, "corpus", &src.id).await?;
```

and `enqueue_with` now writes to the control pool rather than the passed executor — take the executor argument away and call `self.control` instead. **Note in the commit message:** this is the atomicity the spec records as given up. `src/jobs/reconcile.rs` covers it.

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add -A src/store
git commit -m "feat(store): one job queue in the control database, keyed by subject

The queue is instance-wide because the inference endpoints are:
server.workers is the admission point in front of one GPU and must stay
one number however many users sign up. Claim order is unchanged, so seq
keeps interleaving batches across tenants.

Capture's enqueue no longer rides inside the capture transaction, since
SQLite makes no atomicity promise across two databases in WAL mode. A
crash in that window leaves a corpus with no job, which is the case
jobs/reconcile.rs already exists for.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: The tenant registry

**Files:**
- Create: `src/tenants.rs`
- Modify: `src/lib.rs` (add `pub mod tenants;`)
- Modify: `src/config.rs:650-652` (`StoreConfig` gains three keys)
- Test: in `src/tenants.rs` under `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `Control`, `User`, `slug_for` (Task 1); `Store::connect` (Tasks 2-3).
- Produces:
  - `pub struct Tenants` with `Tenants::new(cfg: Arc<Config>, control: Control) -> Tenants`
  - `Tenants::get_or_provision(&self, subject: &str, email: Option<&str>) -> Result<Tenant>`
  - `Tenants::get(&self, subject: &str) -> Result<Tenant>` — for the workers, which have a subject off a claimed row and must not provision from it
  - `Tenants::single(core: Core, user: User) -> Tenants` — a fixed one-tenant registry for the tests
  - `pub struct Tenant { pub core: Core, pub user: User }`, `#[derive(Clone)]`

- [ ] **Step 1: Add the config keys**

```rust
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct StoreConfig {
    /// The single-user database. Read by adoption alone, and meaningless once
    /// the `users` table is non-empty.
    pub path: String,
    /// The instance-wide control database: identity and the job queue.
    pub control_path: String,
    /// Where per-tenant databases live, one `{slug}.db` per user.
    pub dir: String,
    /// How many tenants may be open at once. An open tenant costs a SQLite
    /// pool and a background queue; the rest are opened on demand.
    pub max_open_tenants: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: "engram.db".into(),
            control_path: "engram-control.db".into(),
            dir: "data/users".into(),
            max_open_tenants: 32,
        }
    }
}
```

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_first_request_provisions_and_a_second_reuses() {
        let t = test_tenants().await;
        let a = t.get_or_provision("sub-1", None).await.unwrap();
        let b = t.get_or_provision("sub-1", None).await.unwrap();
        assert_eq!(a.user.slug, b.user.slug);
        assert_eq!(t.open_count(), 1, "the second request reused the open core");
    }

    #[tokio::test]
    async fn racing_first_requests_provision_once() {
        let t = std::sync::Arc::new(test_tenants().await);
        let (one, two) = tokio::join!(
            { let t = t.clone(); async move { t.get_or_provision("sub-1", None).await } },
            { let t = t.clone(); async move { t.get_or_provision("sub-1", None).await } },
        );
        assert_eq!(one.unwrap().user.slug, two.unwrap().user.slug);
        assert_eq!(t.control().users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_subjects_get_two_slugs_two_files_and_two_aliases() {
        let t = test_tenants().await;
        let a = t.get_or_provision("sub-a", None).await.unwrap();
        let b = t.get_or_provision("sub-b", None).await.unwrap();
        assert_ne!(a.user.slug, b.user.slug);
        assert!(t.db_path(&a.user).exists());
        assert!(t.db_path(&b.user).exists());
        assert_ne!(t.alias(&a.user), t.alias(&b.user));
    }

    #[tokio::test]
    async fn the_worker_path_refuses_to_provision_from_a_queue_row() {
        let t = test_tenants().await;
        assert!(t.get("never-seen").await.is_err());
    }

    #[tokio::test]
    async fn opening_past_the_cap_evicts_the_least_recently_used() {
        let t = test_tenants_with_cap(2).await;
        t.get_or_provision("sub-a", None).await.unwrap();
        t.get_or_provision("sub-b", None).await.unwrap();
        t.get_or_provision("sub-c", None).await.unwrap();
        assert_eq!(t.open_count(), 2);
        // Reopening is transparent: the same slug, the same file.
        let a_again = t.get_or_provision("sub-a", None).await.unwrap();
        assert_eq!(a_again.user.slug, super::super::store::control::slug_for("sub-a"));
    }
}
```

Write `test_tenants()` and `test_tenants_with_cap(n)` as helpers in the same module: a `tempfile::TempDir` for `store.dir`, `Control::memory()`, and a config built from `crate::core::test_support` defaults with `MemoryVectors` in place of Qdrant.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --lib tenants`
Expected: FAIL to compile — `cannot find type Tenants`.

- [ ] **Step 4: Write the registry**

```rust
//! The tenant registry: subject in, `Core` out.
//!
//! Provisioning is five steps and every one of them is idempotent, because a
//! crash part-way through has to be recoverable by logging in again rather
//! than by an operator with a shell. It is deliberately *not* transactional:
//! three systems are involved — the control database, a file, and Qdrant — and
//! nothing can span them. A Qdrant outage during a first login must therefore
//! fail loudly at the door, since half-provisioning presents to the user as a
//! base that returns empty searches.

use crate::config::Config;
use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::store::control::{Control, User};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Tenant {
    pub core: Core,
    pub user: User,
}

pub struct Tenants {
    cfg: Arc<Config>,
    control: Control,
    /// Open cores, and the order they were last used in. A plain map plus a
    /// recency vector rather than an LRU crate: the cap is in the tens, and a
    /// dependency for a linear scan over thirty-two entries is not a trade.
    open: Mutex<(HashMap<String, Tenant>, Vec<String>)>,
    /// One provisioning at a time per subject, so two first requests racing
    /// cannot both create the collection. `INSERT OR IGNORE` makes the row
    /// safe on its own; this is what makes the Qdrant call safe.
    provisioning: tokio::sync::Mutex<()>,
}

impl Tenants {
    pub fn new(cfg: Arc<Config>, control: Control) -> Tenants {
        Tenants {
            cfg,
            control,
            open: Mutex::new((HashMap::new(), Vec::new())),
            provisioning: tokio::sync::Mutex::new(()),
        }
    }

    pub fn control(&self) -> &Control {
        &self.control
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
        // while this one waited.
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self.control.provision(subject, email).await?;
        let tenant = self.open(user).await?;
        self.remember(tenant.clone());
        Ok(tenant)
    }

    /// The worker door: a subject read off a claimed queue row. Never
    /// provisions — a subject that is not in `users` is a bug or a deleted
    /// user, and inventing a tenant for it would create a database nobody
    /// asked for.
    pub async fn get(&self, subject: &str) -> Result<Tenant> {
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let _guard = self.provisioning.lock().await;
        if let Some(t) = self.cached(subject) {
            return Ok(t);
        }
        let user = self
            .control
            .user(subject)
            .await?
            .ok_or(Error::NotFound)?;
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

        let mut vector_cfg = self.cfg.vector.clone();
        vector_cfg.collection = self.alias(&user);
        let vectors: Arc<dyn crate::vector::VectorStore> =
            Arc::new(crate::vector::qdrant::QdrantVectors::connect(&vector_cfg).await?);
        vectors.ensure_collection(self.cfg.infer.embed.dim).await?;

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
            let Some(oldest) = order.first().cloned() else { break };
            order.remove(0);
            map.remove(&oldest);
        }
    }
}
```

Add `pub mod tenants;` to `src/lib.rs`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib tenants`
Expected: PASS, five tests.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add src/tenants.rs src/lib.rs src/config.rs
git commit -m "feat: a tenant registry, one core per user

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The `Tenant` extractor, and `core` leaves `AppState`

This is the largest task by line count and the smallest by decision count: once the extractor exists, the rest is a substitution the compiler drives.

**Files:**
- Create: `src/web/tenant.rs`
- Modify: `src/web/state.rs:24-45`
- Modify: `src/web/test_support.rs:10-23`
- Modify: every file in `src/web/` that names `st.core` (187 sites), and `src/mcp/mod.rs:284`
- Test: `src/web/tenant.rs` tests plus the whole existing `web` suite

**Interfaces:**
- Consumes: `Tenants`, `Tenant` (Task 4).
- Produces:
  - `impl FromRequestParts<AppState> for Tenant`
  - `AppState { pub tenants: Arc<Tenants>, pub auth, pub config, pub config_path, pub ask_handoff }` — no `core`
  - `test_support::router(core: Core, local: Option<LocalConfig>) -> Router` — unchanged signature

- [ ] **Step 1: Write the failing test**

In `src/web/tenant.rs`:

```rust
#[cfg(test)]
mod tests {
    use crate::web::test_support::{get, router};

    #[tokio::test]
    async fn an_unauthenticated_request_is_still_a_401_before_any_tenant_is_touched() {
        let core = crate::core::test_support::test_core().await;
        let app = router(core, Some(crate::config::LocalConfig {
            username: "dev".into(),
            password_hash: "$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aaaa".into(),
        }));
        let res = get(&app, "/api/corpora").await;
        assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib web::tenant`
Expected: FAIL to compile — no module `tenant`.

- [ ] **Step 3: Write the extractor**

```rust
//! Turning an authenticated request into the data it is allowed to see.
//!
//! Runs after `Identity`, so an unauthenticated request fails in exactly the
//! place it failed before — with the same 401, which the redirect middleware
//! in `web/mod.rs` still rewrites for a browser.

use crate::auth::Identity;
use crate::error::Error;
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

impl FromRequestParts<AppState> for Tenant {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let id = Identity::from_request_parts(parts, state).await?;
        let tenant = state
            .tenants
            .get_or_provision(&id.subject, id.email.as_deref())
            .await?;
        Ok(tenant)
    }
}

/// A tenant whose user may reach the judge — which is also the only door in
/// the tree that writes `config.toml`.
///
/// Named by every judge handler in place of `Tenant`, so a route added to that
/// router later without it does not compile against the pattern its neighbours
/// use, rather than silently opening.
pub struct CanJudge(pub Tenant);

impl FromRequestParts<AppState> for CanJudge {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let t = Tenant::from_request_parts(parts, state).await?;
        if !t.user.can_judge {
            return Err(Error::Forbidden);
        }
        Ok(CanJudge(t))
    }
}
```

- [ ] **Step 4: Remove `core` from `AppState`**

```rust
#[derive(Clone)]
pub struct AppState {
    pub tenants: Arc<crate::tenants::Tenants>,
    pub auth: Arc<AuthContext>,
    /// The instance-wide settings, which every tenant shares.
    pub config: Arc<crate::config::Config>,
    pub config_path: Arc<std::path::PathBuf>,
    pub ask_handoff: Arc<Mutex<HashMap<String, ParkedAsk>>>,
}
```

`ask_enabled` and `judge_pending` take what they need rather than reaching through state:

```rust
pub fn ask_enabled(t: &crate::tenants::Tenant) -> bool {
    t.core.asks()
}

/// `None` when there is nothing to show — learning off, or this user may not
/// judge. An ungranted user is not shown a door they cannot open.
pub async fn judge_pending(t: &crate::tenants::Tenant) -> Option<i64> {
    if !t.core.learn.enabled || !t.user.can_judge {
        return None;
    }
    match t.core.store.pending_count().await {
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!(error = %e, "could not count searches waiting to be judged");
            None
        }
    }
}
```

- [ ] **Step 5: Fix the 187 sites the compiler names**

Run: `cargo build 2>&1 | grep "no field .core" | wc -l` to see the count come down.

The edit is mechanical and has exactly two forms:

```rust
// before
async fn corpora(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    let list = st.core.store.list_corpora().await?;

// after
async fn corpora(t: Tenant) -> Result<Response> {
    let list = t.core.store.list_corpora().await?;
```

and for the 18 helpers taking `&AppState`, add `t: &Tenant` and keep `st` only where the body reads `st.config`, `st.config_path`, `st.auth` or `st.ask_handoff`.

**Do not** clone a `Core` out of a `Tenant` into anything that outlives the request. The isolation test in Task 10 is the backstop, but the rule is the defence.

- [ ] **Step 6: Update the test seam**

```rust
/// The real router over `core`, in local auth mode with no password
/// configured (`local`); pass `Some(cfg)` to test the login form itself.
///
/// Builds a one-tenant registry around the passed core, so every test written
/// against the single-user app keeps working unchanged. If tenancy ever needs
/// edits scattered across the web tests, the extractor boundary is in the
/// wrong place and this is where that shows.
pub fn router(core: Core, local: Option<crate::config::LocalConfig>) -> axum::Router {
    let user = crate::store::control::User {
        subject: crate::store::TEST_SUBJECT.into(),
        email: None,
        slug: crate::store::control::slug_for(crate::store::TEST_SUBJECT),
        can_judge: true,
        created_at: 0,
        last_seen_at: 0,
    };
    let tenants = crate::tenants::Tenants::single(core, user);
    crate::web::router(crate::web::state::AppState {
        tenants: std::sync::Arc::new(tenants),
        auth: std::sync::Arc::new(crate::web::state::AuthContext { /* as before */ }),
        config: std::sync::Arc::new(crate::config::Config::test_default()),
        config_path: std::sync::Arc::new(scratch_config()),
        ask_handoff: Default::default(),
    })
}
```

Add `Tenants::single(core: Core, user: User) -> Tenants` to `src/tenants.rs`: it puts the pair straight into `open` and returns `Err(Error::NotFound)` from `open()` if anything asks for a different subject. Move the `test_config()` body from `src/main.rs:322-...` into `Config::test_default()` behind `#[cfg(test)]` in `src/config.rs`, and have the startup tests call it.

`can_judge: true` in the fixture, so that the existing judge tests keep passing. Task 6 adds the negative case.

- [ ] **Step 7: Run the full suite**

Run: `cargo test`
Expected: PASS. Every failure here is a call site, not a design problem.

- [ ] **Step 8: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add -A src/web src/mcp src/tenants.rs src/config.rs
git commit -m "feat(web): handlers reach data through a Tenant, not a global Core

Removing the field rather than leaving it in place is the point: the
compiler enumerates all 187 call sites, so no handler can quietly keep
talking to a core that belongs to nobody.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The judge gate

**Files:**
- Modify: `src/web/judge.rs:921-931` (handlers take `CanJudge`)
- Modify: `src/web/ui.rs` (nav, via `judge_pending`)
- Test: `src/web/judge.rs` tests

**Interfaces:**
- Consumes: `CanJudge` (Task 5).
- Produces: no new API. Every handler in `judge_router` takes `CanJudge(t): CanJudge`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_ungranted_user_is_refused_at_every_judge_route() {
    let app = crate::web::test_support::router_ungranted(
        crate::core::test_support::test_core().await,
        None,
    );
    for path in [
        "/ui/judge",
        "/ui/judge/next",
        "/ui/judge/read/a1",
    ] {
        let res = crate::web::test_support::get(&app, path).await;
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN, "{path}");
    }
    for path in [
        "/ui/judge/tune/r1/apply",
        "/ui/judge/j1/hit",
        "/ui/judge/j1/gap",
        "/ui/judge/j1/discard",
        "/ui/judge/j1/skip",
        "/ui/judge/j1/undo",
    ] {
        let res = crate::web::test_support::post(&app, path, "").await;
        assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn an_ungranted_user_gets_no_judge_entry_in_the_nav() {
    let core = crate::core::test_support::test_core().await;
    let app = crate::web::test_support::router_ungranted(core, None);
    let body = crate::web::test_support::body(crate::web::test_support::get(&app, "/ui/search").await).await;
    assert!(!body.contains("/ui/judge"), "an ungranted user was shown the door");
}

#[tokio::test]
async fn the_config_writing_route_is_behind_the_same_gate() {
    let app = crate::web::test_support::router_ungranted(
        crate::core::test_support::test_core().await,
        None,
    );
    let res = crate::web::test_support::post(&app, "/ui/judge/tune/r1/apply", "").await;
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
}
```

Add `router_ungranted` to `test_support.rs`: identical to `router` with `can_judge: false`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::judge`
Expected: FAIL — routes answer 200, and `router_ungranted` does not exist.

- [ ] **Step 3: Swap the extractor in every judge handler**

For each of the eleven handlers, `t: Tenant` becomes `CanJudge(t): CanJudge`. The bodies do not change.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::judge`
Expected: PASS, including the existing judge tests, which run under the granted fixture.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add src/web/judge.rs src/web/test_support.rs src/web/ui.rs
git commit -m "feat(web): the judge, and the config write behind it, are granted per user

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Workers claim globally

**Files:**
- Modify: `src/jobs/mod.rs:391-421` (`Worker::spawn`), `:60-140` (`run_one`)
- Modify: `src/core/background.rs:254-...` (repair ticker)
- Test: `src/jobs/mod.rs` tests

**Interfaces:**
- Consumes: `Control::claim_job` (Task 3), `Tenants::get` (Task 4).
- Produces:
  - `Worker::spawn(tenants: Arc<Tenants>, workers: usize, shutdown) -> Vec<JoinHandle<()>>`
  - `async fn run_one(tenants: &Tenants) -> Result<bool>`
  - `spawn_repair_ticker(tenants: Arc<Tenants>, shutdown) -> JoinHandle<()>`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_worker_runs_each_job_against_its_own_tenants_core() {
    let (tenants, a, b) = crate::tenants::test_support::two_tenants().await;
    a.core.store.enqueue(Stage::Embed, "corpus", "c-a").await.unwrap();
    b.core.store.enqueue(Stage::Embed, "corpus", "c-b").await.unwrap();

    // Two claims, two tenants, in queue order.
    let mut seen = Vec::new();
    while let Some((subject, job)) = tenants.control().claim_job().await.unwrap() {
        seen.push((subject, job.target_id));
        if seen.len() == 2 { break; }
    }
    assert!(seen.contains(&("sub-a".to_string(), "c-a".to_string())));
    assert!(seen.contains(&("sub-b".to_string(), "c-b".to_string())));
}

#[tokio::test]
async fn a_job_for_a_deleted_user_is_dropped_rather_than_retried() {
    let (tenants, a, _b) = crate::tenants::test_support::two_tenants().await;
    a.core.store.enqueue(Stage::Embed, "corpus", "c-a").await.unwrap();
    tenants.control().delete_user("sub-a").await.unwrap();
    assert!(!run_one(&tenants).await.unwrap(), "nothing left to claim");
}
```

Add `crate::tenants::test_support::two_tenants()` returning `(Arc<Tenants>, Tenant, Tenant)` over one in-memory `Control`, a `TempDir`, and `MemoryVectors`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib jobs::`
Expected: FAIL to compile — `run_one` takes a `&Core`.

- [ ] **Step 3: Rewrite the claim path**

```rust
/// Claim one unit and run it, whoever it belongs to.
///
/// The queue is instance-wide; the work is not. The subject comes off the
/// claimed row and names the core the unit runs against, so a worker never
/// holds a tenant across two units — which is what makes this round-robin
/// without a scheduler.
pub async fn run_one(tenants: &crate::tenants::Tenants) -> Result<bool> {
    let Some((subject, job)) = tenants.control().claim_job().await? else {
        return Ok(false);
    };
    let core = match tenants.get(&subject).await {
        Ok(t) => t.core,
        // The user was deleted between the enqueue and the claim. Their rows
        // go with the row cascade, but one already-claimed unit can outlive
        // it, and retrying it can never succeed.
        Err(Error::NotFound) => {
            tracing::info!(subject = %subject, "queue row for a user that no longer exists; dropping");
            tenants.control().complete_job(job.id).await?;
            return Ok(true);
        }
        Err(e) => return Err(e),
    };
    run_job(&core, job).await
}
```

`run_job(&core, job)` is the existing body of `run_one` from the point it has a `Job`. `Worker::spawn` takes `Arc<Tenants>` and passes `&tenants` to `run_one`.

- [ ] **Step 4: Make the repair ticker instance-wide**

```rust
/// Finish what a crash left half-done, for every tenant.
///
/// Iterates `users` rather than the open registry: a tenant nobody has touched
/// since boot is exactly the one whose interrupted work nothing else will
/// find. `heal_store_drift` is deliberately *not* here — it scrolls a whole
/// collection over the network, and doing that for every registered user on a
/// timer costs a hundred full passes to find, on almost every base, nothing.
/// It runs on a tenant's first open instead.
pub fn spawn_repair_ticker(
    tenants: Arc<crate::tenants::Tenants>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
```

Inside the tick, replace the single-core body with a loop over `tenants.control().users().await`, calling `tenants.get(&u.subject)` and running the existing per-core passes — `reclaim_stuck`, `age_background`, `arm_missing_periodic` — against each.

Move the `heal_store_drift` spawn from `startup_checks` into `Tenants::open`, after the `Core` is built.

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add src/jobs/mod.rs src/core/background.rs src/tenants.rs
git commit -m "feat(jobs): workers claim from one queue and dispatch by subject

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Boot, adoption, and the CLI

**Files:**
- Modify: `src/main.rs` (boot, adoption, flags)
- Modify: `src/config.rs` (`MigrateConfig`)
- Modify: `src/vector/qdrant.rs` (alias rename)
- Test: `src/main.rs` startup tests

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `pub struct MigrateConfig { pub adopt_subject: Option<String> }` on `Config` as `migrate`
  - `QdrantVectors::rename_alias(&self, to: &str) -> Result<()>`
  - `async fn adopt(cfg: &Config, control: &Control) -> Result<Option<User>>`
  - CLI flags: `--user <SUBJECT>` on `--reindex`, `--export-eval`, `--recompute-coverage`; new `--list-users`, `--grant-judge <SUBJECT>`, `--revoke-judge <SUBJECT>`, `--delete-user <SUBJECT>`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn adoption_claims_the_single_user_database_once() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("engram.db");
    // A real single-user base: connect once so the schema is there.
    let control = Control::memory().await.unwrap();
    Store::connect(
        &StoreConfig { path: old.to_string_lossy().into(), ..Default::default() },
        control.clone(),
        "unused",
    ).await.unwrap();

    let mut cfg = Config::test_default();
    cfg.store.path = old.to_string_lossy().into();
    cfg.store.dir = dir.path().join("users").to_string_lossy().into();
    cfg.migrate.adopt_subject = Some("sub-1".into());

    let user = adopt(&cfg, &control).await.unwrap().expect("adopted");
    assert!(user.can_judge, "the adopting operator keeps the judge");
    assert!(!old.exists(), "the old file was moved, not copied");
    assert!(dir.path().join("users").join(format!("{}.db", user.slug)).exists());

    // Second boot is a no-op: the users table is no longer empty.
    assert!(adopt(&cfg, &control).await.unwrap().is_none());
}

#[tokio::test]
async fn adoption_does_nothing_without_a_subject_to_adopt_for() {
    let control = Control::memory().await.unwrap();
    let cfg = Config::test_default();
    assert!(adopt(&cfg, &control).await.unwrap().is_none());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --bin engram startup_tests`
Expected: FAIL to compile — `cannot find function adopt`.

- [ ] **Step 3: Write adoption**

```rust
/// Take over a single-user installation, once.
///
/// Guarded on the `users` table being empty, so this cannot fire on a running
/// multi-user instance however the config is edited afterwards. The alias is
/// renamed rather than the collections behind it: nothing re-embeds, and the
/// generation history the reindex path depends on is preserved.
///
/// The file move is rolled back if the rename fails. A half-adopted install
/// that boots is worse than one that refuses, because it presents as a base
/// whose searches have gone empty.
async fn adopt(cfg: &Config, control: &Control) -> Result<Option<User>> {
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

    let user = control.provision(subject, None).await?;
    control.set_can_judge(subject, true).await?;
    std::fs::create_dir_all(&cfg.store.dir)
        .map_err(|e| Error::Store(format!("could not make {}: {e}", cfg.store.dir)))?;
    let new = std::path::Path::new(&cfg.store.dir).join(format!("{}.db", user.slug));
    std::fs::rename(old, &new)
        .map_err(|e| Error::Store(format!("could not move {} to {}: {e}", old.display(), new.display())))?;

    let vectors = engram::vector::qdrant::QdrantVectors::connect(&cfg.vector).await?;
    let alias = format!("{}_{}", cfg.vector.collection, user.slug);
    if let Err(e) = vectors.rename_alias(&alias).await {
        // Put the file back before failing, or the next boot finds no base to
        // adopt and quietly starts an empty one.
        let _ = std::fs::rename(&new, old);
        let _ = control.delete_user(subject).await;
        return Err(e);
    }
    tracing::info!(subject, slug = %user.slug, "adopted the single-user base");
    Ok(Some(user))
}
```

`rename_alias` in `src/vector/qdrant.rs`, beside `point_alias_at`:

```rust
    /// Point a new alias name at whatever this one currently serves, and drop
    /// the old name. One `actions` batch, so there is no window in which the
    /// collection is reachable under neither name.
    pub async fn rename_alias(&self, to: &str) -> Result<()> {
        let Some(target) = self.resolve_alias().await? else {
            return Ok(());
        };
        let _: Value = self
            .call(
                Method::POST,
                "/collections/aliases",
                Some(json!({ "actions": [
                    { "create_alias": { "collection_name": target, "alias_name": to } },
                    { "delete_alias": { "alias_name": self.alias } }
                ]})),
            )
            .await?;
        Ok(())
    }
```

- [ ] **Step 4: Rewrite `main`**

Boot opens the control database only:

```rust
    let control = engram::store::control::Control::connect(&cfg.store.control_path).await?;
    adopt(&cfg, &control).await?;
    let cfg = Arc::new(cfg);
    let tenants = Arc::new(engram::tenants::Tenants::new(cfg.clone(), control.clone()));
    startup_checks(&cfg).await?;   // the inference probes only; no tenant is opened
```

`startup_checks` loses `core.vectors.ensure_collection`, `reclaim_stuck`, `purge_expired_sessions` (which moves to `control`), the drift spawn, and `embed_recipe_check`. `embed_recipe_check` moves into `Tenants::open`, after the `Core` is built, so it warns about the collection it is actually describing.

The CLI paths resolve a tenant first:

```rust
/// Resolve `--user`, or refuse with the list rather than picking one.
///
/// A default here is how the wrong collection gets reindexed: the operator
/// meant one tenant and the flag silently meant another.
async fn require_user(control: &Control, subject: Option<&str>) -> Result<User> {
    let known = control.users().await?;
    match subject.and_then(|s| known.iter().find(|u| u.subject == s)) {
        Some(u) => Ok(u.clone()),
        None => Err(Error::Validation(format!(
            "--user is required, and must be one of: {}",
            known.iter().map(|u| u.subject.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}
```

The four new flags:

```rust
    /// List the users this instance knows, with their slug and judge grant.
    #[arg(long)]
    list_users: bool,
    /// Let SUBJECT reach /ui/judge, which is also the only route that writes
    /// config.toml.
    #[arg(long, value_name = "SUBJECT")]
    grant_judge: Option<String>,
    #[arg(long, value_name = "SUBJECT")]
    revoke_judge: Option<String>,
    /// Remove SUBJECT: the row, the database file, and the Qdrant alias.
    #[arg(long, value_name = "SUBJECT")]
    delete_user: Option<String>,
    /// Which tenant a data command acts on.
    #[arg(long, value_name = "SUBJECT")]
    user: Option<String>,
```

`--delete-user` prints what it is about to remove and requires a typed `yes` on stdin before doing it.

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add src/main.rs src/config.rs src/vector/qdrant.rs
git commit -m "feat: control-only boot, one-time adoption, and per-user CLI flags

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: Empty-run backoff

**Files:**
- Modify: `src/jobs/mod.rs:284-300` (`rearm_periodic`), the `run_accounted` signature
- Modify: `src/store/jobs.rs` (`arm_periodic` writes `empty_runs`)
- Modify: `src/config.rs` (`ScheduleConfig::backoff_max_hours`)
- Test: `src/jobs/mod.rs` tests, on tokio's paused clock

**Interfaces:**
- Consumes: the `empty_runs` column added in Task 3.
- Produces:
  - `async fn run_accounted(core: &Core, stage: Stage) -> Result<bool>` — `true` when the run did work
  - `Control::arm_periodic(&self, subject, stage, target_kind, target_id, run_after: i64, empty_runs: i64)`
  - `ScheduleConfig { pub backoff_max_hours: u64, .. }`, default 24
  - `Control::empty_runs(&self, subject: &str, stage: Stage, target_id: &str) -> Result<i64>`, with the usual `Store::empty_runs(&self, stage, target_id)` delegate beside the ones from Task 3
  - `Control::arm_periodic_with_backoff(&self, subject, stage, target_kind, target_id, run_after: i64, empty_runs: i64) -> Result<()>`, and its `Store` delegate
  - `core.schedule.backoff_max_hours` is reachable already: `Core` carries `pub schedule: ScheduleConfig` at `src/core/mod.rs:163`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test(start_paused = true)]
async fn a_sweep_that_finds_nothing_waits_longer_each_time() {
    let core = crate::core::test_support::test_core().await;
    let base = crate::core::background::periodic_period(&core, Stage::Retention)
        .unwrap()
        .as_secs() as i64;

    let waits = {
        let mut out = Vec::new();
        for _ in 0..3 {
            rearm_periodic_with(&core, Stage::Retention, "collection", false).await;
            let row = pending_row(&core, Stage::Retention).await;
            out.push(row.run_after - crate::store::now());
        }
        out
    };
    assert_eq!(waits[0], base);
    assert_eq!(waits[1], base * 2);
    assert_eq!(waits[2], base * 4);
}

#[tokio::test(start_paused = true)]
async fn the_wait_is_capped() {
    let core = crate::core::test_support::test_core().await;
    let cap = core.schedule.backoff_max_hours as i64 * 3600;
    for _ in 0..20 {
        rearm_periodic_with(&core, Stage::Retention, "collection", false).await;
    }
    let row = pending_row(&core, Stage::Retention).await;
    assert!(row.run_after - crate::store::now() <= cap);
}

#[tokio::test(start_paused = true)]
async fn a_run_that_did_work_goes_back_to_the_configured_period() {
    let core = crate::core::test_support::test_core().await;
    let base = crate::core::background::periodic_period(&core, Stage::Retention)
        .unwrap()
        .as_secs() as i64;
    for _ in 0..5 {
        rearm_periodic_with(&core, Stage::Retention, "collection", false).await;
    }
    rearm_periodic_with(&core, Stage::Retention, "collection", true).await;
    let row = pending_row(&core, Stage::Retention).await;
    assert_eq!(row.run_after - crate::store::now(), base);
}

#[tokio::test(start_paused = true)]
async fn new_data_cancels_the_backoff() {
    let core = crate::core::test_support::test_core().await;
    for _ in 0..5 {
        rearm_periodic_with(&core, Stage::Retention, "collection", false).await;
    }
    core.store.arm_now(Stage::Retention, "collection", "collection").await.unwrap();
    let row = pending_row(&core, Stage::Retention).await;
    assert_eq!(row.run_after, 0, "arm_now already pulls a sleeping unit forward");
}
```

`pending_row` is a test helper reading the `jobs` row for a stage out of `core.store.control.pool`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::`
Expected: FAIL — `rearm_periodic_with` does not exist, `backoff_max_hours` is not a field.

- [ ] **Step 3: Implement**

```rust
/// How long until this sweep runs again.
///
/// The configured period when the run did something, doubled per consecutive
/// empty run when it did not, capped at `schedule.backoff_max_hours`. A quiet
/// base therefore stops waking every interval to find nothing — which is what
/// a dormant tenant costs, multiplied by however many of them there are.
///
/// The reset comes free and is what makes this safe: `arm_now` already pulls a
/// sleeping unit's `run_after` forward to zero, and every producer already
/// calls it. New data cancels the backoff without a single producer change,
/// which is the whole reason this is a backoff and not a firing rule.
async fn rearm_periodic_with(core: &Core, stage: Stage, target: &str, did_work: bool) {
    let Some(period) = crate::core::background::periodic_period(core, stage) else {
        return;
    };
    let empty = if did_work {
        0
    } else {
        core.store.empty_runs(stage, target).await.unwrap_or(0) + 1
    };
    let cap = core.schedule.backoff_max_hours.saturating_mul(3600);
    let wait = period
        .as_secs()
        .saturating_mul(1u64 << empty.min(16) as u32 >> 1)
        .min(cap)
        .max(period.as_secs());
    let at = crate::store::now() + wait as i64;
    if let Err(e) = core
        .store
        .arm_periodic_with_backoff(stage, "collection", target, at, empty)
        .await
    {
        tracing::warn!(stage = stage.as_str(), error = %e, "could not re-arm the sweep");
    }
}
```

`rearm_periodic(core, job)` calls this with the `did_work` that `run_accounted` now returns. `run_accounted` reads it from the counts it already writes into `sweep_runs.detail`: non-zero in any count is work.

`arm_periodic_with_backoff` is `arm_periodic` with `empty_runs = ?` added to both the insert and the `DO UPDATE SET`; `arm_now` sets `empty_runs = 0` alongside `run_after = 0`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib jobs::`
Expected: PASS, four new tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings
git add src/jobs/mod.rs src/store/jobs.rs src/config.rs
git commit -m "feat(jobs): a sweep that finds nothing waits longer next time

A dormant tenant costs a wake-up and a few queries per interval, not
model calls. Backoff is proportionate to that; arm_now already resets it,
so new data cancels the wait with no producer changes.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 10: Cross-tenant isolation, end to end

**Files:**
- Create: `tests/multi_tenant.rs`
- Modify: `tests/integration_qdrant.rs`
- Modify: `README.md`, `config.example.toml`
- Test: this task is the test

**Interfaces:**
- Consumes: everything.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the isolation test**

```rust
//! Two tenants over the real router. Everything else in the suite runs one
//! tenant, which is what keeps it honest about the single-user case; this is
//! what keeps it honest about the other one.

#[tokio::test]
async fn neither_tenant_can_see_the_others_artifact() {
    let (app, a, b) = two_tenant_app().await;

    let a_id = capture(&app, &a, "the same words in both bases").await;
    let b_id = capture(&app, &b, "the same words in both bases").await;
    assert_ne!(a_id, b_id);

    // Search
    let hits = search(&app, &a, "the same words").await;
    assert_eq!(hits, vec![a_id.clone()]);

    // Corpus list
    assert!(!corpora(&app, &a).await.contains(&b_id));

    // Direct fetch: a 404, not a 403. A 403 confirms the id exists, which is
    // itself a leak across a boundary that is supposed to be total.
    let res = get_as(&app, &a, &format!("/api/artifacts/{b_id}")).await;
    assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);

    // MCP
    assert!(!mcp_search(&app, &a, "the same words").await.contains(&b_id));
}

#[tokio::test]
async fn a_capture_by_one_tenant_queues_work_only_for_them() {
    let (app, a, b) = two_tenant_app().await;
    capture(&app, &a, "something to chew on").await;
    let queued = pending_by_subject(&app).await;
    assert!(queued.contains_key(&a.subject));
    assert!(!queued.contains_key(&b.subject));
}
```

Write `two_tenant_app()` in the same file: one `Control::memory()`, a `TempDir` for `store.dir`, `MemoryVectors` per tenant, and an `AppState` whose `Tenants` provisions both subjects. Drive requests with two distinct session cookies.

- [ ] **Step 2: Run to verify it fails, then passes**

Run: `cargo test --test multi_tenant`
Expected: FAIL first if any handler is still reading a shared core — that is the point of the test. Then PASS.

- [ ] **Step 3: Add the Qdrant case**

In `tests/integration_qdrant.rs`, add a test that provisions two tenants against the live Qdrant, writes one point each, and asserts each alias resolves to a different collection and each search returns only its own point. This is the one part alias-per-tenant that `MemoryVectors` cannot cover.

- [ ] **Step 4: Document the operator half**

In `README.md`, add a "Multiple users" section covering: `auth.mode = "oidc"` provisions on first login; `store.control_path`, `store.dir`, `store.max_open_tenants`; adoption via `migrate.adopt_subject` and that it fires once; `--list-users`, `--grant-judge`, `--revoke-judge`, `--delete-user`; that `--reindex`, `--export-eval` and `--recompute-coverage` need `--user`; the raw SQL fallback:

```
sqlite3 engram-control.db "UPDATE users SET can_judge = 1 WHERE subject = '...'"
```

and a **Backup** subsection: a backup is now the control database plus every file under `store.dir`, taken together. Restoring one side from a different moment shows up as store drift, which `heal_store_drift` repairs per tenant on first open.

Add the same keys, commented, to `config.example.toml`.

- [ ] **Step 5: Run everything**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add tests README.md config.example.toml
git commit -m "test: two tenants over the real router, and the operator docs

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```
