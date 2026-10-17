//! Copying a live SQLite database, and reading the counts a manifest records.
//!
//! Every database here is in WAL mode, so a `cp` of the `.db` while the server
//! runs misses whatever is still in the `-wal` and can tear a page mid-write.
//! `VACUUM INTO` runs inside one read transaction and writes a whole, compact
//! file with no WAL of its own. The source is opened read-only and with no
//! schema setup: `Store::connect` and `Control::connect` migrate, and a backup
//! must not write to what it copies.

use super::create_private;
use crate::error::{Error, Result};
use crate::store::artifacts::{ALL_ARTIFACT_IDS, EMBEDDED_ARTIFACT_IDS};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{ConnectOptions, Connection, Row};
use std::path::Path;

async fn open_read_only(path: &Path) -> Result<SqliteConnection> {
    if !path.is_file() {
        return Err(Error::Store(format!(
            "{}: no such database",
            path.display()
        )));
    }
    SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(std::time::Duration::from_secs(5))
        .connect()
        .await
        .map_err(|e| Error::Store(format!("{}: {e}", path.display())))
}

/// A consistent copy of `src` at `dst`, which must not exist yet.
///
/// `dst` is created empty and `0600` first — `VACUUM INTO` accepts an empty
/// file — so the copy is never readable by anyone else, not even while it is
/// being written.
pub async fn vacuum_into(src: &Path, dst: &Path) -> Result<()> {
    let dst_str = dst
        .to_str()
        .ok_or_else(|| Error::Validation(format!("{} is not UTF-8", dst.display())))?;
    let mut conn = open_read_only(src).await?;
    drop(create_private(dst)?);
    let copied = sqlx::query("VACUUM INTO ?")
        .bind(dst_str)
        .execute(&mut conn)
        .await
        .map_err(|e| Error::Store(format!("VACUUM INTO {}: {e}", dst.display())));
    let _ = conn.close().await;
    if copied.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    copied.map(|_| ())
}

/// `(artifacts, embedded)` in a tenant database: the same queries the
/// store-drift heal reads, counted.
pub async fn counts(db: &Path) -> Result<(u64, u64)> {
    let mut conn = open_read_only(db).await?;
    let artifacts = count(&mut conn, ALL_ARTIFACT_IDS).await?;
    let embedded = count(&mut conn, EMBEDDED_ARTIFACT_IDS).await?;
    let _ = conn.close().await;
    Ok((artifacts, embedded))
}

/// `sql` is one of the two crate constants above, never input.
async fn count(conn: &mut SqliteConnection, sql: &'static str) -> Result<u64> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM ({sql})"
    )))
    .fetch_one(conn)
    .await?;
    Ok(row.get::<i64, _>("n") as u64)
}

/// Every user's slug in a control database, in order.
pub async fn users_in(control: &Path) -> Result<Vec<String>> {
    let mut conn = open_read_only(control).await?;
    let rows = sqlx::query("SELECT slug FROM users ORDER BY slug")
        .fetch_all(&mut conn)
        .await?;
    let _ = conn.close().await;
    Ok(rows.iter().map(|r| r.get::<String, _>("slug")).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifacts::NewArtifact;
    use crate::store::control::Control;
    use std::os::unix::fs::PermissionsExt;

    /// A WAL tenant database at `path` with three artifacts, one embedded.
    pub(crate) async fn tenant_with_three(path: &Path) -> Store {
        let s = Store::connect(
            path.to_str().unwrap(),
            Control::memory().await.unwrap(),
            "sub",
        )
        .await
        .unwrap();
        let c = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &c.id,
                &(0..3)
                    .map(|i| NewArtifact {
                        ordinal: i,
                        text: format!("artifact {i}"),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE artifacts SET embed_state = 'embedded' WHERE id = ?")
            .bind(&made[0].id)
            .execute(&s.pool)
            .await
            .unwrap();
        s
    }

    fn sidecar(db: &Path, ext: &str) -> std::path::PathBuf {
        let mut p = db.as_os_str().to_owned();
        p.push(ext);
        p.into()
    }

    #[tokio::test]
    async fn copy_is_consistent_under_a_live_writer() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("alice.db");
        let store = tenant_with_three(&live).await;
        // The committed rows are still in the WAL: the pool is open, so nothing
        // has checkpointed them into the `.db`. A file copy would miss them.
        assert!(sidecar(&live, "-wal").exists());

        // A second writer, mid-transaction: it has deleted everything and not
        // committed. The copy must show the committed state, not this.
        let mut writer = SqliteConnectOptions::new()
            .filename(&live)
            .connect()
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut writer)
            .await
            .unwrap();
        sqlx::query("DELETE FROM artifacts")
            .execute(&mut writer)
            .await
            .unwrap();

        let copy = dir.path().join("copy.db");
        vacuum_into(&live, &copy).await.unwrap();

        assert!(
            !sidecar(&copy, "-wal").exists(),
            "a VACUUM INTO copy has no WAL"
        );
        assert_eq!(counts(&copy).await.unwrap(), (3, 1));
        sqlx::query("ROLLBACK").execute(&mut writer).await.unwrap();
        drop(store);
    }

    #[tokio::test]
    async fn copy_works_on_a_cleanly_closed_wal_database() {
        // The `--offline` case: the server has stopped, its last connection
        // checkpointed and removed the sidecars, and the copy is taken from a
        // read-only connection that may not create them.
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("alice.db");
        tenant_with_three(&live).await.pool.close().await;
        assert!(!sidecar(&live, "-wal").exists());
        assert!(!sidecar(&live, "-shm").exists());

        let before = std::fs::read(&live).unwrap();
        let copy = dir.path().join("copy.db");
        vacuum_into(&live, &copy).await.unwrap();
        assert_eq!(counts(&copy).await.unwrap(), (3, 1));
        assert_eq!(
            std::fs::read(&live).unwrap(),
            before,
            "the source was not written"
        );
        // SQLite opens a WAL database's sidecars even read-only, and a reader
        // may not remove them on close. An empty `-wal` holds no frame to
        // replay, so what is left is inert; the next opener reuses it.
        let wal = std::fs::metadata(sidecar(&live, "-wal")).map_or(0, |m| m.len());
        assert_eq!(wal, 0, "a reader wrote frames into the source's WAL");
    }

    #[tokio::test]
    async fn copy_has_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("alice.db");
        let _store = tenant_with_three(&live).await;
        let copy = dir.path().join("copy.db");
        vacuum_into(&live, &copy).await.unwrap();
        let mode = std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[tokio::test]
    async fn a_copy_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("alice.db");
        let _store = tenant_with_three(&live).await;
        let copy = dir.path().join("copy.db");
        std::fs::write(&copy, b"keep").unwrap();
        assert!(vacuum_into(&live, &copy).await.is_err());
        assert_eq!(std::fs::read(&copy).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn users_are_read_from_a_control_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.db");
        let control = Control::connect(path.to_str().unwrap()).await.unwrap();
        let b = control.provision("sub-b", None).await.unwrap();
        let a = control
            .provision("sub-a", Some("a@example.com"))
            .await
            .unwrap();
        let copy = dir.path().join("copy.db");
        vacuum_into(&path, &copy).await.unwrap();
        let mut want = vec![a.slug, b.slug];
        want.sort();
        assert_eq!(users_in(&copy).await.unwrap(), want);
    }
}
