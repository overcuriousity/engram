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
pub mod insights;
pub mod jobs;
pub mod lineage;
pub mod links;
pub mod moments;
pub mod pairs;
pub mod pursuits;
pub mod segments;
pub mod shingle;
pub mod sweeps;

use crate::error::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

#[derive(Clone)]
pub struct Store {
    pub pool: sqlx::SqlitePool,
    /// The instance-wide control database: identity, and the job queue.
    ///
    /// Held by every tenant `Store` rather than reached through the registry,
    /// because the enqueue paths are deep inside capture and have no business
    /// carrying a second handle down with them.
    pub control: control::Control,
    /// Whose database this is.
    ///
    /// Bound into every queue query, and the only place a tenant identity
    /// appears anywhere below the web layer. The knowledge tables never see
    /// it: they are already alone in a file of their own.
    pub subject: String,
}

impl Store {
    /// Open one tenant's file. The path is passed rather than read from
    /// [`StoreConfig`]: every base belongs to a user, and `store.dir` plus the
    /// slug is what names it.
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
            // Four rather than eight. A hundred open tenants at eight
            // connections each is a file-descriptor problem, and no single
            // tenant needs eight.
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

    /// The same database under a different subject.
    ///
    /// Used by the registry when it opens a tenant, and by the tests that need
    /// two of them over one queue. Cheap: a `Store` is two pool handles and a
    /// string.
    pub fn for_subject(&self, subject: &str) -> Store {
        Store {
            subject: subject.to_string(),
            ..self.clone()
        }
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
        // One exception to "recreate it", and deliberately a list rather than
        // a rule.
        //
        // The doctrine above is about columns that change *meaning* — a type,
        // a constraint, a default that rewrites what existing rows say. It is
        // the right doctrine and it is not what `artifacts.updated_at` is: a
        // column added beside the others, `NOT NULL DEFAULT 0`, that no row
        // needs to have been written with. Against that, "recreate the base"
        // means re-ingesting every artifact to gain a stamp, which is a price
        // nobody would pay and so a column nobody would add.
        //
        // Named one at a time on purpose. A general "ALTER in anything
        // additive" would make this boot path guess, and the guess would be
        // wrong the first time a column's default is not what its old rows
        // should say. Everything not on this list still recreates.
        const ADDITIVE: [(&str, &str, &str); 4] = [
            (
                "artifacts",
                "updated_at",
                "ALTER TABLE artifacts ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
            ),
            // Nullable and with no default, which is the whole reason it
            // qualifies: NULL on an existing row says "nobody recorded who
            // decided this", which is exactly true of every row written before
            // the column existed. A default would have those rows claim an
            // author they never had.
            (
                "artifact_pairs",
                "decided_by",
                "ALTER TABLE artifact_pairs ADD COLUMN decided_by TEXT",
            ),
            // Both nullable for the same reason: every verdict before these
            // columns came from the deck, and no search before them was opened
            // in a way anything recorded.
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
        ];
        for (table, column, ddl) in ADDITIVE {
            let key = format!("{table}.{column}");
            let Some(i) = missing.iter().position(|m| *m == key) else {
                continue;
            };
            sqlx::raw_sql(ddl)
                .execute(&self.pool)
                .await
                .map_err(|e| crate::error::Error::Store(e.to_string()))?;
            tracing::info!(column = %key, "added a column this schema expects");
            missing.remove(i);
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

        Ok(())
    }

    /// Fresh in-memory database with the schema applied, for the tests.
    ///
    /// Builds its own in-memory control database too, so a test that only
    /// wants a `Store` does not have to know a control plane exists. This is
    /// the seam that keeps every existing test compiling.
    pub async fn memory() -> Result<Store> {
        Store::memory_with(control::Control::memory().await?).await
    }

    /// The same, over a control database the caller already holds -- for the
    /// tests that need two tenants sharing one queue.
    pub async fn memory_with(control: control::Control) -> Result<Store> {
        control.provision(TEST_SUBJECT, None).await?;
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
            control,
            subject: TEST_SUBJECT.to_string(),
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

/// The subject every `Store::memory()` runs as, so that the foreign key on
/// `jobs.subject` is satisfied without every test knowing tenancy exists.
///
/// `user-1` and not something tidier because that is the subject the web
/// fixtures have always signed in as. The identity at the door and the owner
/// of the data behind it have to be one person, or every handler resolves a
/// tenant that holds none of the rows the test just wrote.
pub const TEST_SUBJECT: &str = "user-1";

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
    async fn a_base_without_updated_at_gains_the_column_instead_of_being_refused() {
        // `artifacts.updated_at` is additive: a column beside the others with a
        // default no existing row needs to have been written with. Refusing the
        // base for it meant "re-ingest every artifact you own to gain a
        // stamp", which is a price nobody pays and so a column nobody adds.
        // Everything not on the ADDITIVE list still recreates, which is the
        // line `a_base_older_than_the_schema_is_named_before_anything_is_touched`
        // holds.
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
        // A base as it was before the column existed: the real schema, with
        // the column taken back off. Hand-writing a cut-down `artifacts` would
        // only test a table this application does not have.
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
        // The row that was already there kept its text and gained the default.
        let (text, stamp): (String, i64) =
            sqlx::query_as("SELECT text, updated_at FROM artifacts WHERE id = 'a'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(text, "hours", "the row was rewritten");
        assert_eq!(stamp, 0, "a stamp nobody has set is not a claim about when");
        // And again, because migrate runs on every connect.
        store.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn a_base_judged_only_from_the_deck_gains_the_two_search_columns() {
        // `judged_by` and `opened_at` are nullable and mean, on an old row,
        // exactly what NULL says: the deck gave the verdict, and nobody recorded
        // an open. A base full of deck verdicts must keep them.
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
        // A `segments` table as it was before `no_promote` existed.
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
        // The control plane is not in here. A tenant database holds knowledge
        // and nothing about who may read it, which is what makes the isolation
        // structural rather than a filter somebody has to remember to write.
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
            control: control::Control::memory().await.unwrap(),
            subject: TEST_SUBJECT.to_string(),
        };

        let err = store.migrate().await.unwrap_err().to_string();
        assert!(err.contains("segments.text"), "unhelpful message: {err}");
        assert!(err.contains("segments.carry_lines"), "{err}");
    }
}
