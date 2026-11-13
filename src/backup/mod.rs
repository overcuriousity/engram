//! Backup, check and restore of control, tenant databases and vectors (docs/superpowers/specs/2026-10-17-backup-restore-design.md).
//! `control.db` holds sessions and the VAPID key, so every directory is `0700` and every file `0600` before its first byte.

pub mod generations;
pub mod lock;
pub mod manifest;
pub mod restore;
pub mod sqlite;
pub mod take;
pub mod verify;

use crate::error::{Error, Result};
use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// `create_new`: an existing name means two writers or a stale archive.
pub fn create_private(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::Store(format!("could not create {}: {e}", path.display())))
}

/// An existing directory is accepted only if empty, and is narrowed to `0700` first.
pub fn create_private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        let mut entries = std::fs::read_dir(path).map_err(|e| {
            Error::Validation(format!("{} is not a directory: {e}", path.display()))
        })?;
        if entries.next().is_some() {
            return Err(Error::Validation(format!(
                "{} is not empty; a backup is written into a new or empty directory only",
                path.display()
            )));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::Store(format!("could not narrow {}: {e}", path.display())))?;
        return Ok(());
    }
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| Error::Store(format!("could not create {}: {e}", path.display())))
}
