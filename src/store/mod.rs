pub mod actions;
pub mod artifacts;
pub mod asks;
pub mod attachments;
pub mod auth;
pub mod context;
pub mod control;
pub mod corpora;
pub mod eval_runs;
pub mod feedback;
pub mod gaps;
pub mod generations;
pub mod grants;
pub mod insights;
pub mod integrations;
pub mod jobs;
pub mod lineage;
pub mod links;
pub mod moments;
pub mod observations;
pub mod pairs;
pub mod pursuits;
pub mod rehearsals;
pub mod segments;
pub mod shingle;
pub mod sleep_runs;
pub mod sweeps;
pub mod versions;

use crate::error::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

#[derive(Clone)]
pub struct Store {
    pub pool: sqlx::SqlitePool,
    /// Held by every tenant `Store` so the enqueue paths deep in capture need no second handle.
    pub control: control::Control,
    /// Bound into every queue query; the knowledge tables never see it.
    pub subject: String,
}

/// Retires journal entries back to ordinary captures. Idempotent, so it runs on every connect;
/// `json_valid` guards rows whose metadata is not JSON.
const RETIRE_JOURNAL: &str = "
UPDATE corpora
   SET origin = COALESCE(json_extract(metadata, '$.origin_was'), 'web'),
       metadata = json_remove(metadata, '$.origin_was', '$.day')
 WHERE origin = 'journal' AND json_valid(metadata);
UPDATE corpora SET origin = 'web'
 WHERE origin = 'journal';
UPDATE corpora
   SET metadata = json_remove(metadata, '$.intent')
 WHERE json_valid(metadata) AND json_extract(metadata, '$.intent') = 'journal';
UPDATE corpora
   SET metadata = json_remove(metadata, '$.entry_refused')
 WHERE json_valid(metadata) AND json_extract(metadata, '$.entry_refused') IS NOT NULL;
";

