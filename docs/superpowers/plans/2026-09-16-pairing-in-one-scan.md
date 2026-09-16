# Pairing in One Scan Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A session-holder presses a button, a QR code appears, the Android app scans it and comes away with a bearer token — without the token ever being drawn on a screen.

**Architecture:** A `pair_grants` table in the control database holds two-minute, single-use codes hashed with SHA-256. `/ui/app` renders the code as an `engram://pair?…` URI inside a server-rendered SVG QR. `POST /api/v1/pair/claim`, unauthenticated, atomically spends the grant and mints a token through the existing `auth::tokens::mint`. An optional `[server] tls_fingerprint` rides along as `f=`.

**Tech Stack:** Rust, axum, sqlx/SQLite, askama, `qrcode` 0.14.1 (svg feature only), `sha2`, `base64`, `hex` — all but `qrcode` already in the tree.

**Spec:** `docs/superpowers/specs/2026-09-16-pairing-in-one-scan-design.md`

## Global Constraints

- Branch: `feat/web-push`, stacked on Part A. Never rebase or touch Part A's commits.
- Grant life: **120 seconds**, a `pub const TTL: i64` in `auth::grants`.
- Code: 32 random bytes, base64url **without padding**; must not start with `engram_`.
- Hash: SHA-256 hex, never argon2 — and the schema comment says why.
- URI: `engram://pair?o=<origin>&c=<code>&v=<CARGO_PKG_VERSION>[&f=<fingerprint>]`, values encoded with `pair::urlencode`.
- Claim failure of any kind (unknown, expired, claimed) is one `401`; empty `device` is `400` before the grant is touched.
- Claim success is `201` with `{ "token", "version" }`.
- QR: inline SVG, error-correction level M, no JavaScript, no image route.
- Every commit message ends with the evidence line (which test command, how many passed) and `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
- Tests go through `crate::web::test_support` and `Control::memory()`; nothing spins a real server.
- Comments in the house voice: say *why*, not *what*; the tree's existing doc comments are the model.

---

### Task 1: The `pair_grants` table and its three store calls

**Files:**
- Modify: `src/store/control_schema.sql` (after the `api_tokens` table, before `-- ── Queue`)
- Create: `src/store/grants.rs`
- Modify: `src/store/mod.rs:5` (add `pub mod grants;` in alphabetical order, after `pub mod generations;`)

**Interfaces:**
- Produces on `Control`:
  - `async fn insert_grant(&self, id: &str, code_hash: &str, subject: &str, expires_at: i64) -> Result<()>`
  - `async fn claim_grant(&self, code_hash: &str) -> Result<Option<String>>` — `Some(subject)` when this call spent the grant, `None` otherwise.
  - `async fn purge_expired_grants(&self) -> Result<u64>`

- [ ] **Step 1: Write the failing tests**

Create `src/store/grants.rs`:

```rust
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
        todo!()
    }

    pub async fn claim_grant(&self, code_hash: &str) -> Result<Option<String>> {
        todo!()
    }

    pub async fn purge_expired_grants(&self) -> Result<u64> {
        todo!()
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
```

Add to `src/store/mod.rs` after `pub mod generations;`:

```rust
pub mod grants;
```

- [ ] **Step 2: Run, expect the schema failure**

Run: `cargo test --lib store::grants:: 2>&1 | tail -20`
Expected: the four tests panic — either at `todo!()` or, once the bodies exist, with `no such table: pair_grants`.

- [ ] **Step 3: Add the table**

In `src/store/control_schema.sql`, after the `api_tokens` `CREATE TABLE … );` and before the `-- ── Queue` rule:

```sql
-- A code drawn on a screen for the app to scan, and nothing more: two
-- minutes of life, spent by one claim, exchanged for a real `api_tokens` row
-- over TLS (`auth::grants::claim`). Only the SHA-256 of the code is kept, so
-- a reader of this table holds nothing a photographer of the screen did not.
-- SHA-256 and not argon2id, deliberately: argon2 exists to slow the guessing
-- of low-entropy secrets, and this one has 256 bits and is dead in two
-- minutes. A fast hash also makes the claim one indexed lookup rather than a
-- pass hashing every live row. Expired rows are purged at every mint — the
-- table only grows when someone presses the button, so no reaper is needed.
CREATE TABLE IF NOT EXISTS pair_grants (
  id         TEXT PRIMARY KEY,
  code_hash  TEXT NOT NULL UNIQUE,
  subject    TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  claimed_at INTEGER
);
```

- [ ] **Step 4: Implement the three calls**

Replace the three `todo!()` bodies in `src/store/grants.rs`:

```rust
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
```

- [ ] **Step 5: Run, expect green, commit**

Run: `cargo test --lib store::grants:: 2>&1 | tail -5`
Expected: `4 passed`.

```bash
git add src/store/control_schema.sql src/store/grants.rs src/store/mod.rs
git commit -m "feat(store): pair_grants, the two-minute single-use code behind the QR

