use super::control::Control;
use super::now;
use crate::error::Result;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub subject: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    /// `None` for older tokens and for callers that sent no `User-Agent`.
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

    pub async fn active_token(&self, id: &str) -> Result<Option<StoredToken>> {
        let row = sqlx::query!(
            r#"SELECT id AS "id!", subject, token_hash FROM api_tokens
             WHERE id = ? AND revoked_at IS NULL AND scheme = 2"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| StoredToken {
            id: r.id,
            subject: r.subject,
            token_hash: r.token_hash,
        }))
    }

    /// `api_tokens` is instance-wide: without the subject filter this lists every tenant's tokens.
    pub async fn list_tokens(&self, subject: &str) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!", name, subject, created_at, last_used_at, revoked_at, user_agent
             FROM api_tokens WHERE subject = ? ORDER BY created_at DESC"#,
            subject
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ApiToken {
                id: r.id,
                name: r.name,
                subject: r.subject,
                created_at: r.created_at,
                last_used_at: r.last_used_at,
                revoked_at: r.revoked_at,
                user_agent: r.user_agent,
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

    /// Scoped by subject so one user cannot revoke another's. An unknown id and another
    /// tenant's id both return `false`, or the answer enumerates token ids.
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
