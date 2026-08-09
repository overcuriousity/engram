pub mod auth;
pub mod chunks;
pub mod jobs;
pub mod sources;

use crate::config::StoreConfig;
use crate::error::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

#[derive(Clone)]
pub struct Store {
    pub pool: sqlx::SqlitePool,
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
        let store = Store { pool };
        store.migrate().await?;
        Ok(store)
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| crate::error::Error::Store(e.to_string()))
    }

    /// Fresh in-memory database with migrations applied. Test use only.
    #[cfg(test)]
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
        let store = Store { pool };
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
