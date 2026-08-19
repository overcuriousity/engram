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
pub mod segments;
pub mod shingle;

use crate::config::StoreConfig;
use crate::error::Result;
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
    pub async fn migrate(&self) -> Result<()> {
        const SCHEMA: &str = include_str!("schema.sql");

        sqlx::raw_sql(SCHEMA)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))?;
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