Only the SHA-256 of the code is stored, and the comment says why it is not
argon2. The claim is one UPDATE … RETURNING, so two phones scanning the
same screen cannot both win.

Evidence: cargo test --lib store::grants:: — 4 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Mint and claim in `auth::grants`

**Files:**
- Create: `src/auth/grants.rs`
- Modify: `src/auth/mod.rs:1` (add `pub mod grants;` before `pub mod local;`)

**Interfaces:**
- Consumes: `Control::insert_grant`, `Control::claim_grant`, `Control::purge_expired_grants` (Task 1); `crate::auth::tokens::mint(control, name, subject, user_agent) -> Result<(ApiToken, String)>`.
- Produces:
  - `pub const TTL: i64 = 120;`
  - `pub async fn mint(control: &Control, subject: &str) -> Result<String>` — the plaintext code.
  - `pub async fn claim(control: &Control, code: &str, device: &str, user_agent: Option<&str>) -> Result<String>` — the plaintext token; `Err(Error::Unauthorized)` when the grant does not spend.
  - `pub fn hash_code(code: &str) -> String` (pub for the web tests, which expire a row directly).

- [ ] **Step 1: Write the failing tests**

Create `src/auth/grants.rs`:

```rust
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
    todo!()
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
    todo!()
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
            crate::auth::tokens::verify(&c, &token).await.unwrap().subject,
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
```

Add to `src/auth/mod.rs` as the first line:

```rust
pub mod grants;
```

- [ ] **Step 2: Run, expect panics at `todo!()`**

Run: `cargo test --lib auth::grants:: 2>&1 | tail -20`
Expected: 5 failed, each `not yet implemented`.

- [ ] **Step 3: Implement**

Replace the two `todo!()` bodies:

```rust
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
        .insert_grant(
            &id,
            &hash_code(&code),
            subject,
            crate::store::now() + TTL,
        )
        .await?;
    tracing::info!(grant_id = %id, "pairing grant minted");
    Ok(code)
}

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
```

- [ ] **Step 4: Run, expect green, commit**

Run: `cargo test --lib auth::grants:: 2>&1 | tail -5`
Expected: `5 passed`.

