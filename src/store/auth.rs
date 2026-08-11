use super::{Store, now};
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
}

#[derive(Debug, Clone)]
pub struct StoredToken {
    pub id: String,
    pub subject: String,
    pub token_hash: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub subject: String,
    pub email: Option<String>,
    pub expires_at: i64,
}

impl Store {
    pub async fn insert_token(
        &self,
        id: &str,
        name: &str,
        hash: &str,
        subject: &str,
    ) -> Result<ApiToken> {
        let t = ApiToken {
            id: id.to_string(),
            name: name.to_string(),
            subject: subject.to_string(),
            created_at: now(),
            last_used_at: None,
            revoked_at: None,
        };
        sqlx::query(
            "INSERT INTO api_tokens (id, name, token_hash, subject, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&t.id).bind(&t.name).bind(hash).bind(&t.subject).bind(t.created_at)
        .execute(&self.pool).await?;
        Ok(t)
    }

    pub async fn active_tokens(&self) -> Result<Vec<StoredToken>> {
        let rows =
            sqlx::query("SELECT id, subject, token_hash FROM api_tokens WHERE revoked_at IS NULL")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .iter()
            .map(|r| StoredToken {
                id: r.get("id"),
                subject: r.get("subject"),
                token_hash: r.get("token_hash"),
            })
            .collect())
    }

    pub async fn list_tokens(&self) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(
            "SELECT id, name, subject, created_at, last_used_at, revoked_at
             FROM api_tokens ORDER BY created_at DESC",
        )
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
            })
            .collect())
    }

    pub async fn touch_token(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn revoke_token(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ?")
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_session(
        &self,
        id: &str,
        subject: &str,
        email: Option<&str>,
        ttl: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, subject, email, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id).bind(subject).bind(email).bind(now() + ttl).bind(now())
        .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let row = sqlx::query(
            "SELECT id, subject, email, expires_at FROM sessions WHERE id = ? AND expires_at > ?",
        )
        .bind(id)
        .bind(now())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Session {
            id: r.get("id"),
            subject: r.get("subject"),
            email: r.get("email"),
            expires_at: r.get("expires_at"),
        }))
    }

    pub async fn extend_session(&self, id: &str, ttl: i64) -> Result<()> {
        sqlx::query("UPDATE sessions SET expires_at = ? WHERE id = ?")
            .bind(now() + ttl)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_session(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn purge_expired_sessions(&self) -> Result<u64> {
        let r = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}
