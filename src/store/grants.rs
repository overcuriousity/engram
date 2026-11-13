//! Pairing grants: two-minute, single-use codes; only a hash of the code is stored.

use super::control::Control;
use super::now;
use crate::error::Result;

impl Control {
    pub async fn insert_grant(
        &self,
        id: &str,
        code_hash: &str,
        subject: &str,
        expires_at: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO pair_grants (id, code_hash, subject, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?)",
            id,
            code_hash,
            subject,
            now(),
            expires_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One statement, so two racing claims cannot both win. The three `None` cases are
    /// deliberately indistinguishable, so a guesser learns nothing.
    pub async fn claim_grant(&self, code_hash: &str) -> Result<Option<String>> {
        let t = now();
        let row = sqlx::query!(
            "UPDATE pair_grants SET claimed_at = ?
             WHERE code_hash = ? AND claimed_at IS NULL AND expires_at > ?
             RETURNING subject",
            t,
            code_hash,
            t
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.subject))
    }

    pub async fn purge_expired_grants(&self) -> Result<u64> {
        let r = sqlx::query!("DELETE FROM pair_grants WHERE expires_at <= ?", now())
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