```bash
git add src/auth/grants.rs src/auth/mod.rs
git commit -m "feat(auth): a pairing grant mints, and a claim spends it for a token

The code is 32 random bytes and never starts with engram_, so it cannot
verify as a token. A claim that does not spend is one Unauthorized, for
every reason it might not have.

Evidence: cargo test --lib auth::grants:: — 5 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: `[server] tls_fingerprint`, validated at load

**Files:**
- Modify: `src/config.rs:994-998` (`ServerConfig`), `src/config.rs:2592` (`validate`), `src/config.rs:2904` (`test_default`)
- Modify: `config.example.toml:53-60` (`[server]` section)

**Interfaces:**
- Produces: `ServerConfig.tls_fingerprint: Option<String>` and
  `pub fn fingerprint(&self) -> std::result::Result<Option<String>, String>` on `ServerConfig` — `Ok(Some(b64url_no_pad))` normalised, `Ok(None)` when unset, `Err(message)` when it does not decode to 32 bytes. Task 4 calls it after config load, when it cannot fail.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `src/config.rs`, next to `a_misspelt_default_zone_is_refused_at_startup` (around line 4000), add:

```rust
    #[test]
    fn a_fingerprint_that_is_not_32_bytes_is_refused_at_startup() {
        // The QR carries it and the phone pins on it, so a typo here is a
        // phone that refuses every connection with nothing saying why. The
        // load is the one moment an operator is looking.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let body = MINIMAL.replace(
            "bind = \"127.0.0.1:8080\"",
            "bind = \"127.0.0.1:8080\"\ntls_fingerprint = \"not-a-digest\"",
        );
        let p = write(&dir, &body);
        match Config::load(Some(&p)) {
            Err(ConfigError::Invalid(msg)) => assert!(msg.contains("server.tls_fingerprint"), "{msg}"),
            other => panic!("expected an Invalid error, got {other:?}"),
        }
    }

    #[test]
    fn a_fingerprint_is_normalised_to_unpadded_base64url() {
        // Operators paste what `openssl` prints, which is standard base64
        // with padding. The QR wants the URL-safe unpadded form. Both are
        // accepted; one is stored.
        let padded = base64::engine::general_purpose::STANDARD.encode([0xABu8; 32]);
        let cfg = ServerConfig {
            bind: "127.0.0.1:8080".into(),
            workers: 1,
            tls_fingerprint: Some(padded),
        };
        let got = cfg.fingerprint().unwrap().unwrap();
        assert_eq!(got.len(), 43);
        assert!(!got.contains('='));
        assert!(!got.contains('+') && !got.contains('/'));

        let unset = ServerConfig {
            bind: "127.0.0.1:8080".into(),
            workers: 1,
            tls_fingerprint: None,
        };
        assert_eq!(unset.fingerprint().unwrap(), None);
    }
```

The test module needs `use base64::Engine;` in scope: add it at the top of the tests module if not already there (check with `grep -n "use base64" src/config.rs`).

- [ ] **Step 2: Run, expect a compile failure**

Run: `cargo test --lib config::tests::a_fingerprint 2>&1 | grep -E "error|tls_fingerprint" | head`
Expected: `no field tls_fingerprint` / `no method fingerprint`.

- [ ] **Step 3: Implement**

Replace `ServerConfig` at `src/config.rs:993-998`:

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// The SHA-256 of the SubjectPublicKeyInfo of the certificate a phone is
    /// shown, base64 in either alphabet, padded or not. Unset by default and
    /// meant to stay that way for most deployments: the app pins on first use
    /// during the two-minute pairing window, which is good enough when the
    /// operator is standing at the screen. Set, the QR carries it and the app
    /// pins on it from the first byte. A config key and not an `instance` row
    /// because the certificate belongs to whatever terminates TLS in front of
    /// this process, which the process cannot see.
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
}

impl ServerConfig {
    /// The fingerprint as the QR carries it: base64url, no padding, 43
    /// characters. `Err` names the key so `validate` can refuse it at load.
    pub fn fingerprint(&self) -> std::result::Result<Option<String>, String> {
        use base64::Engine;
        let Some(raw) = self.tls_fingerprint.as_deref().map(str::trim) else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Ok(None);
        }
        let cleaned: String = raw.chars().filter(|c| *c != '=').collect();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&cleaned)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
            .map_err(|_| {
                format!("server.tls_fingerprint {raw:?} is not base64: it should be the SHA-256 of the certificate's SubjectPublicKeyInfo")
            })?;
        if bytes.len() != 32 {
            return Err(format!(
                "server.tls_fingerprint decodes to {} bytes, and a SHA-256 is 32",
                bytes.len()
            ));
        }
        Ok(Some(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        ))
    }
}
```

In `validate` at `src/config.rs:2592`, before the final `Ok(())`:

```rust
        self.server.fingerprint().map_err(ConfigError::Invalid)?;
```

In `test_default` at `src/config.rs:2904`, add the field:

```rust
            server: ServerConfig {
                bind: "127.0.0.1:8080".into(),
                workers: 2,
                tls_fingerprint: None,
            },
```

In `config.example.toml`, after the `workers = 1` line:

```toml
# The SHA-256 of the certificate's SubjectPublicKeyInfo, base64, for the
# certificate a phone is shown when it reaches this deployment. Leave it
# unset and the app pins whatever it sees while pairing, which is right when
# you are standing at the screen. Set it and the pairing code carries it, so
# the app pins the named key from the first byte.
#   openssl x509 -in cert.pem -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64
# tls_fingerprint = ""
```

