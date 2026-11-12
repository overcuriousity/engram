use super::Identity;
use crate::error::{Error, Result};
use crate::store::auth::ApiToken;
use crate::store::control::Control;
use argon2::Argon2;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine;

pub const TOKEN_PREFIX: &str = "engram_";

fn hash_secret(secret: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::Store(format!("hashing failed: {e}")))
}

pub fn verify_secret(secret: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// How long a revoked token can still be restored.
pub const RESTORE_WINDOW_SECS: i64 = 30 * 24 * 3600;

/// `engram_<id>.<secret>`: the id addresses one row, the secret is argon2-verified against it.
fn parse(presented: &str) -> Option<(&str, &str)> {
    let (id, secret) = presented.strip_prefix(TOKEN_PREFIX)?.split_once('.')?;
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

/// Mint a token. The plaintext is returned exactly once; only the secret's
/// argon2id hash is persisted. `user_agent` tells same-named tokens apart.
pub async fn mint(
    control: &Control,
    name: &str,
    subject: &str,
    user_agent: Option<&str>,
) -> Result<(ApiToken, String)> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let id = crate::store::new_id();
    let row = control
        .insert_token(&id, name, &hash_secret(&secret)?, subject, user_agent)
        .await?;
    tracing::info!(token_id = %id, name, "api token minted");
    Ok((row, format!("{TOKEN_PREFIX}{id}.{secret}")))
}

pub async fn verify(control: &Control, presented: &str) -> Result<Identity> {
    let Some((id, secret)) = parse(presented) else {
        return Err(Error::Unauthorized);
    };
    let Some(t) = control.active_token(id).await? else {
        return Err(Error::Unauthorized);
    };
    if !verify_secret(secret, &t.token_hash) {
        return Err(Error::Unauthorized);
    }
    control.touch_token(&t.id).await?;
    Ok(Identity {
        subject: t.subject,
        email: None,
        // A token is not a conversation. See `Identity::session`.
        session: None,
    })
}

/// Revoke one of `subject`'s tokens. `Error::NotFound` when that subject has
/// no such token — the same answer a made-up id gets, so the route cannot be
/// used to find out which ids belong to somebody else.
pub async fn revoke(control: &Control, id: &str, subject: &str) -> Result<()> {
    if control.revoke_token(id, subject).await? {
        return Ok(());
    }
    Err(Error::NotFound)
}

/// Undo a revoke made in the last 30 days.
pub async fn restore(control: &Control, id: &str, subject: &str) -> Result<()> {
    let since = crate::store::now() - RESTORE_WINDOW_SECS;
    if control.restore_token(id, subject, since).await? {
        return Ok(());
    }
    Err(Error::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::control::Control;

    #[tokio::test]
    async fn a_minted_token_has_prefix_id_dot_secret() {
        let s = Control::memory().await.unwrap();
        let (row, plaintext) = mint(&s, "laptop", "user-1", None).await.unwrap();
        let (id, secret) = parse(&plaintext).unwrap();
        assert_eq!(id, row.id);
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(secret)
                .unwrap()
                .len(),
            32
        );
    }

    #[tokio::test]
    async fn verifying_reads_one_row_by_id() {
        let s = Control::memory().await.unwrap();
        let (_, a) = mint(&s, "a", "user-1", None).await.unwrap();
        let (_, b) = mint(&s, "b", "user-2", None).await.unwrap();
        let (a_id, _) = parse(&a).unwrap();
        let (_, b_secret) = parse(&b).unwrap();
        let crossed = format!("{TOKEN_PREFIX}{a_id}.{b_secret}");
        assert!(matches!(
            verify(&s, &crossed).await,
            Err(Error::Unauthorized)
        ));
        assert_eq!(verify(&s, &b).await.unwrap().subject, "user-2");
    }

    #[tokio::test]
    async fn a_token_without_an_id_is_unauthorized() {
        let s = Control::memory().await.unwrap();
        let (_, t) = mint(&s, "a", "user-1", None).await.unwrap();
        let secret = parse(&t).unwrap().1.to_string();
        for bad in [
            format!("{TOKEN_PREFIX}{secret}"),
            format!("{TOKEN_PREFIX}.{secret}"),
            format!("{TOKEN_PREFIX}nope.{secret}"),
        ] {
            assert!(
                matches!(verify(&s, &bad).await, Err(Error::Unauthorized)),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn a_revoked_token_can_be_restored_within_30_days() {
        let s = Control::memory().await.unwrap();
        let (row, t) = mint(&s, "a", "user-1", None).await.unwrap();
        revoke(&s, &row.id, "user-1").await.unwrap();
        assert!(verify(&s, &t).await.is_err());
        assert!(matches!(
            restore(&s, &row.id, "user-2").await,
            Err(Error::NotFound)
        ));
        restore(&s, &row.id, "user-1").await.unwrap();
        assert_eq!(verify(&s, &t).await.unwrap().subject, "user-1");

        revoke(&s, &row.id, "user-1").await.unwrap();
        sqlx::query!(
            "UPDATE api_tokens SET revoked_at = revoked_at - ?",
            RESTORE_WINDOW_SECS + 1
        )
        .execute(&s.pool)
        .await
        .unwrap();
        assert!(matches!(
            restore(&s, &row.id, "user-1").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_minted_token_verifies_once_and_is_never_retrievable() {
        let s = Control::memory().await.unwrap();
        let (row, plaintext) = mint(&s, "laptop", "user-1", None).await.unwrap();

        assert!(plaintext.starts_with(TOKEN_PREFIX));
        assert!(
            plaintext.len() > 40,
            "insufficient entropy: {}",
            plaintext.len()
        );

        let id = verify(&s, &plaintext).await.unwrap();
        assert_eq!(id.subject, "user-1");

        // Only the hash is stored, and it is not the token.
        let stored = s.list_tokens("user-1").await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, row.id);
        let raw: String = sqlx::query_scalar!("SELECT token_hash FROM api_tokens")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert!(!raw.contains(&plaintext));
        assert!(!raw.contains(plaintext.split_once('.').unwrap().1));
        assert!(raw.starts_with("$argon2"));
    }

    #[tokio::test]
    async fn a_wrong_token_is_unauthorized() {
        let s = Control::memory().await.unwrap();
        mint(&s, "laptop", "user-1", None).await.unwrap();
        for bad in [
            "engram_wrongwrongwrongwrongwrongwrongwrongwrong",
            "garbage",
            "",
        ] {
            assert!(
                matches!(
                    verify(&s, bad).await,
                    Err(crate::error::Error::Unauthorized)
                ),
                "accepted {bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_revoked_token_stops_working() {
        let s = Control::memory().await.unwrap();
        let (row, plaintext) = mint(&s, "laptop", "user-1", None).await.unwrap();
        revoke(&s, &row.id, "user-1").await.unwrap();
        assert!(matches!(
            verify(&s, &plaintext).await,
            Err(crate::error::Error::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn two_tokens_are_distinct() {
        let s = Control::memory().await.unwrap();
        let (_, a) = mint(&s, "one", "user-1", None).await.unwrap();
        let (_, b) = mint(&s, "two", "user-1", None).await.unwrap();
        assert_ne!(a, b);
        assert_eq!(verify(&s, &a).await.unwrap().subject, "user-1");
        assert_eq!(verify(&s, &b).await.unwrap().subject, "user-1");
    }

    #[tokio::test]
    async fn verification_records_last_use() {
        let s = Control::memory().await.unwrap();
        let (row, plaintext) = mint(&s, "laptop", "user-1", None).await.unwrap();
        assert!(row.last_used_at.is_none());
        verify(&s, &plaintext).await.unwrap();
        let after = s.list_tokens("user-1").await.unwrap();
        assert!(after[0].last_used_at.is_some());
    }

    #[tokio::test]
    async fn a_listing_shows_only_that_subjects_tokens() {
        let s = Control::memory().await.unwrap();
        let (mine, _) = mint(&s, "one", "alice", None).await.unwrap();
        mint(&s, "two", "bob", None).await.unwrap();

        let listed = s.list_tokens("alice").await.unwrap();
        assert_eq!(listed.len(), 1, "the listing crossed a tenant boundary");
        assert_eq!(listed[0].id, mine.id);
    }

    #[tokio::test]
    async fn revoking_another_subjects_token_is_refused_and_leaves_it_working() {
        // Not a listing leak but a write: an id-only revoke lets any signed-in
        // user kill somebody else's extension pairing.
        let s = Control::memory().await.unwrap();
        let (bobs, plaintext) = mint(&s, "bob's laptop", "bob", None).await.unwrap();

        assert!(matches!(
            revoke(&s, &bobs.id, "alice").await,
            Err(Error::NotFound)
        ));
        assert_eq!(verify(&s, &plaintext).await.unwrap().subject, "bob");

        // And the owner still can.
        revoke(&s, &bobs.id, "bob").await.unwrap();
        assert!(matches!(
            verify(&s, &plaintext).await,
            Err(Error::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn revoking_an_id_that_does_not_exist_answers_like_someone_elses() {
        // The two have to be indistinguishable, or the answer enumerates other
        // people's token ids.
        let s = Control::memory().await.unwrap();
        mint(&s, "one", "bob", None).await.unwrap();
        assert!(matches!(
            revoke(&s, "no-such-id", "alice").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_token_belonging_to_another_subject_carries_that_subject() {
        // Identity must come from the stored row, never from the request.
        let s = Control::memory().await.unwrap();
        let (_, a) = mint(&s, "one", "alice", None).await.unwrap();
        let (_, b) = mint(&s, "two", "bob", None).await.unwrap();
        assert_eq!(verify(&s, &a).await.unwrap().subject, "alice");
        assert_eq!(verify(&s, &b).await.unwrap().subject, "bob");
    }
}
