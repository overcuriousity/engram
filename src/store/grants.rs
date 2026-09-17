//! Pairing grants: the short-lived, single-use codes a QR carries.
//!
//! A grant is not a token. It lives two minutes, is spent by one claim, and
//! is exchanged for the real credential over TLS — see `auth::grants`. The
//! table holds only a hash of the code, so a reader of the database learns
//! nothing a scanner of the screen did not.

use super::control::Control;
use super::now;
use crate::error::Result;
use sqlx::Row;

impl Control {
    pub async fn insert_grant(
        &self,
        id: &str,
        code_hash: &str,
        subject: &str,
        expires_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO pair_grants (id, code_hash, subject, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(code_hash)
        .bind(subject)
        .bind(now())
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Spend a grant, and say whose it was.
    ///
    /// One statement, so two claims racing on the same code cannot both win:
    /// the row is marked claimed only if it is unclaimed and unexpired, and
    /// `RETURNING` hands back the subject of the row this call changed —
    /// nothing, if some other call or the clock got there first. The three
    /// ways to get `None` are deliberately one answer; the route above turns
    /// them into one status, so a guesser learns nothing from the difference.
    pub async fn claim_grant(&self, code_hash: &str) -> Result<Option<String>> {
        let t = now();
        let row = sqlx::query(
            "UPDATE pair_grants SET claimed_at = ?
             WHERE code_hash = ? AND claimed_at IS NULL AND expires_at > ?
             RETURNING subject",
        )
        .bind(t)
        .bind(code_hash)
        .bind(t)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("subject")))
    }

    pub async fn purge_expired_grants(&self) -> Result<u64> {
        let r = sqlx::query("DELETE FROM pair_grants WHERE expires_at <= ?")
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_grant_is_claimed_once() {
        let c = Control::memory().await.unwrap();
        c.insert_grant("g1", "hash-1", "alice", now() + 120)
            .await
            .unwrap();
        assert_eq!(
            c.claim_grant("hash-1").await.unwrap().as_deref(),
            Some("alice")
        );
        // The same code again is nobody's: the row is spent.
        assert_eq!(c.claim_grant("hash-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_unknown_code_claims_nothing() {
        let c = Control::memory().await.unwrap();
        assert_eq!(c.claim_grant("never-issued").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_expired_grant_cannot_be_claimed() {
        let c = Control::memory().await.unwrap();
        c.insert_grant("g1", "hash-1", "alice", now() - 1)
            .await
            .unwrap();
        assert_eq!(c.claim_grant("hash-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn purging_removes_the_expired_and_keeps_the_live() {
        let c = Control::memory().await.unwrap();
        c.insert_grant("old", "hash-old", "alice", now() - 1)
            .await
            .unwrap();
        c.insert_grant("live", "hash-live", "alice", now() + 120)
            .await
            .unwrap();
        assert_eq!(c.purge_expired_grants().await.unwrap(), 1);
        assert_eq!(
            c.claim_grant("hash-live").await.unwrap().as_deref(),
            Some("alice")
        );
    }
}