- [ ] **Step 4: Run, expect green, commit**

Run: `cargo test --lib config:: 2>&1 | tail -5`
Expected: all pass, including the two new ones.

```bash
git add src/config.rs config.example.toml
git commit -m "feat(config): server.tls_fingerprint, refused at load if it is not a SHA-256

Optional; the QR carries it as f= when set. Either base64 alphabet, padded
or not, normalised to the unpadded URL-safe form the URI wants.

Evidence: cargo test --lib config:: — N passed, two of them new.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

(Replace `N` with the number the run prints.)

---

### Task 4: The page, the press and the QR

**Files:**
- Modify: `Cargo.toml` (after the `url = "2"` line)
- Modify: `src/web/pair.rs:196` (`fn urlencode` → `pub(crate) fn urlencode`)
- Create: `src/web/app.rs`
- Create: `src/web/templates/app.html`
- Modify: `src/web/mod.rs:1` (add `pub mod app;` first) and `src/web/mod.rs:150` (merge the router after `extension::extension_router()`)
- Modify: `src/web/templates/extension.html:43-46` (the link)

**Interfaces:**
- Consumes: `auth::grants::mint`, `ServerConfig::fingerprint`, `pair::request_origin`, `pair::urlencode`, `auth_routes::HtmlTemplate`, `ui_error::UiResult`.
- Produces: `pub fn pair_uri(origin: &str, code: &str, fingerprint: Option<&str>) -> String`, `pub fn app_router() -> Router<AppState>`. Task 5 adds `routes()` to this same file.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, after `url = "2"`:

```toml
# The pairing QR, drawn on the server as inline SVG so the page needs no
# script and no image route. Pure Rust. Default features off: `image` pulls
# the raster stack in to write a PNG nobody serves, and `svg` is the
# renderer this page uses, so it is named on its own.
qrcode = { version = "0.14.1", default-features = false, features = ["svg"] }
```

Run: `cargo build 2>&1 | tail -3` — expected: builds, `qrcode` appears in `Cargo.lock`.

- [ ] **Step 2: Write the failing tests**

Create `src/web/app.rs`:

```rust
//! Pairing the app in one scan.
//!
//! A page behind a session draws a QR code; the app scans it and is paired.
//! What the picture carries is a grant, not a token — `auth::grants` says
//! why — and the app trades it for a token through the claim route below.

use crate::error::Error;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::pair::{request_origin, urlencode};
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// The URI the QR carries. `o` is where the phone will point, `c` the grant,
/// `v` this server's version so the app can say *older than I expect*
/// rather than fail strangely, and `f` the operator-named certificate
/// fingerprint, when there is one.
pub fn pair_uri(origin: &str, code: &str, fingerprint: Option<&str>) -> String {
    todo!()
}

/// The QR as inline SVG. Error-correction level M: the code is under 200
/// characters and a laptop screen is a clean surface, so H would only make
/// the modules smaller for nothing.
fn qr_svg(uri: &str) -> Result<String, Error> {
    todo!()
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppTemplate {
    origin: String,
    /// The URI and its picture, present only on the render that follows the
    /// press. `None` on a GET, for the reason `extension.rs` gives: a
    /// credential that appears whenever a page is opened is one nobody
    /// remembers asking for.
    code: Option<(String, String)>,
}

impl AppTemplate {
    /// Reached from Settings, and its token is revoked there.
    fn section(&self) -> &'static str {
        "settings"
    }
}

async fn app_page(_tenant: Tenant, headers: HeaderMap) -> Response {
    todo!()
}

