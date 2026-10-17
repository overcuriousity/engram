//! The server lock: one file beside the control database, held for as long as
//! the server runs.
//!
//! A restore replaces files by `rename`, and a running server would keep the
//! old inode open and go on writing to a file nobody sees. Nothing else tells a
//! second process that a server is up, so the server takes this at startup and
//! `--backup --offline` (and, later, `--restore`) refuse while it is held.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Held for its lifetime; dropping it releases the lock.
#[derive(Debug)]
pub struct ServerLock {
    _file: File,
}

/// `<control_path>.lock`. Empty, and not part of any backup.
pub fn lock_path(control_path: &Path) -> PathBuf {
    let mut p = control_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Take the lock, or say who holds it.
pub fn acquire(control_path: &Path) -> Result<ServerLock> {
    let path = lock_path(control_path);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| {
            Error::Store(format!(
                "could not open the lock at {}: {e}",
                path.display()
            ))
        })?;
    match file.try_lock() {
        Ok(()) => Ok(ServerLock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(Error::Validation(format!(
            "engram is running (lock at {}); stop it first",
            path.display()
        ))),
        Err(TryLockError::Error(e)) => Err(Error::Store(format!(
            "could not take the lock at {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_refuses_a_second_acquirer() {
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("engram-control.db");
        let held = acquire(&control).unwrap();
        let e = acquire(&control).unwrap_err().to_string();
        assert!(e.contains("is running"), "{e}");
        assert!(e.contains("engram-control.db.lock"), "{e}");
        drop(held);
        assert!(acquire(&control).is_ok(), "dropping the guard releases it");
    }
}
