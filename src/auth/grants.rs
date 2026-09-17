//! The grant a QR carries, and the exchange that turns it into a token.
//!
//! The extension is handed a long-lived token because its browser carries
//! it straight into the extension that asked. A code drawn on a screen has
//! no such carrier — anyone in the room can photograph it — so the screen
//! shows a grant instead: two minutes of life, spent by one claim, and the
//! real token is minted only in the POST that spends it, over TLS, to the
//! device that made the request. The credential is never rendered.

use crate::error::{Error, Result};
use crate::store::control::Control;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine;
use sha2::{Digest, Sha256};

/// How long a code on the screen stays claimable. Two minutes is long
/// enough to find the phone and short enough that a photograph of the screen
/// is worthless by the time anyone acts on it.
pub const TTL: i64 = 120;

/// SHA-256, hex. Not argon2id — see the `pair_grants` comment in
/// `control_schema.sql`.
pub fn hash_code(code: &str) -> String {
    hex::encode(Sha256::digest(code.as_bytes()))
}

/// Mint a grant for `subject` and return the code, which is shown once.
pub async fn mint(control: &Control, subject: &str) -> Result<String> {
    // The button is the only thing that grows this table, so its press is
    // where the dead rows go.
    control.purge_expired_grants().await?;
    // OsRng from the rand_core argon2 re-exports, as `tokens::mint` does,
    // so there is one random source in the tree and not two.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let code = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let id = crate::store::new_id();
    control
        .insert_grant(&id, &hash_code(&code), subject, crate::store::now() + TTL)
        .await?;
    tracing::info!(grant_id = %id, "pairing grant minted");
    Ok(code)
}

/// Spend `code` and mint the token it was standing in for, named for the
/// device that claimed it. `Error::Unauthorized` for a code that is unknown,
/// expired or already spent — one answer, so the route enumerates nothing.
pub async fn claim(
    control: &Control,
    code: &str,
    device: &str,
    user_agent: Option<&str>,
) -> Result<String> {
    let Some(subject) = control.claim_grant(&hash_code(code)).await? else {
        return Err(Error::Unauthorized);
    };
    // If this mint fails the grant is spent and the person presses the button
    // again. That is the right direction to fail in: a grant that could be
    // retried after a failure is a grant that can be claimed twice.
    let (_, token) = crate::auth::tokens::mint(control, device, &subject, user_agent).await?;
    tracing::info!(subject = %subject, device, "app paired");
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_code_is_long_random_and_not_a_token() {
        let c = Control::memory().await.unwrap();
        let a = mint(&c, "alice").await.unwrap();
        let b = mint(&c, "alice").await.unwrap();
        assert!(a.len() >= 40, "too short to be 256 bits: {a}");
        assert_ne!(a, b);
        // A grant must never be mistaken for, or verify as, a bearer token.
        assert!(!a.starts_with(crate::auth::tokens::TOKEN_PREFIX));
        assert!(matches!(
            crate::auth::tokens::verify(&c, &a).await,
            Err(Error::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn the_store_holds_the_hash_and_not_the_code() {
        let c = Control::memory().await.unwrap();
        let code = mint(&c, "alice").await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT code_hash FROM pair_grants")
            .fetch_one(&c.pool)
            .await
            .unwrap();
        assert_ne!(stored, code);
        assert_eq!(stored, hash_code(&code));
    }

    #[tokio::test]
    async fn claiming_mints_a_token_named_for_the_device() {
        let c = Control::memory().await.unwrap();
        let code = mint(&c, "alice").await.unwrap();
        let token = claim(&c, &code, "engram for Android", Some("engram-android/0.1"))
            .await
            .unwrap();
        assert_eq!(
            crate::auth::tokens::verify(&c, &token)
                .await
                .unwrap()
                .subject,
            "alice"
        );
        let listed = c.list_tokens("alice").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "engram for Android");
        assert_eq!(listed[0].user_agent.as_deref(), Some("engram-android/0.1"));
    }

    #[tokio::test]
    async fn a_second_claim_and_a_wrong_code_are_both_unauthorized() {
        let c = Control::memory().await.unwrap();
        let code = mint(&c, "alice").await.unwrap();
        claim(&c, &code, "phone", None).await.unwrap();
        assert!(matches!(
            claim(&c, &code, "phone again", None).await,
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            claim(&c, "not-a-code", "phone", None).await,
            Err(Error::Unauthorized)
        ));
        // The failed claims minted nothing.
        assert_eq!(c.list_tokens("alice").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn minting_purges_what_has_expired() {
        let c = Control::memory().await.unwrap();
        c.insert_grant("old", "hash-old", "alice", crate::store::now() - 1)
            .await
            .unwrap();
        mint(&c, "alice").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pair_grants")
            .fetch_one(&c.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "the expired row should be gone");
    }
}
