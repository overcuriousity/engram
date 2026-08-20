pub mod artifacts;
pub mod asks;
pub mod attachments;
pub mod auth;
pub mod corpora;
pub mod feedback;
pub mod gaps;
pub mod jobs;
pub mod lineage;
pub mod links;
pub mod pairs;
pub mod pursuits;
pub mod segments;
pub mod shingle;
pub mod sweeps;

use crate::config::StoreConfig;
use crate::error::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

#[derive(Clone)]
pub struct Store {
    pub pool: sqlx::SqlitePool,
    /// Held for the length of a capture write. `record_search` reads the
    /// previous event and then writes over it, and the UI fires one of these per
    /// keystroke: two overlapping transactions upgrade from read to write on the
    /// same snapshot, which SQLite answers with `SQLITE_BUSY_SNAPSHOT` and no
    /// `busy_timeout` can wait out. Shared by every clone, which is what makes
    /// it a queue rather than eight of them.
    capture: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Store {
    pub async fn connect(cfg: &StoreConfig) -> Result<Store> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cfg.path))
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .create_if_missing(true)
            // WAL lets readers proceed during writes, which matters because the
            // worker pool writes constantly while the UI reads.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            capture: Default::default(),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Bring the database up to the schema this binary expects.
    ///
    /// One statement of what the schema *is*, rather than a chain of diffs
    /// describing how it came to be. Every object is `IF NOT EXISTS`, so this
    /// creates what is missing on a fresh database and is a no-op on one that
    /// already has it. Changing a column means changing `schema.sql` and
    /// recreating the database.
    ///
    /// Which is exactly why it checks first. `CREATE TABLE IF NOT EXISTS`
    /// leaves an existing table as it is, columns and all, so adding a column
    /// to `schema.sql` without recreating the base changes nothing here and
    /// fails much later, inside a request, with a bare `ColumnNotFound` that
    /// names no cause. Nothing is altered to fix that — recreating is still the
    /// answer — but it is said here, at boot, with the columns named.
    ///
    /// The check runs *before* the file rather than after it because the file
    /// is not inert on an old base. It drops a superseded index by name and
    /// creates its replacement over a column such a base does not have, and
    /// `raw_sql` runs statements one after another with no transaction around
    /// them: applied first, the drop commits, the create fails with the bare
    /// `no such column` this message exists to replace, and the base is left
    /// with neither index. Read first, nothing has happened yet.
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("schema.sql");

        let mut missing = Vec::new();
        for (table, columns) in schema_columns(SCHEMA) {
            // The table-valued form of `PRAGMA table_info`, which takes a bind
            // parameter where the pragma statement would need the name spliced
            // into the SQL.
            let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(&table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            // No columns at all means no such table — a fresh base, or a table
            // this schema adds. That is not a base that is behind; it is one
            // the statement below is about to create.
            if have.is_empty() {
                continue;
            }
            for c in columns {
                if !have.iter().any(|h| h.eq_ignore_ascii_case(&c)) {
                    missing.push(format!("{table}.{c}"));
                }
            }
        }
        if !missing.is_empty() {
            return Err(crate::error::Error::Store(format!(
                "this database is older than the schema: {} missing. \
                 Recreate it, or add the columns by hand.",
                missing.join(", ")
            )));
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

    /// Fresh in-memory database with the schema applied, for the tests.
    pub async fn memory() -> Result<Store> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .foreign_keys(true);
        // One connection: every `sqlite::memory:` connection is a separate
        // database, so a multi-connection pool would see different data per query.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            capture: Default::default(),
        };
        store.migrate().await?;
        Ok(store)
    }
}

