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
/// which may contain anything at all -- including characters that are neither
/// a legal filename nor a legal Qdrant collection name. Sixteen hex digits is
/// 64 bits: this is a naming scheme and not a secret, and the `UNIQUE` on
/// `slug` turns the collision nobody will ever see into an error rather than
/// into two people quietly sharing a database.
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
}

impl User {
    fn from_row(r: &sqlx::sqlite::SqliteRow) -> User {
        User {
            subject: r.get("subject"),
            email: r.get("email"),
            slug: r.get("slug"),
            can_judge: r.get::<i64, _>("can_judge") != 0,
            created_at: r.get("created_at"),
        }
    }
}

#[derive(Clone)]
pub struct Control {
    pub pool: sqlx::SqlitePool,
}

/// What adoption carried out of a single-user database, for the line it prints.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Carried {
    pub tokens: u64,
    pub sessions: u64,
    pub jobs: u64,
}

impl Carried {
    pub fn is_empty(self) -> bool {
        self == Carried::default()
    }
}

/// Whether a database has a table by that name — an old base may predate any
/// of the three adoption carries over.
async fn has_table(pool: &sqlx::SqlitePool, name: &str) -> Result<bool> {
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(name)
            .fetch_one(pool)
            .await?;
    Ok(n > 0)
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
    /// a separate database, so a multi-connection pool would see different
    /// data per query.
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

    /// The schema, and the same refusal `Store::migrate` makes.
    ///
    /// `CREATE TABLE IF NOT EXISTS` leaves an existing table's columns exactly
    /// as they are, so applying this file to a database written by an older
    /// binary is silent: nothing fails until some query names a column that is
    /// not there, at which point a bare `no such column` arrives from the
    /// middle of a request. The tenant schema has refused that since it was
    /// written; the control schema is the one that keeps growing — `class`,
    /// then `empty_runs` — so it needs the guard more, not less.
    ///
    /// `ADDITIVE` is the same exception, and deliberately the same shape: a
    /// column added beside the others with a default no existing row needs to
    /// have been written with is added rather than refused. Anything not
    /// named here still refuses, because a column whose default rewrites what
    /// old rows mean is not something a boot path may guess about.
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("control_schema.sql");
        const ADDITIVE: [(&str, &str, &str); 2] = [
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

        let mut missing = Vec::new();
        for (table, columns) in super::schema_columns(SCHEMA) {
            let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(&table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            // No columns at all is no such table: a fresh control database, or
            // a table this schema is about to create.
            if have.is_empty() {
                continue;
            }
            for c in columns {
                if !have.iter().any(|h| h.eq_ignore_ascii_case(&c)) {
                    missing.push(format!("{table}.{c}"));
                }
            }
        }
        for (table, column, ddl) in ADDITIVE {
            let key = format!("{table}.{column}");
            let Some(i) = missing.iter().position(|m| *m == key) else {
                continue;
            };
            sqlx::raw_sql(ddl)
                .execute(&self.pool)
                .await
                .map_err(|e| crate::error::Error::Store(e.to_string()))?;
            tracing::info!(column = %key, "added a column the control schema expects");
            missing.remove(i);
        }
        if !missing.is_empty() {
            return Err(crate::error::Error::Store(format!(
                "the control database is older than the schema: {} missing. \
                 Recreate it, or add the columns by hand.",
                missing.join(", ")
            )));
        }

        // The mirror of `ADDITIVE`, and rarer: a column this schema used to
        // have and no longer does. `CREATE TABLE IF NOT EXISTS` cannot take one
        // away, and a leftover `NOT NULL` column with no default is not inert —
        // it fails every `INSERT` that stopped naming it, which here is the one
        // in `provision`, so the next person to sign in on an upgraded instance
        // gets a 500 at the door.
        //
        // Only for a column nothing reads. `users.last_seen_at` was written by
        // `provision` and by a `touch` no production path ever called, listed
        // by nothing and read by nothing, so what this drops is a column whose
        // every value was the row's own `created_at`.
        //
        // After the refusal and not before it, which is the whole reason this
        // sits down here rather than beside `ADDITIVE`. A drop is the one step
        // in this function that cannot be undone: a control database missing
        // some *other* column has to come out of a refused migration exactly as
        // it went in, so that the operator can put the previous binary back.
        // Dropping first and refusing second takes `last_seen_at` away from a
        // base the old binary's `provision` still names, which strands them on
        // both sides. `ADDITIVE` may run before the refusal because adding a
        // column with a default is invisible to a binary that does not know it.
        const REMOVED: [(&str, &str, &str); 1] = [(
            "users",
            "last_seen_at",
            "ALTER TABLE users DROP COLUMN last_seen_at",
        )];
        for (table, column, ddl) in REMOVED {
            let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            if !have.iter().any(|h| h.eq_ignore_ascii_case(column)) {
                continue;
            }
            sqlx::raw_sql(ddl)
                .execute(&self.pool)
                .await
                .map_err(|e| crate::error::Error::Store(e.to_string()))?;
            tracing::info!(column = %format!("{table}.{column}"), "dropped a column the control schema no longer has");
        }

        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
        self.backfill_job_class().await?;
        Ok(())
    }

    /// Put the sweeps in the background class.
    ///
    /// `jobs.class` defaults to `0`, which is foreground — the safe direction
    /// to be wrong in, and the wrong answer for every sweep. Not for a row
    /// written before the column existed: `migrate` reads the columns before it
    /// applies the schema and refuses a base without `jobs.class` outright, so
    /// no such row ever reaches this. What it corrects is a row written by an
    /// older binary for a stage that was foreground then and is background now
    /// — pending work outlives an upgrade, and nothing else revisits its class.
    ///
    /// It runs on every connect rather than once: it is idempotent by
    /// construction, since it only ever moves rows that are still `0` *and*
    /// whose stage says they should not be, which is why
    /// `applying_the_schema_twice_changes_nothing` still holds.
    ///
    /// It cannot tell such a row from one that aged (§4.4) and so undoes an
    /// ageing across a restart. That costs a moment: the repair ticker's first
    /// tick fires immediately at boot, the ageing predicate is still satisfied
    /// by a row that had already aged, and it ages straight back.
    async fn backfill_job_class(&self) -> Result<()> {
        let background: Vec<&str> = crate::store::jobs::Stage::ALL
            .iter()
            .filter(|s| s.class() == 1)
            .map(|s| s.as_str())
            .collect();
        // The stage names are `&'static str` from our own enum, never anything
        // a request supplied, so splicing the placeholders is splicing a count.
        let holes = vec!["?"; background.len()].join(", ");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE jobs SET class = 1 WHERE class = 0 AND stage IN ({holes})"
        )));
        for stage in background {
            q = q.bind(stage);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Idempotent by construction. Two concurrent first requests for the same
    /// unseen subject both run this: `INSERT OR IGNORE` means one row, and the
    /// `SELECT` afterwards means both callers get it.
    pub async fn provision(&self, subject: &str, email: Option<&str>) -> Result<User> {
        let now = super::now();
        sqlx::query(
            "INSERT OR IGNORE INTO users (subject, email, slug, can_judge, created_at)
             VALUES (?, ?, ?, 0, ?)",
        )
        .bind(subject)
        .bind(email)
        .bind(slug_for(subject))
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

    /// `false` when there is no such subject, so the grant CLI can say so
    /// rather than report success on a typo nobody will ever log in as.
    pub async fn set_can_judge(&self, subject: &str, on: bool) -> Result<bool> {
        Ok(
            sqlx::query("UPDATE users SET can_judge = ? WHERE subject = ?")
                .bind(i64::from(on))
                .bind(subject)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    /// Remove a user and everything in the control plane that speaks for them:
    /// their sessions, their API tokens, and — through `jobs`' foreign key —
    /// their queued work.
    ///
    /// The credentials go with the row and not after it. `sessions` and
    /// `api_tokens` carry no foreign key to `users`, so deleting only the user
    /// left every unrevoked token working: `tokens::verify` scans
    /// `active_tokens` without asking whether that subject still exists, and
    /// the `Tenant` extractor behind it calls `get_or_provision` — which puts
    /// the row back, recreates the database file, and re-creates the Qdrant
    /// collection. The account the operator had just deleted came back, empty
    /// and still authenticated.
    ///
    /// One transaction, so a failure part-way through cannot leave live
    /// credentials for a user that is gone. The three statements run in that
    /// order for the same reason: credentials first, identity last.
    pub async fn delete_user(&self, subject: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM sessions WHERE subject = ?")
            .bind(subject)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM api_tokens WHERE subject = ?")
            .bind(subject)
            .execute(&mut *tx)
            .await?;
        let gone = sqlx::query("DELETE FROM users WHERE subject = ?")
            .bind(subject)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(gone)
    }

    /// Which of `targets` this tenant has a job for at `stage`.
    ///
    /// The queue lives in a different database from the artifacts now, so the
    /// four `NOT EXISTS (SELECT 1 FROM jobs ...)` clauses that used to ride
    /// along with an `artifacts` scan cannot be written as joins any more.
    /// They become this: the tenant names its candidates, and the queue says
    /// which of them are spoken for. One extra round trip, and no
    /// cross-database join to keep working.
    ///
    /// `states` empty means any state. `max_attempts` limits the answer to
    /// rows that still have tries left, which is what separates "something is
    /// going to do this" from "something gave up on this".
    pub async fn targets_with_jobs(
        &self,
        subject: &str,
        stage: crate::store::jobs::Stage,
        targets: &[String],
        states: &[&str],
        max_attempts: Option<i64>,
    ) -> Result<std::collections::HashSet<String>> {
        let mut found = std::collections::HashSet::new();
        // Chunked well under SQLITE_MAX_VARIABLE_NUMBER, which is 999 on the
        // builds that predate 3.32, for the same reason `stale_unreachable_pairs`
        // and `stranded_merges` are. The callers here decide the length: the
        // relate backstop hands over a window of `limit * 8` ids — four thousand
        // for the one production call — and `pending_artifacts_are_isolated`
        // hands over every pending chunk of a source, which nothing bounds at
        // all. Unchunked, both fail outright with `too many SQL variables` on
        // such a build, and both fail *quietly*: the backstop is a repair pass
        // that only logs, so it would simply never run again.
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

    /// How many of this tenant's units at `stage` are still going to run.
    pub async fn live_count(&self, subject: &str, stage: crate::store::jobs::Stage) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs
              WHERE subject = ? AND stage = ? AND state IN ('pending', 'running')",
        )
        .bind(subject)
        .bind(stage.as_str())
        .fetch_one(&self.pool)
        .await?)
    }

    /// What a single-user database held about the person using it, moved into
    /// the control plane under `subject`.
    ///
    /// Adoption renames a file and an alias, which carries every table this
    /// build still reads out of a tenant database. Three that it does not are
    /// left behind in it: `api_tokens`, `sessions` and `jobs` moved to the
    /// control plane when it was written, and `schema.sql` no longer so much as
    /// names them. Left where they are, the upgrade silently invalidates every
    /// API token the operator ever minted — the browser extension's among them
    /// — signs them out of the browser they had open, and drops whatever was
    /// queued when the old process stopped.
    ///
    /// Every row is rewritten under `subject`, the subject adoption is
    /// provisioning for, rather than under whatever the old install wrote. A
    /// single-user base has exactly one owner however they signed in, and that
    /// old value may be a local-mode username that no identity provider will
    /// ever present. `jobs.subject` is a foreign key onto the row adoption just
    /// wrote, so it could not be anything else in any case.
    ///
    /// Idempotent: every insert is `INSERT OR IGNORE`, and `jobs` carries the
    /// same `UNIQUE(subject, stage, target_id)` the queue is keyed by.
    ///
    /// Tolerant of a database that predates any of the three tables — an
    /// install old enough not to have had them is one with nothing to carry,
    /// not a failure.
    pub async fn carry_over_single_user(&self, db_path: &str, subject: &str) -> Result<Carried> {
        // Not `read_only`: a database with a hot `-wal` needs to write its
        // `-shm` to be readable at all, and the file being adopted is exactly
        // the one a process was using until a moment ago.
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .create_if_missing(false)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let old = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;

        let mut carried = Carried::default();
        if has_table(&old, "api_tokens").await? {
            for r in sqlx::query("SELECT * FROM api_tokens")
                .fetch_all(&old)
                .await?
            {
                let done = sqlx::query(
                    "INSERT OR IGNORE INTO api_tokens
                       (id, name, token_hash, subject, created_at, last_used_at, revoked_at, user_agent)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(r.get::<String, _>("id"))
                .bind(r.get::<String, _>("name"))
                .bind(r.get::<String, _>("token_hash"))
                .bind(subject)
                .bind(r.get::<i64, _>("created_at"))
                .bind(r.try_get::<Option<i64>, _>("last_used_at").unwrap_or(None))
                .bind(r.try_get::<Option<i64>, _>("revoked_at").unwrap_or(None))
                // A column added after some of these bases were written.
                .bind(r.try_get::<Option<String>, _>("user_agent").unwrap_or(None))
                .execute(&self.pool)
                .await?;
                carried.tokens += done.rows_affected();
            }
        }
        if has_table(&old, "sessions").await? {
            // Expired ones are not carried: they are a cookie nobody can use,
            // and the purge would drop them on the first repair tick anyway.
            for r in sqlx::query("SELECT * FROM sessions WHERE expires_at > ?")
                .bind(super::now())
                .fetch_all(&old)
                .await?
            {
                let done = sqlx::query(
                    "INSERT OR IGNORE INTO sessions (id, subject, email, expires_at, created_at)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(r.get::<String, _>("id"))
                .bind(subject)
                .bind(r.try_get::<Option<String>, _>("email").unwrap_or(None))
                .bind(r.get::<i64, _>("expires_at"))
                .bind(r.get::<i64, _>("created_at"))
                .execute(&self.pool)
                .await?;
                carried.sessions += done.rows_affected();
            }
        }
        if has_table(&old, "jobs").await? {
            // Live work only. A `done` row is a unit that ran, and a `failed`
            // one is a unit an older build gave up on — neither is work in
            // flight, and both are the old queue's history rather than its
            // contents. `running` comes across as `pending` with its
            // `claimed_at` dropped, which is what `reclaim_stuck` would make of
            // it: the process holding it is the one that stopped.
            for r in
                sqlx::query("SELECT * FROM jobs WHERE state IN ('pending', 'running') ORDER BY id")
                    .fetch_all(&old)
                    .await?
            {
                let stage: String = r.get("stage");
                let class = r.try_get::<i64, _>("class").unwrap_or_else(|_| {
                    crate::store::jobs::Stage::parse(&stage).map_or(0, |s| s.class())
                });
                let done = sqlx::query(
                    "INSERT OR IGNORE INTO jobs
                       (subject, stage, target_kind, target_id, state, attempts, run_after,
                        created_at, seq, class, empty_runs)
                     VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?, 0)",
                )
                .bind(subject)
                .bind(&stage)
                .bind(r.get::<String, _>("target_kind"))
                .bind(r.get::<String, _>("target_id"))
                .bind(r.get::<i64, _>("attempts"))
                .bind(r.get::<i64, _>("run_after"))
                .bind(r.try_get::<i64, _>("created_at").unwrap_or(0))
                .bind(r.try_get::<i64, _>("seq").unwrap_or(0))
                .bind(class)
                .execute(&self.pool)
                .await?;
                carried.jobs += done.rows_affected();
            }
        }
        old.close().await;
        Ok(carried)
    }

    /// Undo `carry_over_single_user`, for an adoption that then failed.
    ///
    /// `jobs` is not named here: it cascades off the user row, and every path
    /// that calls this deletes that too. Best-effort by construction — it runs
    /// on a path that is already returning an error, and the error worth
    /// reporting is the one that got there.
    pub async fn discard_carried_over(&self, subject: &str) {
        for sql in [
            "DELETE FROM api_tokens WHERE subject = ?",
            "DELETE FROM sessions WHERE subject = ?",
        ] {
            let _ = sqlx::query(sql).bind(subject).execute(&self.pool).await;
        }
    }
}

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

    /// The mirror of the tenant side's "must not have applied half the file".
    ///
    /// A drop is the one irreversible step in `migrate`, so a control database
    /// that is refused has to come out of it exactly as it went in — otherwise
    /// the operator cannot put the previous binary back either, and both
    /// directions are broken at once.
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
        // drops, and it does not yet have `can_judge`, which this schema
        // refuses over.
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT, slug TEXT NOT NULL UNIQUE,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let control = Control { pool };

        let err = control.migrate().await.unwrap_err().to_string();
        assert!(
            err.contains("older than the schema") && err.contains("users.can_judge"),
            "the operator has to be told which column is missing: {err}"
        );

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

    /// The relate backstop hands over `limit * 8` ids — four thousand for its
    /// one production call — and `pending_artifacts_are_isolated` hands over
    /// however many chunks a source has. Unchunked, both fail with `too many
    /// SQL variables` on a SQLite older than 3.32, and the backstop fails
    /// silently: it is a repair pass that only logs, so it would simply stop
    /// running.
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

    #[tokio::test]
    async fn deleting_a_user_takes_their_credentials_with_it() {
        // `sessions` and `api_tokens` carry no foreign key to `users`, so a
        // delete that took only the row left every unrevoked token working —
        // and `tokens::verify` does not ask whether the subject still exists.
        // The `Tenant` extractor behind it calls `get_or_provision`, which puts
        // the row back and recreates the database file and the collection: the
        // account the operator had just deleted, back, empty, and still
        // authenticated.
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        c.provision("sub-2", None).await.unwrap();
        let (_, doomed) = crate::auth::tokens::mint(&c, "laptop", "sub-1", None)
            .await
            .unwrap();
        let (_, spared) = crate::auth::tokens::mint(&c, "laptop", "sub-2", None)
            .await
            .unwrap();
        c.insert_session("sid-1", "sub-1", None, 3600)
            .await
            .unwrap();
        c.insert_session("sid-2", "sub-2", None, 3600)
            .await
            .unwrap();

        assert!(c.delete_user("sub-1").await.unwrap());

        assert!(matches!(
            crate::auth::tokens::verify(&c, &doomed).await,
            Err(crate::error::Error::Unauthorized)
        ));
        assert!(c.get_session("sid-1").await.unwrap().is_none());

        // The other tenant is untouched, which is the whole point of scoping
        // the delete rather than clearing the tables.
        assert_eq!(
            crate::auth::tokens::verify(&c, &spared)
                .await
                .unwrap()
                .subject,
            "sub-2"
        );
        assert!(c.get_session("sid-2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn provisioning_again_does_not_move_created_at() {
        let c = Control::memory().await.unwrap();
        let before = c.provision("sub-1", None).await.unwrap();
        let after = c.provision("sub-1", None).await.unwrap();
        assert_eq!(before.created_at, after.created_at);
    }

    /// A pool with nothing in it, so a test can put an older schema there.
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

    /// `CREATE TABLE IF NOT EXISTS` leaves an existing table's columns alone,
    /// so without this the first query naming the new column is where an
    /// upgraded instance finds out — a bare `no such column`, from the middle
    /// of somebody's request.
    #[tokio::test]
    async fn a_control_database_older_than_the_schema_refuses_at_boot() {
        let pool = empty_pool().await;
        // `users` from before the judge grant.
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT, slug TEXT NOT NULL UNIQUE,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let e = Control { pool }.migrate().await.unwrap_err().to_string();
        assert!(e.contains("users.can_judge"), "{e}");
    }

    /// The exception, and the same one `Store::migrate` makes: a column added
    /// beside the others with a default no existing row needs to have been
    /// written with is added, not refused.
    /// The mirror of the additive case: a column the schema used to have and no
    /// longer does has to go, or the `INSERT` in `provision` that stopped naming
    /// it fails against a leftover `NOT NULL` with no default — which presents
    /// as a 500 at the door for the next person to sign in after an upgrade.
    #[tokio::test]
    async fn a_control_database_still_holding_last_seen_at_loses_it() {
        let pool = empty_pool().await;
        sqlx::raw_sql(
            "CREATE TABLE users (
               subject TEXT PRIMARY KEY, email TEXT, slug TEXT NOT NULL UNIQUE,
               can_judge INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let control = Control { pool };
        control
            .migrate()
            .await
            .expect("the stale column is dropped");
        control
            .provision("sub-1", None)
            .await
            .expect("a sign-in still provisions");
        let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('users')")
            .fetch_all(&control.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(!cols.iter().any(|c| c == "last_seen_at"), "{cols:?}");
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
        let n: i64 = sqlx::query_scalar("SELECT count(empty_runs) FROM jobs")
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

    #[tokio::test]
    async fn a_session_survives_in_the_control_database() {
        let c = Control::memory().await.unwrap();
        c.insert_session("sid-1", "sub-1", Some("a@example.org"), 3600)
            .await
            .unwrap();
        let s = c.get_session("sid-1").await.unwrap().expect("session");
        assert_eq!(s.subject, "sub-1");
    }

    /// The three tables the control plane took over, as an older build left
    /// them in the one database it had.
    const LEGACY: &str = "
        CREATE TABLE sessions (
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
          class INTEGER NOT NULL DEFAULT 0, UNIQUE(stage, target_id))";

    /// A single-user database with one of everything adoption has to carry.
    async fn single_user_file(path: &str) {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::raw_sql(LEGACY).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO api_tokens (id, name, token_hash, subject, created_at, user_agent)
             VALUES ('tok-1', 'extension', 'hash', 'dev', 10, 'engram-extension')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, expires) in [("sid-live", crate::store::now() + 3600), ("sid-dead", 1)] {
            sqlx::query(
                "INSERT INTO sessions (id, subject, email, expires_at, created_at)
                 VALUES (?, 'dev', 'dev@example.org', ?, 10)",
            )
            .bind(id)
            .bind(expires)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (target, state) in [
            ("c-live", "pending"),
            ("c-held", "running"),
            ("c-old", "done"),
        ] {
            sqlx::query(
                "INSERT INTO jobs (stage, target_kind, target_id, state, attempts, created_at, seq, class)
                 VALUES ('synthesize', 'corpus', ?, ?, 2, 11, 3, 0)",
            )
            .bind(target)
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;
    }

    /// Adoption renames a file, and these three tables are the ones that does
    /// not carry: they live in the control plane now and `schema.sql` no longer
    /// names them. Left behind, the upgrade silently invalidates every API
    /// token the operator minted — the extension's included — signs them out,
    /// and drops whatever was queued when the old process stopped.
    #[tokio::test]
    async fn adoption_carries_the_auth_and_queue_rows_out_of_a_single_user_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram.db").to_string_lossy().to_string();
        single_user_file(&path).await;

        let c = Control::memory().await.unwrap();
        c.provision("sub-new", None).await.unwrap();
        let carried = c.carry_over_single_user(&path, "sub-new").await.unwrap();
        assert_eq!(
            carried,
            Carried {
                tokens: 1,
                // The expired one is a cookie nobody can use.
                sessions: 1,
                // `done` is history, not work in flight.
                jobs: 2,
            }
        );

        // Every row under the subject adoption provisioned, not the one the
        // old install happened to write — which may be a local-mode username.
        let tokens = c.active_tokens().await.unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].subject, "sub-new");
        assert_eq!(tokens[0].token_hash, "hash");
        assert_eq!(
            c.get_session("sid-live").await.unwrap().unwrap().subject,
            "sub-new"
        );
        assert!(c.get_session("sid-dead").await.unwrap().is_none());

        // The claimed one comes across as pending, which is what
        // `reclaim_stuck` would have made of it: the process holding it is the
        // one that stopped.
        let queued: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT target_id, state, attempts FROM jobs WHERE subject = 'sub-new' ORDER BY target_id",
        )
        .fetch_all(&c.pool)
        .await
        .unwrap();
        assert_eq!(
            queued,
            vec![
                ("c-held".to_string(), "pending".to_string(), 2),
                ("c-live".to_string(), "pending".to_string(), 2),
            ]
        );
    }

    #[tokio::test]
    async fn carrying_the_same_base_over_twice_carries_nothing_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram.db").to_string_lossy().to_string();
        single_user_file(&path).await;

        let c = Control::memory().await.unwrap();
        c.provision("sub-new", None).await.unwrap();
        c.carry_over_single_user(&path, "sub-new").await.unwrap();
        assert!(
            c.carry_over_single_user(&path, "sub-new")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(c.active_tokens().await.unwrap().len(), 1);
    }

    /// An install old enough not to have had these tables has nothing to
    /// carry, which is not a failure to adopt it.
    #[tokio::test]
    async fn carrying_over_a_base_without_those_tables_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram.db").to_string_lossy().to_string();
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
            .close()
            .await;

        let c = Control::memory().await.unwrap();
        c.provision("sub-new", None).await.unwrap();
        assert!(
            c.carry_over_single_user(&path, "sub-new")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_failed_adoption_takes_the_carried_rows_back_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engram.db").to_string_lossy().to_string();
        single_user_file(&path).await;

        let c = Control::memory().await.unwrap();
        c.provision("sub-new", None).await.unwrap();
        c.carry_over_single_user(&path, "sub-new").await.unwrap();
        c.discard_carried_over("sub-new").await;
        c.delete_user("sub-new").await.unwrap();

        assert!(c.active_tokens().await.unwrap().is_empty());
        assert!(c.get_session("sid-live").await.unwrap().is_none());
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&c.pool)
            .await
            .unwrap();
        assert_eq!(jobs, 0, "the queue rows did not cascade off the user");
    }

    #[tokio::test]
    async fn a_tenant_store_carries_the_control_handle() {
        let store = crate::store::Store::memory().await.unwrap();
        store.control.provision("sub-1", None).await.unwrap();
        // Two: the one just provisioned, and the test subject every
        // `Store::memory()` runs as so that `jobs.subject` has something to
        // point at.
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
