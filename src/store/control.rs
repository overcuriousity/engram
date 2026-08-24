//! The control plane: identity and scheduling.
//!
//! One database for the whole instance, holding what is about people rather
//! than knowledge. Every knowledge table lives in a per-tenant database that
//! never learns other tenants exist, which is what makes isolation structural:
//! there is no query anywhere that could be written without a tenant filter,
//! because no tenant filter exists.

use crate::error::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

/// The tenant key: a hex SHA-256 prefix of the OIDC subject.
///
/// Not derived from the email, which can change, and not the subject itself,
/// which may contain anything at all -- including characters that are neither
/// a legal filename nor a legal Qdrant collection name. Sixteen hex digits is
/// 64 bits: this is a naming scheme and not a secret, and the `UNIQUE` on
/// `slug` turns the collision nobody will ever see into an error rather than
/// into two people quietly sharing a database.
pub fn slug_for(subject: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(subject.as_bytes());
    hex::encode(&digest[..8])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub subject: String,
    pub email: Option<String>,
    pub slug: String,
    pub can_judge: bool,
    pub created_at: i64,
    pub last_seen_at: i64,
}

impl User {
    fn from_row(r: &sqlx::sqlite::SqliteRow) -> User {
        User {
            subject: r.get("subject"),
            email: r.get("email"),
            slug: r.get("slug"),
            can_judge: r.get::<i64, _>("can_judge") != 0,
            created_at: r.get("created_at"),
            last_seen_at: r.get("last_seen_at"),
        }
    }
}

#[derive(Clone)]
pub struct Control {
    pub pool: sqlx::SqlitePool,
}

impl Control {
    pub async fn connect(path: &str) -> Result<Control> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        let control = Control { pool };
        control.migrate().await?;
        Ok(control)
    }

    /// Fresh in-memory control database, for the tests. One connection, for
    /// the reason `Store::memory` gives: every `sqlite::memory:` connection is
    /// a separate database, so a multi-connection pool would see different
    /// data per query.
    pub async fn memory() -> Result<Control> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| crate::error::Error::Store(e.to_string()))?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let control = Control { pool };
        control.migrate().await?;
        Ok(control)
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("control_schema.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Idempotent by construction. Two concurrent first requests for the same
    /// unseen subject both run this: `INSERT OR IGNORE` means one row, and the
    /// `SELECT` afterwards means both callers get it.
    pub async fn provision(&self, subject: &str, email: Option<&str>) -> Result<User> {
        let now = super::now();
        sqlx::query(
            "INSERT OR IGNORE INTO users (subject, email, slug, can_judge, created_at, last_seen_at)
             VALUES (?, ?, ?, 0, ?, ?)",
        )
        .bind(subject)
        .bind(email)
        .bind(slug_for(subject))
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.user(subject)
            .await?
            .ok_or_else(|| crate::error::Error::Store(format!("could not provision `{subject}`")))
    }

    pub async fn user(&self, subject: &str) -> Result<Option<User>> {
        Ok(sqlx::query("SELECT * FROM users WHERE subject = ?")
            .bind(subject)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| User::from_row(&r)))
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        Ok(sqlx::query("SELECT * FROM users ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(User::from_row)
            .collect())
    }

    /// `false` when there is no such subject, so the grant CLI can say so
    /// rather than report success on a typo nobody will ever log in as.
    pub async fn set_can_judge(&self, subject: &str, on: bool) -> Result<bool> {
        Ok(sqlx::query("UPDATE users SET can_judge = ? WHERE subject = ?")
            .bind(i64::from(on))
            .bind(subject)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    pub async fn delete_user(&self, subject: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM users WHERE subject = ?")
            .bind(subject)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Last seen, for `--list-users`. Deliberately not a policy input:
    /// dormancy is handled by the sweeps backing off when they find nothing,
    /// not by a cutoff on this column.
    pub async fn touch(&self, subject: &str) -> Result<()> {
        sqlx::query("UPDATE users SET last_seen_at = ? WHERE subject = ?")
            .bind(super::now())
            .bind(subject)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_stable_hex_and_safe_in_a_filename() {
        let a = slug_for("https://idp.example/sub|1234");
        assert_eq!(a, slug_for("https://idp.example/sub|1234"));
        assert_ne!(a, slug_for("https://idp.example/sub|1235"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn provisioning_twice_makes_one_user() {
        let c = Control::memory().await.unwrap();
        let first = c.provision("sub-1", Some("a@example.org")).await.unwrap();
        let again = c.provision("sub-1", Some("a@example.org")).await.unwrap();
        assert_eq!(first.slug, again.slug);
        assert_eq!(first.created_at, again.created_at);
        assert_eq!(c.users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_new_user_may_not_judge() {
        let c = Control::memory().await.unwrap();
        assert!(!c.provision("sub-1", None).await.unwrap().can_judge);
    }

    #[tokio::test]
    async fn granting_and_revoking_judge_is_visible_immediately() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        assert!(c.set_can_judge("sub-1", true).await.unwrap());
        assert!(c.user("sub-1").await.unwrap().unwrap().can_judge);
        assert!(c.set_can_judge("sub-1", false).await.unwrap());
        assert!(!c.user("sub-1").await.unwrap().unwrap().can_judge);
    }

    #[tokio::test]
    async fn granting_to_an_unknown_subject_says_so_rather_than_inventing_one() {
        let c = Control::memory().await.unwrap();
        assert!(!c.set_can_judge("nobody", true).await.unwrap());
        assert!(c.users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_user_removes_the_row() {
        let c = Control::memory().await.unwrap();
        c.provision("sub-1", None).await.unwrap();
        assert!(c.delete_user("sub-1").await.unwrap());
        assert!(c.user("sub-1").await.unwrap().is_none());
        assert!(!c.delete_user("sub-1").await.unwrap());
    }

    #[tokio::test]
    async fn last_seen_moves_and_created_at_does_not() {
        let c = Control::memory().await.unwrap();
        let before = c.provision("sub-1", None).await.unwrap();
        c.touch("sub-1").await.unwrap();
        let after = c.user("sub-1").await.unwrap().unwrap();
        assert_eq!(before.created_at, after.created_at);
        assert!(after.last_seen_at >= before.last_seen_at);
    }
}
