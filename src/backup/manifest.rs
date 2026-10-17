//! `manifest.json`: what an archive holds, and the hashes that prove it.
//!
//! Written last, after every file it names exists and has been hashed, so a
//! directory without one is an aborted backup. It holds slugs, hashes, counts
//! and versions only — no URL, API key, subject, email, token or key material —
//! and is safe to paste into an issue; the directory beside it is not.
//!
//! Read back, it is untrusted input: unknown fields are refused, and no file
//! name is ever taken from it as a path. Names are rebuilt from a validated
//! slug or alias and must match what the manifest says.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

pub const FORMAT: u32 = 1;
pub const FILE: &str = "manifest.json";
pub const CONTROL_FILE: &str = "control.db";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub created_at: i64,
    pub engram_version: String,
    pub mode: Mode,
    pub embed: EmbedRecipe,
    pub vector_backend: Backend,
    pub control: FileEntry,
    pub tenants: Vec<TenantEntry>,
    /// Slugs the control copy names that this archive does not hold, because
    /// their capture failed. Non-empty means the archive is partial.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Taken beside a running server: reconcilable, not point-in-time.
    Online,
    /// Taken under the server lock: point-in-time.
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Qdrant,
    /// The contained build's: vectors live in the tenant's own `.db`.
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedRecipe {
    pub model: String,
    pub dim: usize,
    /// `EmbedRole::fingerprint`, the string `meta.embed.recipe` holds.
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub users: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantEntry {
    pub slug: String,
    pub db: DbEntry,
    /// `None` when the vectors live in the `.db` itself.
    pub vector: Option<VectorEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbEntry {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub artifacts: u64,
    pub embedded: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorEntry {
    pub alias: String,
    pub generation: String,
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub points: u64,
    pub dim: usize,
}

/// Where a tenant's database goes in the archive.
pub fn tenant_file(slug: &str) -> String {
    format!("tenants/{slug}.db")
}

/// Where an alias' snapshot goes in the archive.
pub fn vector_file(alias: &str) -> String {
    format!("vectors/{alias}.snapshot")
}

/// `[a-z0-9][a-z0-9_-]*`: the shape `Control::provision` gives a slug, and the
/// only one allowed to become part of a path.
pub fn valid_slug(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// An alias is `{vector.collection}_{slug}`, and `vector.collection` is the
/// operator's, so case is allowed; separators and dots are not.
pub(crate) fn valid_alias(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// SHA-256 and length of a file, read in pieces so a snapshot of any size
/// costs one buffer.
pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| Error::Validation(format!("{}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| Error::Store(format!("{}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), bytes))
}

impl Manifest {
    /// Read `manifest.json` from an archive directory.
    pub fn load(dir: &Path) -> Result<Manifest> {
        let path = dir.join(FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::Validation(format!(
                "{}: {e} — a directory without a manifest is an aborted backup",
                path.display()
            ))
        })?;
        let m: Manifest = serde_json::from_str(&text)
            .map_err(|e| Error::Validation(format!("{}: {e}", path.display())))?;
        if m.format != FORMAT {
            return Err(Error::Validation(format!(
                "{}: format {} is not one this binary reads ({FORMAT})",
                path.display(),
                m.format
            )));
        }
        Ok(m)
    }

    /// Every file this manifest names exists under `dir` with the stated
    /// length and SHA-256, and every name is the one rebuilt from its slug or
    /// alias. Nothing is opened as a database here.
    pub fn verify(&self, dir: &Path) -> Result<()> {
        check(
            dir,
            CONTROL_FILE,
            &self.control.file,
            &self.control.sha256,
            self.control.bytes,
        )?;
        let mut seen = std::collections::HashSet::new();
        for t in &self.tenants {
            if !valid_slug(&t.slug) {
                return Err(Error::Validation(format!(
                    "manifest names a tenant {:?}, which is not a slug",
                    t.slug
                )));
            }
            if !seen.insert(t.slug.as_str()) {
                return Err(Error::Validation(format!(
                    "manifest names the tenant {} twice",
                    t.slug
                )));
            }
            check(
                dir,
                &tenant_file(&t.slug),
                &t.db.file,
                &t.db.sha256,
                t.db.bytes,
            )?;
            if let Some(v) = &t.vector {
                if !valid_alias(&v.alias) {
                    return Err(Error::Validation(format!(
                        "manifest names an alias {:?}, which is not a collection name",
                        v.alias
                    )));
                }
                check(dir, &vector_file(&v.alias), &v.file, &v.sha256, v.bytes)?;
            }
        }
        Ok(())
    }
}

/// One file: the name must be the rebuilt one, then length, then hash.
fn check(dir: &Path, expected: &str, named: &str, sha256: &str, bytes: u64) -> Result<()> {
    if named != expected {
        return Err(Error::Validation(format!(
            "manifest names {named:?} where {expected:?} belongs"
        )));
    }
    let path = dir.join(expected);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| Error::Validation(format!("{expected}: {e}")))?;
    if !meta.is_file() {
        return Err(Error::Validation(format!("{expected}: not a regular file")));
    }
    if meta.len() != bytes {
        return Err(Error::Validation(format!(
            "{expected}: {} bytes, manifest says {bytes}",
            meta.len()
        )));
    }
    let (got, _) = sha256_file(&path)?;
    if got != sha256 {
        return Err(Error::Validation(format!(
            "{expected}: SHA-256 does not match the manifest"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Manifest {
        Manifest {
            format: FORMAT,
            created_at: 1_760_700_000,
            engram_version: "0.1.0".into(),
            mode: Mode::Online,
            embed: EmbedRecipe {
                model: "m".into(),
                dim: 4,
                fingerprint: "f".into(),
            },
            vector_backend: Backend::Qdrant,
            control: FileEntry {
                file: CONTROL_FILE.into(),
                sha256: String::new(),
                bytes: 0,
                users: 1,
            },
            tenants: vec![TenantEntry {
                slug: "alice".into(),
                db: DbEntry {
                    file: tenant_file("alice"),
                    sha256: String::new(),
                    bytes: 0,
                    artifacts: 2,
                    embedded: 1,
                },
                vector: Some(VectorEntry {
                    alias: "engram_alice".into(),
                    generation: "engram_alice_g1".into(),
                    file: vector_file("engram_alice"),
                    sha256: String::new(),
                    bytes: 0,
                    points: 1,
                    dim: 4,
                }),
            }],
            skipped: vec![],
        }
    }

    /// `sample()` with its three files written under `dir` and hashed.
    fn on_disk(dir: &Path) -> Manifest {
        let mut m = sample();
        std::fs::create_dir_all(dir.join("tenants")).unwrap();
        std::fs::create_dir_all(dir.join("vectors")).unwrap();
        let put = |name: &str, body: &[u8]| {
            std::fs::write(dir.join(name), body).unwrap();
            sha256_file(&dir.join(name)).unwrap()
        };
        (m.control.sha256, m.control.bytes) = put(CONTROL_FILE, b"control");
        (m.tenants[0].db.sha256, m.tenants[0].db.bytes) = put(&tenant_file("alice"), b"tenant");
        let v = m.tenants[0].vector.as_mut().unwrap();
        (v.sha256, v.bytes) = put(&vector_file("engram_alice"), b"snapshot");
        m
    }

    #[test]
    fn a_manifest_round_trips() {
        let m = sample();
        let text = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&text).unwrap(), m);
        assert!(text.contains("\"mode\":\"online\""), "{text}");
        assert!(text.contains("\"vector_backend\":\"qdrant\""), "{text}");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let mut v = serde_json::to_value(sample()).unwrap();
        v["tenants"][0]["db"]["path"] = "/etc/passwd".into();
        assert!(serde_json::from_value::<Manifest>(v).is_err());
        let mut v = serde_json::to_value(sample()).unwrap();
        v["api_key"] = "k".into();
        assert!(serde_json::from_value::<Manifest>(v).is_err());
    }

    #[test]
    fn a_slug_is_never_a_path() {
        assert!(valid_slug("alice"));
        assert!(valid_slug("a1_b-c"));
        for bad in ["../x", "a/b", "", "-a", "_a", "A", "a.b", ".."] {
            assert!(!valid_slug(bad), "{bad:?}");
        }
    }

    #[test]
    fn verify_passes_on_an_intact_archive() {
        let dir = tempfile::tempdir().unwrap();
        on_disk(dir.path()).verify(dir.path()).unwrap();
    }

    #[test]
    fn verify_names_the_file_whose_byte_was_flipped() {
        let dir = tempfile::tempdir().unwrap();
        let m = on_disk(dir.path());
        let path = dir.path().join(tenant_file("alice"));
        let mut body = std::fs::read(&path).unwrap();
        body[0] ^= 1;
        std::fs::write(&path, body).unwrap();
        let e = m.verify(dir.path()).unwrap_err().to_string();
        assert!(e.contains("tenants/alice.db"), "{e}");
        assert!(e.contains("SHA-256"), "{e}");
    }

    #[test]
    fn verify_refuses_a_file_name_that_is_not_the_rebuilt_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = on_disk(dir.path());
        m.tenants[0].db.file = "../outside.db".into();
        let e = m.verify(dir.path()).unwrap_err().to_string();
        assert!(e.contains("../outside.db"), "{e}");

        let mut m = on_disk(&dir.path().join("again"));
        m.tenants[0].slug = "../x".into();
        let e = m.verify(&dir.path().join("again")).unwrap_err().to_string();
        assert!(e.contains("not a slug"), "{e}");
    }
}
