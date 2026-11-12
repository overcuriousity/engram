use super::control::Control;
use super::now;
use crate::error::Result;
use sqlx::Row;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub subject: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    /// What asked for this token. `None` for one minted before it was
    /// recorded, and for a caller that sent no `User-Agent` — which is most
    /// scripts, and honest of them.
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredToken {
    pub id: String,
    pub subject: String,
    pub token_hash: String,
}

impl Control {
    pub async fn insert_token(
        &self,
        id: &str,
        name: &str,
        hash: &str,
        subject: &str,
        user_agent: Option<&str>,
    ) -> Result<ApiToken> {
        let t = ApiToken {
            id: id.to_string(),
            name: name.to_string(),
            subject: subject.to_string(),
            created_at: now(),
            last_used_at: None,
            revoked_at: None,
            user_agent: user_agent.map(str::to_string),
        };
        sqlx::query!(
            "INSERT INTO api_tokens (id, name, token_hash, subject, created_at, user_agent, scheme)
             VALUES (?, ?, ?, ?, ?, ?, 2)",
            &t.id,
            &t.name,
            hash,
            &t.subject,
            t.created_at,
            &t.user_agent
        )
        .execute(&self.pool)
        .await?;
        Ok(t)
    }

    /// The live token with this id, if there is one.
    pub async fn active_token(&self, id: &str) -> Result<Option<StoredToken>> {
        let row = sqlx::query(
            "SELECT id, subject, token_hash FROM api_tokens
             WHERE id = ? AND revoked_at IS NULL AND scheme = 2",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| StoredToken {
            id: r.get("id"),
            subject: r.get("subject"),
            token_hash: r.get("token_hash"),
        }))
    }

    /// One subject's tokens, newest first.
    ///
    /// The predicate is not a nicety. `api_tokens` used to live in the single
    /// user's own database, where "every row" and "my rows" were the same set;
    /// it is one instance-wide table now, and an unfiltered listing is every
    /// tenant's token ids, names, user-agents and last-used times rendered on
    /// whoever opened Settings.
    pub async fn list_tokens(&self, subject: &str) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(
            "SELECT id, name, subject, created_at, last_used_at, revoked_at, user_agent
             FROM api_tokens WHERE subject = ? ORDER BY created_at DESC",
        )
        .bind(subject)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ApiToken {
                id: r.get("id"),
                name: r.get("name"),
                subject: r.get("subject"),
                created_at: r.get("created_at"),
                last_used_at: r.get("last_used_at"),
                revoked_at: r.get("revoked_at"),
                user_agent: r.get("user_agent"),
            })
            .collect())
    }

    pub async fn touch_token(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE api_tokens SET last_used_at = ? WHERE id = ?",
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Revoke one of `subject`'s tokens, and say whether there was one.
    ///
    /// Scoped for the same reason the listing is, and the consequence is worse:
    /// an id-only UPDATE lets any signed-in user kill another tenant's
    /// extension pairing. `false` rather than an error, so the caller decides
    /// whether an unknown id is a 404 or nothing at all — from the outside the
    /// two cases must look the same, or the answer enumerates other people's
    /// token ids.
    pub async fn revoke_token(&self, id: &str, subject: &str) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND subject = ?",
            now(),
            id,
            subject
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Un-revoke one of `subject`'s tokens revoked after `since`; `false` when there is none.
    pub async fn restore_token(&self, id: &str, subject: &str, since: i64) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE api_tokens SET revoked_at = NULL
             WHERE id = ? AND subject = ? AND revoked_at IS NOT NULL AND revoked_at >= ?",
            id,
            subject,
            since
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }
}