impl Store {
    pub async fn connect(path: &str, control: control::Control, subject: &str) -> Result<Store> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .create_if_missing(true)
            // WAL lets readers proceed during writes, which matters because the
            // worker pool writes constantly while the UI reads.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            // Four, not eight: a hundred open tenants at eight each is a file-descriptor problem.
            .max_connections(4)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            control,
            subject: subject.to_string(),
        };
        store.migrate().await?;
        Ok(store)
    }

    pub fn for_subject(&self, subject: &str) -> Store {
        Store {
            subject: subject.to_string(),
            ..self.clone()
        }
    }

    /// Checks columns before applying `schema.sql`: `CREATE TABLE IF NOT EXISTS` keeps old tables,
    /// and the file's index drop would commit before its create failed, outside a transaction.
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("schema.sql");

        let mut missing = missing_columns(&self.pool, SCHEMA).await?;
        // Columns safe to add in place: their default (or NULL) is true of every existing row.
        // Named one by one; any other missing column still refuses.
        const ADDITIVE: [(&str, &str, &str); 22] = [
            (
                "artifacts",
                "trashed_at",
                "ALTER TABLE artifacts ADD COLUMN trashed_at INTEGER",
            ),
            (
                "artifacts",
                "trashed_from",
                "ALTER TABLE artifacts ADD COLUMN trashed_from TEXT",
            ),
            (
                "artifacts",
                "updated_at",
                "ALTER TABLE artifacts ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "artifact_pairs",
                "decided_by",
                "ALTER TABLE artifact_pairs ADD COLUMN decided_by TEXT",
            ),
            (
                "search_events",
                "judged_by",
                "ALTER TABLE search_events ADD COLUMN judged_by TEXT",
            ),
            (
                "search_events",
                "opened_at",
                "ALTER TABLE search_events ADD COLUMN opened_at INTEGER",
            ),
            (
                "artifact_pairs",
                "synthesis_asked",
                "ALTER TABLE artifact_pairs ADD COLUMN synthesis_asked INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "corpora",
                "retired_at",
                "ALTER TABLE corpora ADD COLUMN retired_at INTEGER",
            ),
            (
                "corpora",
                "auto_reread_at",
                "ALTER TABLE corpora ADD COLUMN auto_reread_at INTEGER",
            ),
            // NULL: the reap sweep stamps it fresh before judging; no migration invents a date.
            (
                "artifacts",
                "retired_at",
                "ALTER TABLE artifacts ADD COLUMN retired_at INTEGER",
            ),
            (
                "artifacts",
                "reaped_at",
                "ALTER TABLE artifacts ADD COLUMN reaped_at INTEGER",
            ),
            (
                "moments",
                "moved_from",
                "ALTER TABLE moments ADD COLUMN moved_from INTEGER",
            ),
            (
                "moments",
                "moved_at",
                "ALTER TABLE moments ADD COLUMN moved_at INTEGER",
            ),
            // A moment with no `series_id` counts by the rows on its artifact.
            (
                "moments",
                "series_id",
                "ALTER TABLE moments ADD COLUMN series_id TEXT",
            ),
            (
                "moments",
                "origin_corpus_id",
                "ALTER TABLE moments ADD COLUMN origin_corpus_id TEXT",
            ),
            (
                "observations",
                "event_id",
                "ALTER TABLE observations ADD COLUMN event_id TEXT",
            ),
            (
                "search_candidates",
                "band",
                "ALTER TABLE search_candidates ADD COLUMN band INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "search_candidates",
                "explored_from",
                "ALTER TABLE search_candidates ADD COLUMN explored_from INTEGER",
            ),
            (
                "search_events",
                "generation_id",
                "ALTER TABLE search_events ADD COLUMN generation_id TEXT",
            ),
            (
                "graveyard",
                "vec",
                "ALTER TABLE graveyard ADD COLUMN vec BLOB",
            ),
            (
                "graveyard",
                "embed_model",
                "ALTER TABLE graveyard ADD COLUMN embed_model TEXT",
            ),
            (
                "gap_coverage",
                "covered_by",
                "ALTER TABLE gap_coverage ADD COLUMN covered_by TEXT",
            ),
        ];
        apply_additive(&self.pool, &ADDITIVE, &mut missing).await?;

        if !missing.is_empty() {
            return Err(crate::error::Error::Store(format!(
                "this database is older than the schema: {} missing. \
                 Recreate it, or add the columns by hand.",
                missing.join(", ")
            )));
        }

        // Dropped only after the refusal, so a refused base can still run the previous binary.
        // The index over the two columns must go first or SQLite refuses the drop.
        const REMOVED: [(&str, &str, &str); 2] = [
            (
                "eval_runs",
                "recommended",
                "DROP INDEX IF EXISTS idx_eval_runs_open;
                 ALTER TABLE eval_runs DROP COLUMN recommended",
            ),
            (
                "eval_runs",
                "applied_at",
                "DROP INDEX IF EXISTS idx_eval_runs_open;
                 ALTER TABLE eval_runs DROP COLUMN applied_at",
            ),
        ];
        drop_removed(&self.pool, &REMOVED).await?;

        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;

        sqlx::raw_sql(RETIRE_JOURNAL)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;

        Ok(())
    }

    /// Builds its own in-memory control database too.
    pub async fn memory() -> Result<Store> {
        Store::memory_with(control::Control::memory().await?).await
    }

    pub async fn memory_with(control: control::Control) -> Result<Store> {
        control.provision(TEST_SUBJECT, None).await?;
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .foreign_keys(true);
        // One connection: every `sqlite::memory:` connection is a separate database.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let store = Store {
            pool,
            control,
            subject: TEST_SUBJECT.to_string(),
        };
        store.migrate().await?;
        Ok(store)
    }
}

/// The reference database for `cargo sqlx prepare`: control and tenant schema in one file.
pub async fn dev_schema(path: &std::path::Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let control = control::Control::connect(&path.to_string_lossy()).await?;
    let store = Store {
        pool: control.pool.clone(),
        control: control.clone(),
        subject: String::new(),
    };
    store.migrate().await?;
    control.pool.close().await;
    Ok(())
}

/// Columns `schema` declares that the database lacks, as `table.column`. Parsed from the file
/// (one column per line); a table with no columns is absent and contributes nothing.
pub(crate) async fn missing_columns(pool: &sqlx::SqlitePool, schema: &str) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    for (table, columns) in schema_columns(schema) {
        // The table-valued form takes a bind parameter; the pragma statement would need splicing.
        let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
            .bind(&table)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        if have.is_empty() {
            continue;
        }
        for c in columns {
            if !have.iter().any(|h| h.eq_ignore_ascii_case(&c)) {
                missing.push(format!("{table}.{c}"));
            }
        }
    }
    Ok(missing)
}

