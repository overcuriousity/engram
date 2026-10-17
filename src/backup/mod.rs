//! Backup as one unit: the control database, every tenant database and every
//! tenant's vectors, in one directory with a checksummed manifest.
//!
//! Design: `docs/superpowers/specs/2026-10-17-backup-restore-design.md`. This
//! module takes, checks and restores an archive, and verifies that the stores
//! agree.
//!
//! The archive is as sensitive as the instance — `control.db` carries sessions,
//! token hashes and the VAPID private key — so every directory here is created
//! `0700` and every file `0600` before its first byte is written.

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

/// A new file only its owner can read, refused if anything is already there.
///
/// `create_new` rather than truncate: an archive is never written over, and a
/// name that already exists means two writers or a stale archive.
pub fn create_private(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::Store(format!("could not create {}: {e}", path.display())))
}

/// A directory only its owner can enter. An existing one is accepted only if it
/// is empty, and is narrowed to `0700` before anything goes into it.
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