async fn app_grant(
    tenant: Tenant,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> UiResult<Response> {
    todo!()
}

pub fn app_router() -> Router<AppState> {
    Router::new()
        .route("/ui/app", get(app_page))
        .route("/ui/app/grant", post(app_grant))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn with_cookie(method: &str, uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(method)
            .header("cookie", cookie)
            .header("host", "engram.test")
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn the_uri_carries_origin_code_and_version_and_the_fingerprint_only_when_set() {
        let plain = pair_uri("https://engram.test", "abc", None);
        assert_eq!(
            plain,
            format!(
                "engram://pair?o=https%3A%2F%2Fengram.test&c=abc&v={}",
                env!("CARGO_PKG_VERSION")
            )
        );
        let pinned = pair_uri("https://engram.test", "abc", Some("FPFPFP"));
        assert!(pinned.ends_with("&f=FPFPFP"), "{pinned}");
    }

    #[test]
    fn the_qr_is_an_svg_that_encodes_something() {
        let svg = qr_svg("engram://pair?o=x&c=y&v=0").unwrap();
        assert!(svg.starts_with("<svg") || svg.starts_with("<?xml"), "{svg}");
        assert!(svg.contains("<path") || svg.contains("<rect"), "{svg}");
    }

    #[tokio::test]
    async fn the_page_and_the_press_need_a_session() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        for (method, path) in [("GET", "/ui/app"), ("POST", "/ui/app/grant")] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .method(method)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(res.status(), StatusCode::OK, "{method} {path} served a stranger");
        }
    }

    #[tokio::test]
    async fn opening_the_page_mints_nothing() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let res = app
            .oneshot(with_cookie("GET", "/ui/app", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        assert!(!body.contains("engram://"), "a GET drew a code");
        assert!(body.contains("https://engram.test"), "the page says where the phone will point");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pair_grants")
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn pressing_draws_a_code_for_this_origin() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(with_cookie("POST", "/ui/app/grant", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        assert!(
            body.contains("engram://pair?o=https%3A%2F%2Fengram.test&amp;c=")
                || body.contains("engram://pair?o=https%3A%2F%2Fengram.test&c="),
            "no URI for this origin in: {body}"
        );
        assert!(body.contains("<svg"), "no picture");
        // The picture is not the credential.
        assert!(!body.contains("engram_"), "a token was drawn on the page");
    }
}
```

Create `src/web/templates/app.html`:

```html
{% extends "layout.html" %}
{% block title %}Pair the app — engram{% endblock %}
{% block content %}
<h1>Pair the app</h1>
{# The origin is stated because it is the one thing the operator can check:
   the phone will be pointed at exactly this address. #}
<p>This pairs the engram app on a phone with <strong>{{ origin }}</strong>.
Open the app, choose <em>Scan to pair</em>, and point it at the code.</p>
{% match code %}
{% when Some with ((uri, svg)) %}
<div class="qr" style="max-width:256px">{{ svg|safe }}</div>
<p class="mono muted" style="word-break:break-all">{{ uri }}</p>
<p class="muted">Good for two minutes, and for one phone. Press again for
another.</p>
{% when None %}
{% endmatch %}
<form method="post" action="/ui/app/grant">
  <button class="btn btn-accent" type="submit">
    {% if code.is_some() %}Show a new code{% else %}Show a code for this phone{% endif %}
  </button>
</form>
<p class="muted">The picture carries a code, not a credential: the phone
trades it for its own token, which then appears under Settings → API tokens
named for the device, and is revoked there.</p>
{% endblock %}
```

Add `pub mod app;` as the first line of `src/web/mod.rs`, and in the router chain after `.merge(extension::extension_router())`:

```rust
        .merge(app::app_router())
```

In `src/web/pair.rs`, change `fn urlencode(s: &str) -> String` to `pub(crate) fn urlencode(s: &str) -> String`.

- [ ] **Step 3: Run, expect panics at `todo!()`**

Run: `cargo test --lib web::app:: 2>&1 | tail -20`
Expected: compiles; the unit tests panic `not yet implemented`, the router tests fail likewise.

- [ ] **Step 4: Implement**

Replace the four `todo!()` bodies:

```rust
pub fn pair_uri(origin: &str, code: &str, fingerprint: Option<&str>) -> String {
    let mut uri = format!(
        "engram://pair?o={}&c={}&v={}",
        urlencode(origin),
        urlencode(code),
        urlencode(env!("CARGO_PKG_VERSION")),
    );
    if let Some(f) = fingerprint {
        uri.push_str("&f=");
        uri.push_str(&urlencode(f));
    }
    uri
}

fn qr_svg(uri: &str) -> Result<String, Error> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)
        .map_err(|e| Error::Internal(format!("qr: {e}")))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .build())
}

async fn app_page(_tenant: Tenant, headers: HeaderMap) -> Response {
    HtmlTemplate(AppTemplate {
        origin: request_origin(&headers).unwrap_or_default(),
        code: None,
    })
    .into_response()
}

async fn app_grant(
    tenant: Tenant,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> UiResult<Response> {
    let origin = request_origin(&headers).unwrap_or_default();
    let code = crate::auth::grants::mint(&tenant.core.store.control, &tenant.user.subject).await?;
    // Validated at load, so `Err` here is unreachable; `ok().flatten()`
    // rather than an unwrap because a page must not panic over config.
    let fingerprint = st.config.server.fingerprint().ok().flatten();
    let uri = pair_uri(&origin, &code, fingerprint.as_deref());
    let svg = qr_svg(&uri)?;
    Ok(HtmlTemplate(AppTemplate {
        origin,
        code: Some((uri, svg)),
    })
    .into_response())
}
```

`Error::Internal` is the "this server broke" variant (`src/error.rs:53`), and `UiError` implements `From<Error>`, so `?` works on both calls in `app_grant`.

In `src/web/templates/extension.html`, replace the paragraph at lines 43-46 (the one beginning `On Android, install engram from the browser’s menu`) with:

```html
<p>On Android, the engram app is the door: it joins the share sheet from any
browser, holds a capture until the server is reachable, and rings for
reminders. <a href="/ui/app">Pair it in one scan.</a> Installed from the
browser's menu instead, engram joins the share sheet on Chromium only, and
nothing below is needed for that.</p>
```

- [ ] **Step 5: Run, expect green, commit**

Run: `cargo test --lib web::app:: web::extension:: web::pair:: 2>&1 | tail -5`
Expected: all pass.

```bash
git add Cargo.toml Cargo.lock src/web/app.rs src/web/templates/app.html src/web/mod.rs src/web/pair.rs src/web/templates/extension.html
git commit -m "feat(ui): /ui/app draws a pairing code, and the picture is not the credential

A press mints a two-minute grant and renders it as engram://pair?o&c&v[&f]
inside a server-drawn SVG. A GET mints nothing, for the reason the phone
token has always had.

Evidence: cargo test --lib web::app:: — 5 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: `POST /api/v1/pair/claim`

**Files:**
- Modify: `src/web/app.rs` (add the handler, `routes()`, and tests)
- Modify: `src/web/api.rs:1793` (`.merge(crate::web::app::routes())` beside `push::routes()`)

**Interfaces:**
- Consumes: `auth::grants::claim(control, code, device, user_agent) -> Result<String>`, `auth::grants::hash_code`.
- Produces: `pub fn routes() -> Router<AppState>` mounted under `/api/v1`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/web/app.rs`:

```rust
    fn claim_req(body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri("/api/v1/pair/claim")
            .method("POST")
            .header("content-type", "application/json")
            .header("user-agent", "engram-android/0.1 (Pixel 8)")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Press the button and read the code out of the page.
    async fn press_and_read_code(app: &axum::Router, cookie: &str) -> String {
        let res = app
            .clone()
            .oneshot(with_cookie("POST", "/ui/app/grant", cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::body_of(res).await;
        body.split("&c=")
            .nth(1)
            .or_else(|| body.split("&amp;c=").nth(1))
            .map(|rest| rest.split(['&', '<', '"', ' ']).next().unwrap().to_string())
            .expect("a code on the page")
    }

    #[tokio::test]
    async fn a_scanned_code_becomes_a_token_named_for_the_device() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let code = press_and_read_code(&app, &cookie).await;

        let res = app
            .clone()
            .oneshot(claim_req(serde_json::json!({
                "code": code, "device": "engram for Android · Pixel 8"
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body = crate::web::test_support::json_of(res).await;
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        let token = body["token"].as_str().expect("a token").to_string();
        assert!(token.starts_with("engram_"));

        // The token opens the door the app will post to.
        let res = app
            .clone()
            .oneshot(crate::web::api::tests::raw_post(
                "/api/v1/capture",
                &token,
                "text/plain",
                b"shared from the app",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        // And it is listed under the device's name, with its user agent.
        let listed = core.store.control.list_tokens("user-1").await.unwrap();
        let row = listed
            .iter()
            .find(|t| t.name == "engram for Android · Pixel 8")
            .expect("the app's token in the list");
        assert_eq!(row.user_agent.as_deref(), Some("engram-android/0.1 (Pixel 8)"));
    }

    #[tokio::test]
    async fn a_code_claims_once() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let code = press_and_read_code(&app, &cookie).await;
        let body = serde_json::json!({ "code": code, "device": "phone" });
        let first = app.clone().oneshot(claim_req(body.clone())).await.unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);
        let second = app.oneshot(claim_req(body)).await.unwrap();
        assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_expired_or_invented_code_is_unauthorized() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let code = press_and_read_code(&app, &cookie).await;
        // Age the row directly: the clock is not the test's to move.
        sqlx::query("UPDATE pair_grants SET expires_at = ? WHERE code_hash = ?")
            .bind(crate::store::now() - 1)
            .bind(crate::auth::grants::hash_code(&code))
            .execute(&core.store.control.pool)
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(claim_req(serde_json::json!({ "code": code, "device": "phone" })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .oneshot(claim_req(serde_json::json!({ "code": "invented", "device": "phone" })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unnamed_device_is_refused_before_the_code_is_spent() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let code = press_and_read_code(&app, &cookie).await;
        for body in [
            serde_json::json!({ "code": code }),
            serde_json::json!({ "code": code, "device": "   " }),
        ] {
            let res = app.clone().oneshot(claim_req(body)).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
        // The grant survived the malformed requests.
        let res = app
            .oneshot(claim_req(serde_json::json!({ "code": code, "device": "phone" })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }
```

- [ ] **Step 2: Run, expect 404s**

Run: `cargo test --lib web::app::tests::a_scanned 2>&1 | tail -8`
Expected: fails with `left: 404, right: 201`.

- [ ] **Step 3: Implement**

In `src/web/app.rs`, after `app_grant` and before `app_router`, add:

```rust
#[derive(serde::Deserialize)]
pub struct Claim {
    #[serde(default)]
    pub code: String,
    /// How the app names itself — it is the token's name in Settings, and
    /// the only thing telling two phones apart there.
    #[serde(default)]
    pub device: String,
}

/// Spend a scanned code for a token. No bearer: the code is the credential.
///
/// The device name is checked before the grant is touched, so a malformed
/// request does not burn a code the person then has to press for again.
async fn claim(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(c): axum::Json<Claim>,
) -> crate::error::Result<(axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    let device = c.device.trim();
    if device.is_empty() {
        return Err(Error::Validation("device: empty".into()));
    }
    let token = crate::auth::grants::claim(
        st.tenants.control(),
        c.code.trim(),
        device,
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await?;
    Ok((
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "token": token,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    ))
}

/// The API side, mounted under `/api/v1`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/pair/claim", post(claim))
}
```

The handler has no `Tenant` — nobody is signed in yet — so it reads the control database through `Tenants::control()` (`src/tenants.rs:129`), which already exists.

In `src/web/api.rs`, at the `.merge(crate::web::push::routes())` line (around 1793), add directly after it:

```rust
        .merge(crate::web::app::routes())
```

- [ ] **Step 4: Run, expect green, commit**

Run: `cargo test --lib web::app:: 2>&1 | tail -5`
Expected: `9 passed`.

```bash
git add src/web/app.rs src/web/api.rs
git commit -m "feat(api): POST /pair/claim spends a scanned code for a token named for the device

No bearer, because the code is the credential. Unknown, expired and spent
are one 401; an unnamed device is a 400 before the grant is touched.

Evidence: cargo test --lib web::app:: — 9 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: The full run

**Files:** none new.

- [ ] **Step 1: The whole suite, in the background**

Run: `cargo test 2>&1 | tail -30` (background it; it takes several minutes).
Expected: every suite green. A failure elsewhere means a task above changed something shared — `ServerConfig` construction in a test fixture is the likely one; fix it in the task's file and amend nothing, commit as `fix:`.

- [ ] **Step 2: Lint and format**

Run: `cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | sort | uniq -c`
Expected: nothing new. Then `cargo fmt --check`; if it prints a diff, run `cargo fmt` and commit as `style: cargo fmt`.

- [ ] **Step 3: Report**

State the test count, the clippy result and the fmt result. Push the branch: `git push -u origin feat/web-push`. Do not open a PR unless asked; the branch carries Parts A and C together and the person decides when it goes up.