/// Strikes each added column off `missing`; what remains is what the caller refuses over.
/// `'static` DDL is sqlx's requirement: the SQL must be written here, not assembled.
pub(crate) async fn apply_additive(
    pool: &sqlx::SqlitePool,
    additive: &[(&'static str, &'static str, &'static str)],
    missing: &mut Vec<String>,
) -> Result<()> {
    for &(table, column, ddl) in additive {
        let key = format!("{table}.{column}");
        let Some(i) = missing.iter().position(|m| *m == key) else {
            continue;
        };
        sqlx::raw_sql(ddl)
            .execute(pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
        tracing::info!(column = %key, "added a column the schema expects");
        missing.remove(i);
    }
    Ok(())
}

/// Takes the DDL rather than generating `ALTER`: a column under an index must lose it first.
/// Skipped where already gone, so it runs on every connect.
pub(crate) async fn drop_removed(
    pool: &sqlx::SqlitePool,
    removed: &[(&'static str, &'static str, &'static str)],
) -> Result<()> {
    for &(table, column, ddl) in removed {
        let have: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        if !have.iter().any(|h| h.eq_ignore_ascii_case(column)) {
            continue;
        }
        sqlx::raw_sql(ddl)
            .execute(pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
        tracing::info!(column = %format!("{table}.{column}"), "dropped a column the schema no longer has");
    }
    Ok(())
}

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
            if let Some(t) = open.take() {
                tables.push(t);
            }
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

/// `user-1` because the web fixtures sign in as it: the door and the data must be one person.
pub const TEST_SUBJECT: &str = "user-1";

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Seconds are the clock, so the row id breaks ties; readers order by `(at, id)`.
/// Written as `at|id`; a bare integer reads as `(at, "")`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    pub at: i64,
    pub id: String,
}

impl Cursor {
    pub fn parse(s: &str) -> Self {
        match s.split_once('|') {
            Some((at, id)) => Cursor {
                at: at.parse().unwrap_or(0),
                id: id.to_string(),
            },
            None => Cursor {
                at: s.parse().unwrap_or(0),
                id: String::new(),
            },
        }
    }

    pub fn encode(&self) -> String {
        format!("{}|{}", self.at, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn table_names(pool: &sqlx::SqlitePool) -> Vec<String> {
        sqlx::query_scalar!(
            r#"SELECT name AS "name!" FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'"#
        )
            .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn dev_schema_builds_a_database_with_both_schemas() {
        let control = control::Control::memory().await.unwrap();
        let store = Store::memory().await.unwrap();
        let ours = table_names(&store.pool).await;
        let clash: Vec<_> = table_names(&control.pool)
            .await
            .into_iter()
            .filter(|t| ours.contains(t))
            .collect();
        assert!(clash.is_empty(), "defined by both schemas: {clash:?}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prepare.db");
        dev_schema(&path).await.unwrap();
        let both = control::Control::connect(&path.to_string_lossy())
            .await
            .unwrap();
        let names = table_names(&both.pool).await;
        assert!(names.contains(&"users".to_string()), "{names:?}");
        assert!(names.contains(&"artifacts".to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn a_base_without_updated_at_gains_the_column_instead_of_being_refused() {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        let store = Store {
            pool,
            control: control::Control::memory().await.unwrap(),
            subject: TEST_SUBJECT.to_string(),
        };
        // The real schema with the column dropped, not a hand-written cut-down table.
        store.migrate().await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO corpora (id, raw_text, origin, content_hash, status, created_at, updated_at)
                  VALUES ('c', 'hours', 'web', 'h', 'ready', 1, 1);
             INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
                  VALUES ('a', 'c', 0, 'hours', 1);
             ALTER TABLE artifacts DROP COLUMN updated_at;",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store.migrate().await.unwrap();
        let (text, stamp): (String, i64) =
            sqlx::query_as("SELECT text, updated_at FROM artifacts WHERE id = 'a'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(text, "hours", "the row was rewritten");
        assert_eq!(stamp, 0, "a stamp nobody has set is not a claim about when");
        // migrate runs on every connect.
        store.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn a_journal_from_before_autonomy_loses_the_columns_that_waited_for_a_press() {
        // `recommended` was `NOT NULL` with no default and failed the idle pass's journal write.
        let store = Store::memory().await.unwrap();
        sqlx::raw_sql(
            "ALTER TABLE eval_runs ADD COLUMN recommended INTEGER NOT NULL DEFAULT 1;
             ALTER TABLE eval_runs ADD COLUMN applied_at INTEGER;
             CREATE INDEX IF NOT EXISTS idx_eval_runs_open
               ON eval_runs(recommended, applied_at, created_at DESC);
             INSERT INTO eval_runs (id, created_at, judged_count, pairs_used, pairs_skipped,
                                    base_params, base_recall, base_mrr,
                                    best_params, best_recall, best_mrr, diff,
                                    recommended, applied_at)
                  VALUES ('r', 1, 50, 12, 0,
                          '{\"recency_weight\":0.05,\"per_source_cap\":3}', 0.7, 0.5,
                          '{\"recency_weight\":0.1,\"per_source_cap\":null}', 0.8, 0.6,
                          '[]', 1, NULL);",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store.migrate().await.unwrap();
        let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('eval_runs')")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(!cols.iter().any(|c| c == "recommended"), "{cols:?}");
        assert!(!cols.iter().any(|c| c == "applied_at"), "{cols:?}");
        let run = store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(run.id, "r", "the journal row survived the drop");
        store
            .record_eval_run(&eval_runs::NewEvalRun {
                judged_count: 0,
                pairs_used: 0,
                pairs_skipped: 0,
                base: Default::default(),
                base_recall: 0.0,
                base_mrr: 0.0,
                best: Default::default(),
                best_recall: 0.0,
                best_mrr: 0.0,
                diff: vec![],
            })
            .await
            .unwrap();
        // migrate runs on every connect.
        store.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn a_base_judged_only_from_the_deck_gains_the_two_search_columns() {
        let store = Store::memory().await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO search_events (id, query, door, query_vec, vec_dim, embed_model,
                                        created_at, judged_at, verdict)
                  VALUES ('e', 'fat32', 'ui', x'00', 0, 'fake', 1, 2, 'gap');
             ALTER TABLE search_events DROP COLUMN judged_by;
             ALTER TABLE search_events DROP COLUMN opened_at;",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store.migrate().await.unwrap();
        let (verdict, by, opened): (String, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT verdict, judged_by, opened_at FROM search_events WHERE id = 'e'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!((verdict.as_str(), by, opened), ("gap", None, None));
    }

    #[tokio::test]
    async fn a_base_that_never_explored_gains_the_borrowed_rank_column() {
        let store = Store::memory().await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO search_events (id, query, door, query_vec, vec_dim, embed_model,
                                        created_at)
                  VALUES ('e', 'fat32', 'ui', x'00', 0, 'fake', 1);
             INSERT INTO corpora (id, raw_text, origin, content_hash, status, created_at,
                                  updated_at)
                  VALUES ('c', 'hours', 'web', 'h', 'ready', 1, 1);
             INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
                  VALUES ('a', 'c', 0, 'hours', 1);
             INSERT INTO search_candidates (event_id, rank, artifact_id, score, shown)
                  VALUES ('e', 0, 'a', 0.5, 1);
             ALTER TABLE search_candidates DROP COLUMN explored_from;",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store.migrate().await.unwrap();
        let (shown, explored): (i64, Option<i64>) = sqlx::query_as(
            "SELECT shown, explored_from FROM search_candidates WHERE event_id = 'e'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            (shown, explored),
            (1, None),
            "the candidate kept its row and gained a rank nobody borrowed"
        );
    }

    #[tokio::test]
    async fn applying_the_schema_twice_changes_nothing() {
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
        // Checked before applying: the drop would commit and the create fail, losing both indexes.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE segments (
               id TEXT PRIMARY KEY, corpus_id TEXT NOT NULL, idx INTEGER NOT NULL,
               start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
               text TEXT NOT NULL DEFAULT '', carry_lines INTEGER NOT NULL DEFAULT 0,
               state TEXT NOT NULL DEFAULT 'pending',
               keep_artifacts INTEGER NOT NULL DEFAULT 0,
               attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = Store {
            pool,
            control: control::Control::memory().await.unwrap(),
            subject: TEST_SUBJECT.to_string(),
        };

        let err = store.migrate().await.unwrap_err().to_string();
        assert!(
            err.contains("older than the schema") && err.contains("segments.no_promote"),
            "the operator has to be told which column is missing, not shown \
             a bare column error from the middle of the file: {err}"
        );
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table'")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(
            tables.len() == 1,
            "a refused migration must not have applied half the file: {tables:?}"
        );
    }

    #[tokio::test]
    async fn a_fresh_file_database_gets_the_whole_schema() {
        let dir = std::env::temp_dir().join(format!("engram-schema-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engram.db");
        let store = Store::connect(
            path.to_str().unwrap(),
            control::Control::memory().await.unwrap(),
            TEST_SUBJECT,
        )
        .await
        .unwrap();
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.0.as_str()).collect();
        for expected in [
            "artifact_pairs",
            "artifacts",
            "corpora",
            "search_candidates",
            "search_events",
            "segments",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} is missing: {names:?}"
            );
        }
        // A tenant database holds nothing about who may read it.
        for control_side in ["users", "sessions", "api_tokens", "jobs"] {
            assert!(
                !names.contains(&control_side),
                "{control_side} belongs to the control database: {names:?}"
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
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // runtime: builds an older schema than the reference database holds.
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
            control: control::Control::memory().await.unwrap(),
            subject: TEST_SUBJECT.to_string(),
        };

        let err = store.migrate().await.unwrap_err().to_string();
        assert!(err.contains("segments.text"), "unhelpful message: {err}");
        assert!(err.contains("segments.carry_lines"), "{err}");
    }

    #[tokio::test]
    async fn a_journal_entry_from_before_comes_back_as_the_capture_it_was() {
        let store = Store::memory().await.unwrap();
        let set = |id: String, origin: &'static str, meta: serde_json::Value| {
            let pool = store.pool.clone();
            async move {
                sqlx::query!(
                    "UPDATE corpora SET origin = ?, metadata = ? WHERE id = ?",
                    origin,
                    meta.to_string(),
                    id
                )
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        let filed = store.insert_corpus("Long day.", "web", None).await.unwrap();
        set(
            filed.id.clone(),
            "journal",
            serde_json::json!({"origin_was": "cli", "intent": "journal", "tz": "Europe/Berlin"}),
        )
        .await;
        let typed = store
            .insert_corpus("Quiet evening.", "web", None)
            .await
            .unwrap();
        set(
            typed.id.clone(),
            "journal",
            serde_json::json!({"day": "2026-09-01"}),
        )
        .await;
        let refused = store
            .insert_corpus("The port is 8443.", "web", None)
            .await
            .unwrap();
        set(
            refused.id.clone(),
            "share",
            serde_json::json!({"entry_refused": true}),
        )
        .await;
        let broken = store.insert_corpus("not json", "web", None).await.unwrap();
        sqlx::query!(
            "UPDATE corpora SET origin = 'journal', metadata = 'nope' WHERE id = ?",
            &broken.id
        )
        .execute(&store.pool)
        .await
        .unwrap();

        store.migrate().await.unwrap();
        store.migrate().await.unwrap();

        let c = store.get_corpus(&filed.id).await.unwrap();
        assert_eq!(c.origin, "cli");
        assert_eq!(c.metadata, serde_json::json!({"tz": "Europe/Berlin"}));
        let c = store.get_corpus(&typed.id).await.unwrap();
        assert_eq!(
            c.origin, "web",
            "a day-page entry has no channel but the web"
        );
        assert_eq!(c.metadata, serde_json::json!({}));
        let c = store.get_corpus(&refused.id).await.unwrap();
        assert_eq!(c.origin, "share");
        assert_eq!(c.metadata, serde_json::json!({}));
        let origin: String =
            sqlx::query_scalar!("SELECT origin FROM corpora WHERE id = ?", &broken.id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            origin, "web",
            "metadata that is not JSON still loses the filing"
        );
    }
}