/// The `(table, columns)` pairs `schema.sql` declares.
///
/// Read out of the file rather than listed by hand, so it cannot fall behind
/// the schema it is checking — a hand-kept list of "columns added recently" is
/// exactly the thing everyone forgets to append to. The file is ours and one
/// column per line, which is what makes this much parsing enough: a line inside
/// a `CREATE TABLE` block starts with the column's name, unless it starts with a
/// comment or a table constraint.
fn schema_columns(sql: &str) -> Vec<(String, Vec<String>)> {
    const CONSTRAINTS: [&str; 5] = ["PRIMARY", "FOREIGN", "UNIQUE", "CHECK", "CONSTRAINT"];
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    let mut open: Option<(String, Vec<String>)> = None;

    for line in sql.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            let name = rest.trim_end_matches('(').trim();
            open = Some((name.to_string(), Vec::new()));
            continue;
        }
        let Some((_, columns)) = open.as_mut() else {
            continue;
        };
        if line.starts_with(')') {
            tables.push(open.take().expect("just borrowed"));
            continue;
        }
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let first = line.split([' ', '(', ',']).next().unwrap_or_default();
        if first.is_empty() || CONSTRAINTS.iter().any(|k| k.eq_ignore_ascii_case(first)) {
            continue;
        }
        columns.push(first.to_string());
    }
    tables
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applying_the_schema_twice_changes_nothing() {
        // migrate() runs on every connect, so a second application has to be a
        // no-op rather than an error. This is what `IF NOT EXISTS` on every
        // object buys, and it is the whole reason the schema can be a
        // statement of shape rather than a chain of diffs.
        let store = Store::memory().await.unwrap();
        let before: Vec<(String, String)> =
            sqlx::query_as("SELECT type, name FROM sqlite_master ORDER BY type, name")
                .fetch_all(&store.pool)
                .await
                .unwrap();

        store.migrate().await.unwrap();
        store.migrate().await.unwrap();

        let after: Vec<(String, String)> =
            sqlx::query_as("SELECT type, name FROM sqlite_master ORDER BY type, name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        assert!(
            before.iter().any(|(_, n)| n == "artifacts"),
            "the schema must actually have been applied"
        );
    }

    #[tokio::test]
    async fn a_base_older_than_the_schema_is_named_before_anything_is_touched() {
        // The check runs before the file, not after it. `schema.sql` drops a
        // superseded index by name and creates its replacement over `class` —
        // so applying it to a base without that column would commit the drop,
        // fail the create with a bare `no such column`, and leave the base with
        // neither index and no idea why it would not boot.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // A `jobs` table as it was before the column existed, and the index the
        // schema means to replace.
        sqlx::raw_sql(
            "CREATE TABLE jobs (
               id INTEGER PRIMARY KEY AUTOINCREMENT, stage TEXT NOT NULL,
               target_kind TEXT NOT NULL, target_id TEXT NOT NULL,
               state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
               run_after INTEGER NOT NULL DEFAULT 0, last_error TEXT,
               claimed_at INTEGER, created_at INTEGER NOT NULL DEFAULT 0,
               seq INTEGER NOT NULL DEFAULT 0, UNIQUE(stage, target_id));
             CREATE INDEX idx_jobs_claim2 ON jobs(state, attempts, seq, id, run_after);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = Store {
            pool,
            capture: Default::default(),
        };

        let err = store.migrate().await.unwrap_err().to_string();
        assert!(
            err.contains("older than the schema") && err.contains("jobs.class"),
            "the operator has to be told which column is missing, not shown \
             a bare column error from the middle of the file: {err}"
        );
        let indexes: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='jobs'")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(
            indexes.iter().any(|(n,)| n == "idx_jobs_claim2"),
            "a refused migration must not have dropped the index the old \
             binary still claims through: {indexes:?}"
        );
    }

    #[tokio::test]
    async fn a_fresh_file_database_gets_the_whole_schema() {
        let dir = std::env::temp_dir().join(format!("engram-schema-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engram.db");
        let cfg = crate::config::StoreConfig {
            path: path.to_str().unwrap().to_string(),
        };

        let store = Store::connect(&cfg).await.unwrap();
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.0.as_str()).collect();
        for expected in [
            "api_tokens",
            "artifact_pairs",
            "artifacts",
            "corpora",
            "jobs",
            "search_candidates",
            "search_events",
            "segments",
            "sessions",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} is missing: {names:?}"
            );
        }

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_schema_parses_into_the_columns_it_declares() {
        let tables = schema_columns(include_str!("schema.sql"));
        let segments = tables
            .iter()
            .find(|(t, _)| t == "segments")
            .expect("segments is in the schema");
        assert_eq!(
            segments.1,
            vec![
                "corpus_id",
                "idx",
                "start_line",
                "end_line",
                "text",
                "carry_lines",
                "state",
                "keep_artifacts",
                "no_promote",
                "attempts",
                "last_error",
            ],
            "comments and the PRIMARY KEY line must not be read as columns"
        );
        assert!(
            tables.len() >= 9,
            "every CREATE TABLE must be found, got {}",
            tables.len()
        );
    }

    #[tokio::test]
    async fn a_database_missing_a_column_is_refused_with_the_column_named() {
        // The failure this replaces: `CREATE TABLE IF NOT EXISTS` leaves an
        // older table exactly as it is, so startup succeeded and every corpus
        // view, synthesis run and reconcile job then panicked on a column that
        // was never added.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE segments (
               corpus_id TEXT NOT NULL, idx INTEGER NOT NULL,
               start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
               state TEXT NOT NULL DEFAULT 'pending',
               attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT,
               PRIMARY KEY (corpus_id, idx))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = Store {
            pool,
            capture: Default::default(),
        };

        let err = store.migrate().await.unwrap_err().to_string();
        assert!(err.contains("segments.text"), "unhelpful message: {err}");
        assert!(err.contains("segments.carry_lines"), "{err}");
    }
}
