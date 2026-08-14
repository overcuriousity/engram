pub mod artifacts;
pub mod auth;
pub mod corpora;
pub mod feedback;
pub mod jobs;
pub mod pairs;
pub mod segments;
pub mod shingle;

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
    /// already has it — then `ADDED_COLUMNS` appends the few columns that
    /// arrived after a table was already deployed, since `IF NOT EXISTS` cannot.
    ///
    /// It still checks afterwards. A table that already exists is otherwise left
    /// as it is, columns and all, so a database from before a column was added
    /// would survive this call and fail much later, in a request, with a bare
    /// `ColumnNotFound` panic that says nothing about the real cause.
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("schema.sql");

        // Columns that arrived after their table was already deployed.
        //
        // `schema.sql` says what the schema *is*, and `CREATE TABLE IF NOT
        // EXISTS` is a no-op against a table that is already there — so a column
        // added to that file never reaches a running base, and the check below
        // then refuses to start, correctly, saying the database is older than
        // the schema. That is the right answer while nothing is deployed and the
        // wrong one afterwards: it asks an operator to recreate a knowledge base
        // to gain a column with a default.
        //
        // So each such column is named here once. SQLite's `ALTER TABLE` can
        // only append, and only with a default, which is exactly the shape of
        // every entry this list is allowed to hold. Adding one is safe; changing
        // or reordering one is not. A column needing more than an append does
        // not belong here — it belongs in a recreate.
        const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
            ("jobs", "seq", "INTEGER NOT NULL DEFAULT 0"),
            (
                "artifact_pairs",
                "judge_unreadable",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            // Nullable, and rightly so: a corpus captured before the extension
            // and link doors existed was read from nowhere this can name.
            ("corpora", "source_url", "TEXT"),
        ];

        // Before the schema, not after. `schema.sql` builds an index over `seq`,
        // and an index cannot name a column that is not there yet — applying the
        // file first fails on exactly the databases this list exists to rescue.
        // On a fresh one the table does not exist yet, the loop skips, and the
        // schema creates it with the column already in place.
        for (table, column, decl) in ADDED_COLUMNS {
            let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            // An empty list means the table does not exist yet, in which case
            // `schema.sql` just created it with the column already in place.
            if have.is_empty() || have.iter().any(|h| h.eq_ignore_ascii_case(column)) {
                continue;
            }
            // A table name cannot be a bind parameter, so this statement has to
            // be built as a string, and sqlx rightly demands the assertion be
            // made explicitly. The audit it asks for: all three parts come from
            // `ADDED_COLUMNS` above, which is a compile-time constant in this
            // file. No caller, request or database value reaches it.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE {table} ADD COLUMN {column} {decl}"
            )))
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
            tracing::info!(table, column, "added a column to an existing database");
        }

        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;

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
        Ok(())
    }

    /// Fresh in-memory database with the schema applied. For the tests, and
    /// for tooling whose output is a file rather than a running instance —
    /// `eval-prepare` segments a corpus and writes JSON, and has no reason to
    /// leave a database behind.
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
    async fn a_deployed_base_gains_source_url_without_being_recreated() {
        // The capture-surfaces column. A knowledge base is expensive to
        // rebuild — every artifact in it was paid for in GPU time — so a
        // nullable column with no default must never be the reason an operator
        // is asked to recreate one.
        let store = Store::memory().await.unwrap();
        sqlx::query("ALTER TABLE corpora DROP COLUMN source_url")
            .execute(&store.pool)
            .await
            .expect("the fixture needs a corpora table without source_url");

        store
            .migrate()
            .await
            .expect("migrate refused a base captured before source_url existed");

        let has: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('corpora') WHERE name = 'source_url'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(has, 1, "source_url was never added to the existing table");

        // And a capture still works against the upgraded base.
        let c = store
            .insert_corpus_with_signature("alpha\n\nbeta", "web", None, vec![], None)
            .await
            .unwrap()
            .into_corpus();
        assert_eq!(store.get_corpus(&c.id).await.unwrap().source_url, None);
    }

    #[tokio::test]
    async fn a_column_added_after_deployment_reaches_a_database_that_predates_it() {
        // The upgrade path, and the trap in it. `CREATE TABLE IF NOT EXISTS` is
        // a no-op against a table that already exists, so a column added to
        // schema.sql never reaches a running base — and the check at the end of
        // migrate() then refuses to start it. An operator would have been asked
        // to recreate a knowledge base to gain a column with a default.
        let store = Store::memory().await.unwrap();
        // The index names the column, so it goes first — which is the same
        // ordering constraint migrate() itself has to respect.
        sqlx::query("DROP INDEX IF EXISTS idx_jobs_claim2")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE jobs DROP COLUMN seq")
            .execute(&store.pool)
            .await
            .expect("the fixture needs a jobs table without seq");

        store
            .migrate()
            .await
            .expect("migrate refused an older base");

        let has_seq: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name = 'seq'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(has_seq, 1, "seq was never added to the existing table");

        // And the default has to be usable, or every pre-existing row would sort
        // as NULL and the claim ordering would be undefined for them.
        store
            .enqueue(jobs::Stage::Embed, "corpus", "c-1")
            .await
            .unwrap();
        let seq = store
            .job_seq(jobs::Stage::Embed, "c-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seq, 0);
    }

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
}
