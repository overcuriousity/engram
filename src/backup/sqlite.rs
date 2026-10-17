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

/// `<path>.restoring`: where a restore stages a file beside its target.
pub fn restoring(path: &Path) -> std::path::PathBuf {
    with_suffix(path, ".restoring")
}

fn with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Store(format!("{}: {e}", path.display()))),
    }
}

/// Copy an archived database to `dst` (`0600`), sync it, and check it hashes
/// to `sha256`. The source is an archive copy with no WAL, not a live file,
/// so a byte copy is whole. A `dst` left by an interrupted restore is
/// replaced: staging is the step a rerun repeats.
pub fn stage(src: &Path, dst: &Path, sha256: &str) -> Result<()> {
    let io = |p: &Path, e: std::io::Error| Error::Store(format!("{}: {e}", p.display()));
    remove_if_present(dst)?;
    let staged = (|| {
        let mut from = std::fs::File::open(src).map_err(|e| io(src, e))?;
        let mut to = create_private(dst)?;
        std::io::copy(&mut from, &mut to).map_err(|e| io(dst, e))?;
        to.sync_all().map_err(|e| io(dst, e))?;
        let (got, _) = super::manifest::sha256_file(dst)?;
        if got != sha256 {
            return Err(Error::Store(format!(
                "{}: staged copy hashes to {got}, the manifest says {sha256}",
                dst.display()
            )));
        }
        Ok(())
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    staged
}

/// Put a staged database in place of `target`.
///
/// The target's `-wal` and `-shm` go first: they describe the file being
/// replaced, and SQLite would replay a stale `-wal` into the new one on its
/// next open. The rename is atomic, and the directory is synced so it holds.
pub fn swap(staged: &Path, target: &Path) -> Result<()> {
    for ext in ["-wal", "-shm"] {
        remove_if_present(&with_suffix(target, ext))?;
    }
    std::fs::rename(staged, target).map_err(|e| {
        Error::Store(format!(
            "rename {} to {}: {e}",
            staged.display(),
            target.display()
        ))
    })?;
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| Error::Store(format!("{}: {e}", parent.display())))?;
    }
    Ok(())
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
    async fn a_swap_removes_the_stale_sidecars_before_the_rename() {
        let dir = tempfile::tempdir().unwrap();
        let archived = dir.path().join("archived.db");
        let live = dir.path().join("alice.db");
        // The archive copy: one artifact fewer than the base it replaces.
        {
            let src = dir.path().join("src.db");
            let s = tenant_with_three(&src).await;
            sqlx::query("DELETE FROM artifacts WHERE ordinal = 2")
                .execute(&s.pool)
                .await
                .unwrap();
            vacuum_into(&src, &archived).await.unwrap();
        }
        let (sha, _) = super::super::manifest::sha256_file(&archived).unwrap();
        // The base being replaced, still holding frames in its WAL.
        let store = tenant_with_three(&live).await;
        assert!(sidecar(&live, "-wal").exists());
        let wal = std::fs::read(sidecar(&live, "-wal")).unwrap();
        store.pool.close().await;

        let staged = restoring(&live);
        stage(&archived, &staged, &sha).unwrap();
        // A stale `-wal` and `-shm` beside the target, as a crashed server leaves.
        std::fs::write(sidecar(&live, "-wal"), &wal).unwrap();
        std::fs::write(sidecar(&live, "-shm"), b"stale").unwrap();
        swap(&staged, &live).unwrap();

        assert!(!staged.exists());
        assert!(!sidecar(&live, "-wal").exists(), "stale -wal removed");
        assert!(!sidecar(&live, "-shm").exists(), "stale -shm removed");
        assert_eq!(
            super::super::manifest::sha256_file(&live).unwrap().0,
            sha,
            "the target is the staged file byte for byte"
        );
        assert_eq!(
            counts(&live).await.unwrap(),
            (2, 1),
            "no old frame replayed"
        );
    }

    #[test]
    fn staging_refuses_a_copy_that_does_not_hash_to_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.db");
        std::fs::write(&src, b"bytes").unwrap();
        let dst = dir.path().join("a.db.restoring");
        std::fs::write(&dst, b"left by an interrupted restore").unwrap();
        assert!(stage(&src, &dst, "00").is_err());
        assert!(!dst.exists(), "nothing half-staged is left");
        let (sha, _) = super::super::manifest::sha256_file(&src).unwrap();
        stage(&src, &dst, &sha).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"bytes");
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
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
