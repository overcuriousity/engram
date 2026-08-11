use super::Identity;
use crate::config::OidcConfig;
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

pub const PENDING_TTL_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub struct PendingAuth {
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub created_at: i64,
}

pub struct PendingStore {
    inner: Mutex<HashMap<String, PendingAuth>>,
}

impl PendingStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn put(&self, p: PendingAuth) {
        let mut m = self.inner.lock().unwrap();
        let cutoff = crate::store::now() - PENDING_TTL_SECS;
        m.retain(|_, v| v.created_at > cutoff);
        m.insert(p.csrf.clone(), p);
    }

    pub fn take(&self, csrf: &str) -> Option<PendingAuth> {
        let mut m = self.inner.lock().unwrap();
        let p = m.remove(csrf)?;
        if p.created_at < crate::store::now() - PENDING_TTL_SECS {
            return None;
        }
        Some(p)
    }
}

impl Default for PendingStore {
    fn default() -> Self {
        Self::new()
    }
}

pub fn is_allowed(cfg: &OidcConfig, subject: &str, email: Option<&str>, groups: &[String]) -> bool {
    if cfg.allowed_subs.iter().any(|s| s == subject) {
        return true;
    }
    if let Some(e) = email {
        let e = e.to_ascii_lowercase();
        if cfg
            .allowed_emails
            .iter()
            .any(|a| a.to_ascii_lowercase() == e)
        {
            return true;
        }
    }
    if cfg
        .allowed_groups
        .iter()
        .any(|g| groups.iter().any(|m| m == g))
    {
        return true;
    }
    false
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct GroupClaims {
    #[serde(default)]
    groups: Vec<String>,
}
impl openidconnect::AdditionalClaims for GroupClaims {}

type EngramIdTokenFields = openidconnect::IdTokenFields<
    GroupClaims,
    openidconnect::EmptyExtraTokenFields,
    openidconnect::core::CoreGenderClaim,
    openidconnect::core::CoreJweContentEncryptionAlgorithm,
    openidconnect::core::CoreJwsSigningAlgorithm,
>;
type EngramTokenResponse =
    openidconnect::StandardTokenResponse<EngramIdTokenFields, openidconnect::core::CoreTokenType>;

type EngramClient<
    HasAuthUrl = openidconnect::EndpointNotSet,
    HasDeviceAuthUrl = openidconnect::EndpointNotSet,
    HasIntrospectionUrl = openidconnect::EndpointNotSet,
    HasRevocationUrl = openidconnect::EndpointNotSet,
    HasTokenUrl = openidconnect::EndpointNotSet,
    HasUserInfoUrl = openidconnect::EndpointNotSet,
> = openidconnect::Client<
    GroupClaims,
    openidconnect::core::CoreAuthDisplay,
    openidconnect::core::CoreGenderClaim,
    openidconnect::core::CoreJweContentEncryptionAlgorithm,
    openidconnect::core::CoreJsonWebKey,
    openidconnect::core::CoreAuthPrompt,
    openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    EngramTokenResponse,
    openidconnect::core::CoreTokenIntrospectionResponse,
    openidconnect::core::CoreRevocableToken,
    openidconnect::core::CoreRevocationErrorResponse,
    HasAuthUrl,
    HasDeviceAuthUrl,
    HasIntrospectionUrl,
    HasRevocationUrl,
    HasTokenUrl,
    HasUserInfoUrl,
>;

pub struct OidcClient {
    metadata: openidconnect::core::CoreProviderMetadata,
    http: openidconnect::reqwest::Client,
    cfg: OidcConfig,
}

impl OidcClient {
    pub async fn discover(cfg: &OidcConfig) -> Result<OidcClient> {
        use openidconnect::IssuerUrl;
        use openidconnect::core::CoreProviderMetadata;

        if cfg.allowed_subs.is_empty()
            && cfg.allowed_emails.is_empty()
            && cfg.allowed_groups.is_empty()
        {
            return Err(Error::Validation(
                "auth.oidc has an empty allowlist: set allowed_subs, allowed_emails or \
                 allowed_groups, otherwise every account in your identity provider could sign in"
                    .into(),
            ));
        }

        let http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| Error::Validation(e.to_string()))?;

        let issuer = IssuerUrl::new(cfg.issuer_url.clone())
            .map_err(|e| Error::Validation(format!("issuer_url: {e}")))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|e| Error::Validation(format!("OIDC discovery failed: {e}")))?;

        tracing::info!(issuer = %cfg.issuer_url, "OIDC provider discovered");
        Ok(OidcClient {
            metadata,
            http,
            cfg: cfg.clone(),
        })
    }

    pub fn authorize_url(&self) -> Result<(String, PendingAuth)> {
        use openidconnect::core::CoreResponseType;
        use openidconnect::{
            AuthenticationFlow, ClientId, ClientSecret, CsrfToken, Nonce, PkceCodeChallenge,
            RedirectUrl, Scope,
        };

        let client = EngramClient::from_provider_metadata(
            self.metadata.clone(),
            ClientId::new(self.cfg.client_id.clone()),
            self.cfg.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.cfg.redirect_url.clone())
                .map_err(|e| Error::Validation(format!("redirect_url: {e}")))?,
        );

        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = client.authorize_url(
            AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for s in &self.cfg.scopes {
            req = req.add_scope(Scope::new(s.clone()));
        }
        let (url, csrf, nonce) = req.set_pkce_challenge(challenge).url();

        Ok((
            url.to_string(),
            PendingAuth {
                csrf: csrf.secret().clone(),
                nonce: nonce.secret().clone(),
                pkce_verifier: verifier.secret().clone(),
                created_at: crate::store::now(),
            },
        ))
    }

    pub async fn exchange(
        &self,
        pending: &PendingAuth,
        code: &str,
        state: &str,
    ) -> Result<Identity> {
        use openidconnect::{
            AuthorizationCode, ClientId, ClientSecret, Nonce, OAuth2TokenResponse,
            PkceCodeVerifier, RedirectUrl, TokenResponse,
        };

        if state != pending.csrf {
            return Err(Error::Unauthorized);
        }

        let client = EngramClient::from_provider_metadata(
            self.metadata.clone(),
            ClientId::new(self.cfg.client_id.clone()),
            self.cfg.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.cfg.redirect_url.clone())
                .map_err(|e| Error::Validation(format!("redirect_url: {e}")))?,
        );

        let tokens = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| Error::Validation(format!("token endpoint unavailable: {e}")))?
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier.clone()))
            .request_async(&self.http)
            .await
            .map_err(|e| Error::Validation(format!("token exchange failed: {e}")))?;

        let id_token = tokens
            .id_token()
            .ok_or_else(|| Error::Validation("provider returned no ID token".into()))?;
        let claims = id_token
            .claims(
                &client.id_token_verifier(),
                &Nonce::new(pending.nonce.clone()),
            )
            .map_err(|e| Error::Validation(format!("ID token rejected: {e}")))?;

        let subject = claims.subject().to_string();
        let mut email = claims.email().map(|e| e.to_string());
        let mut groups = claims.additional_claims().groups.clone();

        if email.is_none() || groups.is_empty() {
            match client.user_info(
                tokens.access_token().clone(),
                Some(claims.subject().clone()),
            ) {
                Ok(req) => match req
                    .request_async::<GroupClaims, _, openidconnect::core::CoreGenderClaim>(
                        &self.http,
                    )
                    .await
                {
                    Ok(info) => {
                        if email.is_none() {
                            email = info.email().map(|e| e.to_string());
                        }
                        if groups.is_empty() {
                            groups = info.additional_claims().groups.clone();
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "userinfo request failed; continuing with the ID token's claims only");
                    }
                },
                Err(e) => {
                    tracing::debug!(error = %e, "provider has no userinfo endpoint");
                }
            }
        }

        if !is_allowed(&self.cfg, &subject, email.as_deref(), &groups) {
            tracing::warn!(%subject, ?groups, "sign-in denied: subject not on the allowlist");
            return Err(Error::Forbidden);
        }
        tracing::info!(%subject, "oidc sign-in");
        Ok(Identity { subject, email })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OidcConfig;

    fn cfg(subs: &[&str], emails: &[&str]) -> OidcConfig {
        cfg_with_groups(subs, emails, &[])
    }

    fn cfg_with_groups(subs: &[&str], emails: &[&str], groups: &[&str]) -> OidcConfig {
        OidcConfig {
            issuer_url: "https://idp.example".into(),
            client_id: "engram".into(),
            client_secret: Some("s".into()),
            redirect_url: "https://engram.example/auth/callback".into(),
            scopes: vec!["openid".into()],
            allowed_subs: subs.iter().map(|s| s.to_string()).collect(),
            allowed_emails: emails.iter().map(|s| s.to_string()).collect(),
            allowed_groups: groups.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn an_empty_allowlist_denies_everyone() {
        assert!(!is_allowed(
            &cfg(&[], &[]),
            "sub-1",
            Some("me@example.com"),
            &[]
        ));
    }

    #[test]
    fn a_listed_subject_is_allowed() {
        assert!(is_allowed(&cfg(&["sub-1"], &[]), "sub-1", None, &[]));
        assert!(!is_allowed(&cfg(&["sub-1"], &[]), "sub-2", None, &[]));
    }

    #[test]
    fn a_listed_email_is_allowed_case_insensitively() {
        let c = cfg(&[], &["Me@Example.com"]);
        assert!(is_allowed(&c, "sub-9", Some("me@example.com"), &[]));
        assert!(!is_allowed(&c, "sub-9", Some("other@example.com"), &[]));
        assert!(!is_allowed(&c, "sub-9", None, &[]));
    }

    #[test]
    fn a_listed_group_is_allowed() {
        let c = cfg_with_groups(&[], &[], &["engram-users"]);
        assert!(is_allowed(
            &c,
            "sub-9",
            None,
            &["engram-users".to_string(), "other-group".to_string()]
        ));
        assert!(!is_allowed(&c, "sub-9", None, &["other-group".to_string()]));
        assert!(!is_allowed(&c, "sub-9", None, &[]));
    }

    #[test]
    fn pending_auth_is_single_use() {
        let store = PendingStore::new();
        let p = PendingAuth {
            csrf: "state-1".into(),
            nonce: "n".into(),
            pkce_verifier: "v".into(),
            created_at: crate::store::now(),
        };
        store.put(p.clone());
        assert!(store.take("state-1").is_some());
        assert!(
            store.take("state-1").is_none(),
            "replaying a state value must fail"
        );
    }

    #[test]
    fn pending_auth_expires() {
        let store = PendingStore::new();
        store.put(PendingAuth {
            csrf: "old".into(),
            nonce: "n".into(),
            pkce_verifier: "v".into(),
            created_at: crate::store::now() - (PENDING_TTL_SECS + 10),
        });
        assert!(
            store.take("old").is_none(),
            "a stale login attempt must not be redeemable"
        );
    }

    #[test]
    fn unknown_state_is_rejected() {
        assert!(PendingStore::new().take("never-issued").is_none());
    }
}
