//! The control plane: identity and scheduling, one database per instance. Knowledge lives in
//! per-tenant databases, so no tenant filter exists to forget.

use crate::error::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

/// A hex SHA-256 prefix of the OIDC subject: emails change and subjects may hold any
/// character. `UNIQUE` on `slug` turns a collision into an error.
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
    pub created_at: i64,
}

impl User {
    fn from_row(r: UserRow) -> User {
        User {
            subject: r.subject,
            email: r.email,
            slug: r.slug,
            created_at: r.created_at,
        }
    }
}

/// The `users` columns a `User` is read from, for `query_as!`.
struct UserRow {
    subject: String,
    email: Option<String>,
    slug: String,
    created_at: i64,
}

/// Both halves base64url without padding: the private scalar and the uncompressed public point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VapidKeys {
    pub private: String,
    pub public: String,
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
        control.ensure_instance().await?;
        Ok(control)
    }

    /// One connection: every `sqlite::memory:` connection is a separate database.
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
        control.ensure_instance().await?;
        Ok(control)
    }

    /// A keypair is generated every boot and discarded if the row exists: `ON CONFLICT DO NOTHING`
    /// lets two processes on one file agree on one sender without a race.
    pub async fn ensure_instance(&self) -> Result<()> {
        use base64::Engine;
        use p256::elliptic_curve::{Generate, sec1::ToSec1Point};
        let secret = p256::SecretKey::generate();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let private = b64.encode(secret.to_bytes());
        let public = b64.encode(secret.public_key().to_sec1_point(false).as_bytes());
        sqlx::query!(
            "INSERT INTO instance (id, vapid_private, vapid_public, created_at) \
             VALUES (1, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
            private,
            public,
            super::now()
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn vapid(&self) -> Result<VapidKeys> {
        let row = sqlx::query!("SELECT vapid_private, vapid_public FROM instance WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(VapidKeys {
            private: row.vapid_private,
            public: row.vapid_public,
        })
    }

    /// Refuses a database missing a column, as `Store::migrate` does: `CREATE TABLE IF NOT
    /// EXISTS` keeps old columns silently. `ADDITIVE` columns with harmless defaults are added.
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("control_schema.sql");
        const ADDITIVE: [(&str, &str, &str); 8] = [
            (
                "users",
                "theme",
                "ALTER TABLE users ADD COLUMN theme TEXT NOT NULL DEFAULT 'system'",
            ),
            (
                "users",
                "density",
                "ALTER TABLE users ADD COLUMN density TEXT NOT NULL DEFAULT 'default'",
            ),
            (
                "api_tokens",
                "scheme",
                "ALTER TABLE api_tokens ADD COLUMN scheme INTEGER NOT NULL DEFAULT 1",
            ),
            (
                "users",
                "issuer",
                "ALTER TABLE users ADD COLUMN issuer TEXT NOT NULL DEFAULT ''",
            ),
            (
                "users",
                "lang",
                "ALTER TABLE users ADD COLUMN lang TEXT NOT NULL DEFAULT ''",
            ),
            (
                "users",
                "notify",
                "ALTER TABLE users ADD COLUMN notify TEXT NOT NULL DEFAULT '{}'",
            ),
            (
                "jobs",
                "class",
                "ALTER TABLE jobs ADD COLUMN class INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "jobs",
                "empty_runs",
                "ALTER TABLE jobs ADD COLUMN empty_runs INTEGER NOT NULL DEFAULT 0",
            ),
        ];

        let mut missing = super::missing_columns(&self.pool, SCHEMA).await?;
        super::apply_additive(&self.pool, &ADDITIVE, &mut missing).await?;
        if !missing.is_empty() {
            return Err(crate::error::Error::Store(format!(
                "the control database is older than the schema: {} missing. \
                 Recreate it, or add the columns by hand.",
                missing.join(", ")
            )));
        }

        // Drops columns whose leftover `NOT NULL` would fail `provision`'s `INSERT`. Only after
        // the refusal: a refused migration must leave the database as the previous binary needs it.
        const REMOVED: [(&str, &str, &str); 2] = [
            (
                "users",
                "last_seen_at",
                "ALTER TABLE users DROP COLUMN last_seen_at",
            ),
            (
                "users",
                "can_judge",
                "ALTER TABLE users DROP COLUMN can_judge",
            ),
        ];
        super::drop_removed(&self.pool, &REMOVED).await?;
        // runtime: the migrator runs against older schemas than the reference.
        sqlx::query("DROP TABLE IF EXISTS sessions")
            .execute(&self.pool)
            .await?;

        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
        // runtime: the migrator runs against older schemas than the reference.
        let removed = sqlx::query("DELETE FROM api_tokens WHERE scheme = 1")
            .execute(&self.pool)
            .await?
            .rows_affected();
        if removed > 0 {
            tracing::warn!(
                "{removed} API tokens from before this version were removed; mint new ones in Settings and pair clients again."
            );
        }
        tower_sessions_sqlx_store::SqliteStore::new(self.pool.clone())
            .migrate()
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
        self.backfill_job_class().await?;
        Ok(())
    }

    pub async fn prefs(&self, subject: &str) -> Result<crate::web::prefs::Prefs> {
        use crate::web::prefs::{Density, Prefs, Theme};
        let row: Option<(String, String)> = sqlx::query!(
            "SELECT theme, density FROM users WHERE subject = ?",
            subject
        )
        .fetch_optional(&self.pool)
        .await
        .map(|o| o.map(|r| (r.theme, r.density)))?;
        Ok(row
            .map(|(t, d)| Prefs {
                theme: Theme::parse(&t).unwrap_or_default(),
                density: Density::parse(&d).unwrap_or_default(),
            })
            .unwrap_or_default())
    }

    pub async fn set_prefs(&self, subject: &str, p: crate::web::prefs::Prefs) -> Result<()> {
        sqlx::query!(
            "UPDATE users SET theme = ?, density = ? WHERE subject = ?",
            p.theme.as_str(),
            p.density.as_str(),
            subject
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn backfill_issuer(&self, issuer: &str, local_user: Option<&str>) -> Result<u64> {
        Ok(sqlx::query!(
            "UPDATE users SET issuer = ? WHERE issuer = '' AND subject IS NOT ?",
            issuer,
            local_user
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn bind_issuer(&self, subject: &str, issuer: &str) -> Result<bool> {
        sqlx::query!(
            "UPDATE users SET issuer = ? WHERE subject = ? AND issuer = ''",
            issuer,
            subject
        )
        .execute(&self.pool)
        .await?;
        let bound: Option<String> =
            sqlx::query_scalar!("SELECT issuer FROM users WHERE subject = ?", subject)
                .fetch_optional(&self.pool)
                .await?;
        Ok(bound.is_some_and(|b| b == issuer))
    }

    pub async fn issuer_of(&self, subject: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT issuer FROM users WHERE subject = ?", subject)
                .fetch_optional(&self.pool)
                .await?
                .filter(|i| !i.is_empty()),
        )
    }

    /// Fixes rows an older binary queued for stages that are background now. Idempotent, so it
    /// runs on every connect; it also undoes ageing across a restart, which re-ages at once.
    async fn backfill_job_class(&self) -> Result<()> {
        let background: Vec<&str> = crate::store::jobs::Stage::ALL
            .iter()
            .filter(|s| s.class() == 1)
            .map(|s| s.as_str())
            .collect();
        // The stage names are `&'static str` from our own enum, so only a count is spliced.
        let holes = vec!["?"; background.len()].join(", ");
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE jobs SET class = 1 WHERE class = 0 AND stage IN ({holes})"
        )));
        for stage in background {
            q = q.bind(stage);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Concurrent first calls: `INSERT OR IGNORE` makes one row; the `SELECT` returns it to both.
    pub async fn provision(&self, subject: &str, email: Option<&str>) -> Result<User> {
        let now = super::now();
        sqlx::query!(
            "INSERT OR IGNORE INTO users (subject, email, slug, created_at)
             VALUES (?, ?, ?, ?)",
            subject,
            email,
            slug_for(subject),
            now
        )
        .execute(&self.pool)
        .await?;
        self.user(subject)
            .await?
            .ok_or_else(|| crate::error::Error::Store(format!("could not provision `{subject}`")))
    }

    pub async fn user(&self, subject: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as!(
            UserRow,
            r#"SELECT subject AS "subject!", email, slug, created_at FROM users WHERE subject = ?"#,
            subject
        )
        .fetch_optional(&self.pool)
        .await?
        .map(User::from_row))
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        Ok(sqlx::query_as!(
            UserRow,
            r#"SELECT subject AS "subject!", email, slug, created_at FROM users ORDER BY created_at"#
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(User::from_row)
        .collect())
    }

    /// The language this account's captures are read in, or `None` for automatic.
    pub async fn lang(&self, subject: &str) -> Result<Option<crate::infer::lang::Lang>> {
        let raw: Option<String> =
            sqlx::query_scalar!("SELECT lang FROM users WHERE subject = ?", subject)
                .fetch_optional(&self.pool)
                .await?;
        // An unknown stored tag reads as automatic, not English: closer to the operator's choice.
        Ok(raw.as_deref().and_then(crate::infer::lang::Lang::parse))
    }

    pub async fn set_lang(
        &self,
        subject: &str,
        lang: Option<crate::infer::lang::Lang>,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE users SET lang = ? WHERE subject = ?",
            lang.map(|l| l.tag()).unwrap_or(""),
            subject
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn notify(&self, subject: &str) -> Result<serde_json::Value> {
        let raw: Option<String> =
            sqlx::query_scalar!("SELECT notify FROM users WHERE subject = ?", subject)
                .fetch_optional(&self.pool)
                .await?;
        Ok(raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({})))
    }

    pub async fn set_notify(&self, subject: &str, notify: &serde_json::Value) -> Result<()> {
        sqlx::query!(
            "UPDATE users SET notify = ? WHERE subject = ?",
            notify.to_string(),
            subject
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One transaction, credentials first, identity last. `sessions` and `api_tokens` have no
    /// foreign key to `users`, and a live token would re-provision the deleted account.
    pub async fn delete_user(&self, subject: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        // Session records are opaque to SQL, so every browser signs in again.
        sqlx::query!("DELETE FROM tower_sessions")
            .execute(&mut *tx)
            .await?;
        sqlx::query!("DELETE FROM api_tokens WHERE subject = ?", subject)
            .execute(&mut *tx)
            .await?;
        let gone = sqlx::query!("DELETE FROM users WHERE subject = ?", subject)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(gone)
    }

    /// The queue lives in the control database, so this replaces a cross-database join.
    /// `states` empty means any state; `max_attempts` keeps rows with tries left.
    pub async fn targets_with_jobs(
        &self,
        subject: &str,
        stage: crate::store::jobs::Stage,
        targets: &[String],
        states: &[&str],
        max_attempts: Option<i64>,
    ) -> Result<std::collections::HashSet<String>> {
        let mut found = std::collections::HashSet::new();
        // Chunked under SQLITE_MAX_VARIABLE_NUMBER (999 before 3.32); callers pass unbounded lists.
        for chunk in targets.chunks(500) {
            // Every placeholder here is a count, never a value: the ids are bound.
            let holes = vec!["?"; chunk.len()].join(", ");
            let mut sql = format!(
                "SELECT target_id FROM jobs
                  WHERE subject = ? AND stage = ? AND target_id IN ({holes})"
            );
            if !states.is_empty() {
                let s = vec!["?"; states.len()].join(", ");
                sql.push_str(&format!(" AND state IN ({s})"));
            }
            if max_attempts.is_some() {
                sql.push_str(" AND attempts < ?");
            }
            // runtime: one placeholder per item in the IN list.
            let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql))
                .bind(subject)
                .bind(stage.as_str());
            for t in chunk {
                q = q.bind(t);
            }
            for st in states {
                q = q.bind(*st);
            }
            if let Some(n) = max_attempts {
                q = q.bind(n);
            }
            found.extend(q.fetch_all(&self.pool).await?);
        }
        Ok(found)
    }

    pub async fn live_count(&self, subject: &str, stage: crate::store::jobs::Stage) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM jobs
              WHERE subject = ? AND stage = ? AND state IN ('pending', 'running')",
            subject,
            stage.as_str()
        )
        .fetch_one(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[test]
    fn a_slug_is_stable_hex_and_safe_in_a_filename() {
        let a = slug_for("https://idp.example/sub|1234");
        assert_eq!(a, slug_for("https://idp.example/sub|1234"));
        assert_ne!(a, slug_for("https://idp.example/sub|1235"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn a_refused_migration_must_not_have_dropped_a_column() {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // An older `users`: it still has `last_seen_at`, which this schema
        // drops, and it does not have `slug`, which this schema refuses over.
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let control = Control { pool };

        let err = control.migrate().await.unwrap_err().to_string();
        assert!(
            err.contains("older than the schema") && err.contains("users.slug"),
            "the operator has to be told which column is missing: {err}"
        );

        // runtime: reads the live schema of the database at hand.
        let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('users')")
            .fetch_all(&control.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(
            have.iter().any(|c| c == "last_seen_at"),
            "a refused migration dropped a column anyway, so the previous \
             binary's `provision` can no longer run either: {have:?}"
        );
    }

    #[tokio::test]
    async fn asking_about_more_targets_than_sqlite_takes_parameters_for() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        let targets: Vec<String> = (0..4000).map(|i| format!("a{i}")).collect();
        // Every tenth one armed, so the answer is a filter and not a constant.
        for t in targets.iter().step_by(10) {
            crate::store::jobs::enqueue_with(
                &c,
                "sub-1",
                crate::store::jobs::Stage::Relate,
                "artifact",
                t,
            )
            .await
            .unwrap();
        }

        let armed = c
            .targets_with_jobs(
                "sub-1",
                crate::store::jobs::Stage::Relate,
                &targets,
                &[],
                None,
            )
            .await
            .unwrap();

        assert_eq!(armed.len(), 400, "the answer lost rows across the chunks");
        assert!(armed.contains("a0") && armed.contains("a3990"));
        assert!(!armed.contains("a1"));
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
    async fn deleting_a_user_removes_the_row() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        assert!(c.delete_user("sub-1").await.unwrap());
        assert!(c.user("sub-1").await.unwrap().is_none());
        assert!(!c.delete_user("sub-1").await.unwrap());
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_credentials_with_it() {
        // A live token would re-provision the deleted account through `get_or_provision`.
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        c.provision("sub-2", None).await.unwrap();
        let (_, doomed) = crate::auth::tokens::mint(&c, "laptop", "sub-1", None)
            .await
            .unwrap();
        let (_, spared) = crate::auth::tokens::mint(&c, "laptop", "sub-2", None)
            .await
            .unwrap();
        let dep = crate::web::security::Deployment::loopback(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            8080,
        )));
        crate::web::test_support::session_cookie(&c, &dep, "sub-2").await;

        assert!(c.delete_user("sub-1").await.unwrap());

        assert!(matches!(
            crate::auth::tokens::verify(&c, &doomed).await,
            Err(crate::error::Error::Unauthorized)
        ));

        // The other tenant is untouched: the delete is scoped, not a table clear.
        assert_eq!(
            crate::auth::tokens::verify(&c, &spared)
                .await
                .unwrap()
                .subject,
            "sub-2"
        );
        let sessions: i64 = sqlx::query_scalar!("SELECT count(*) FROM tower_sessions")
            .fetch_one(&c.pool)
            .await
            .unwrap();
        assert_eq!(sessions, 0, "every browser signs in again");
    }

    #[tokio::test]
    async fn provisioning_again_does_not_move_created_at() {
        let c = Control::memory().await.unwrap();
        let before = c.provision("sub-1", None).await.unwrap();
        let after = c.provision("sub-1", None).await.unwrap();
        assert_eq!(before.created_at, after.created_at);
    }

    async fn empty_pool() -> sqlx::SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_control_database_older_than_the_schema_refuses_at_boot() {
        let pool = empty_pool().await;
        // `users` from before the slug was stored.
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let e = Control { pool }.migrate().await.unwrap_err().to_string();
        assert!(e.contains("users.slug"), "{e}");
    }

    #[tokio::test]
    async fn an_instance_has_one_vapid_keypair_that_survives_a_reread() {
        use base64::Engine;
        let c = Control::memory().await.unwrap();
        let a = c.vapid().await.unwrap();
        assert_eq!(c.vapid().await.unwrap(), a);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let public = b64.decode(&a.public).unwrap();
        assert_eq!(public.len(), 65, "an uncompressed P-256 point");
        assert_eq!(public[0], 4);
        assert_eq!(b64.decode(&a.private).unwrap().len(), 32);
    }

    #[tokio::test]
    async fn ensuring_the_instance_again_keeps_the_first_key() {
        let c = Control::memory().await.unwrap();
        let first = c.vapid().await.unwrap();
        c.ensure_instance().await.unwrap();
        assert_eq!(c.vapid().await.unwrap(), first);
    }

    #[tokio::test]
    async fn notify_is_empty_until_set_and_survives_a_reread() {
        let c = Control::memory().await.unwrap();
        c.provision("u", None).await.unwrap();
        assert_eq!(c.notify("u").await.unwrap(), serde_json::json!({}));
        c.set_notify(
            "u",
            &serde_json::json!({"gotify": {"url": "https://g/message", "token": "t"}}),
        )
        .await
        .unwrap();
        assert_eq!(c.notify("u").await.unwrap()["gotify"]["token"], "t");
    }

    #[tokio::test]
    async fn a_users_table_without_notify_gains_the_column() {
        let pool = empty_pool().await;
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT, slug TEXT NOT NULL UNIQUE,
               can_judge INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let control = Control { pool };
        control.migrate().await.unwrap();
        control.provision("u", None).await.unwrap();
        assert_eq!(control.notify("u").await.unwrap(), serde_json::json!({}));
    }

    #[tokio::test]
    async fn a_control_database_still_holding_the_dropped_columns_loses_them() {
        let pool = empty_pool().await;
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT, slug TEXT NOT NULL UNIQUE,
               can_judge INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             INSERT INTO users (subject, slug, can_judge, created_at, last_seen_at)
                  VALUES ('sub-0', 'slug0', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let control = Control { pool };
        control
            .migrate()
            .await
            .expect("the stale columns are dropped");
        control
            .provision("sub-1", None)
            .await
            .expect("a sign-in still provisions");
        // runtime: reads the live schema of the database at hand.
        let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('users')")
            .fetch_all(&control.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(!cols.iter().any(|c| c == "last_seen_at"), "{cols:?}");
        assert!(!cols.iter().any(|c| c == "can_judge"), "{cols:?}");
        assert_eq!(
            control.users().await.unwrap().len(),
            2,
            "the rows outlive their columns"
        );
    }

    #[tokio::test]
    async fn a_queue_from_before_the_backoff_counter_gains_the_column() {
        let pool = empty_pool().await;
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT, slug TEXT NOT NULL UNIQUE,
               can_judge INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             CREATE TABLE jobs (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               subject TEXT NOT NULL REFERENCES users(subject) ON DELETE CASCADE,
               stage TEXT NOT NULL, target_kind TEXT NOT NULL, target_id TEXT NOT NULL,
               state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
               run_after INTEGER NOT NULL DEFAULT 0, last_error TEXT, claimed_at INTEGER,
               created_at INTEGER NOT NULL DEFAULT 0, seq INTEGER NOT NULL DEFAULT 0,
               class INTEGER NOT NULL DEFAULT 0,
               UNIQUE(subject, stage, target_id))",
        )
        .execute(&pool)
        .await
        .unwrap();

        let control = Control { pool };
        control
            .migrate()
            .await
            .expect("an additive column is added");
        let n: i64 = sqlx::query_scalar!("SELECT count(empty_runs) FROM jobs")
            .fetch_one(&control.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn applying_the_control_schema_twice_changes_nothing() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        c.migrate().await.unwrap();
        assert_eq!(c.users().await.unwrap().len(), 1);
    }

    const PRE_CONFORMANCE_SCHEMA: &str =
        include_str!("../../tests/fixtures/control_schema_pre_conformance.sql");

    async fn old_control_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.db");
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::raw_sql(PRE_CONFORMANCE_SCHEMA)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "INSERT INTO users (subject, email, slug, created_at) VALUES ('sub-1', 'a@example.org', 'slug1', 1);
             INSERT INTO api_tokens (id, name, token_hash, subject, created_at) VALUES ('t1', 'laptop', '$argon2id$x', 'sub-1', 1);
             INSERT INTO sessions (id, subject, email, expires_at, created_at) VALUES ('s1', 'sub-1', NULL, 9999999999, 1);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        (dir, path)
    }

    #[tokio::test]
    async fn migrating_an_old_control_db_removes_scheme_1_tokens_and_keeps_users() {
        let (_dir, path) = old_control_db().await;
        let c = Control::connect(path.to_str().unwrap()).await.unwrap();
        let tokens: i64 = sqlx::query_scalar!("SELECT count(*) FROM api_tokens")
            .fetch_one(&c.pool)
            .await
            .unwrap();
        assert_eq!(tokens, 0);
        let u = c.user("sub-1").await.unwrap().expect("the user is kept");
        assert_eq!(u.email.as_deref(), Some("a@example.org"));
        let (_, fresh) = crate::auth::tokens::mint(&c, "new", "sub-1", None)
            .await
            .unwrap();
        assert_eq!(
            crate::auth::tokens::verify(&c, &fresh)
                .await
                .unwrap()
                .subject,
            "sub-1"
        );
        c.migrate().await.unwrap();
        assert!(
            crate::auth::tokens::verify(&c, &fresh).await.is_ok(),
            "a second migrate keeps new tokens"
        );
    }

    #[tokio::test]
    async fn the_old_sessions_table_is_dropped() {
        let (_dir, path) = old_control_db().await;
        let c = Control::connect(path.to_str().unwrap()).await.unwrap();
        let tables: Vec<String> = sqlx::query_scalar!(
            r#"SELECT name AS "name!" FROM sqlite_master WHERE type = 'table'"#
        )
        .fetch_all(&c.pool)
        .await
        .unwrap();
        assert!(!tables.iter().any(|t| t == "sessions"), "{tables:?}");
        assert!(tables.iter().any(|t| t == "tower_sessions"), "{tables:?}");
    }

    #[tokio::test]
    async fn a_tenant_store_carries_the_control_handle() {
        let store = crate::store::Store::memory().await.unwrap();
        store.control.provision("sub-1", None).await.unwrap();
        // Two: the one just provisioned, and the test subject every `Store::memory()` runs as.
        assert_eq!(store.control.users().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn two_tenant_stores_can_share_one_control_database() {
        let control = Control::memory().await.unwrap();
        let a = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap();
        let b = crate::store::Store::memory_with(control.clone())
            .await
            .unwrap();
        a.control.provision("sub-a", None).await.unwrap();
        b.control.provision("sub-b", None).await.unwrap();
        assert_eq!(control.users().await.unwrap().len(), 3);
    }
}
