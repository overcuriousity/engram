use super::Identity;
use crate::config::OidcConfig;
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// Long enough for a slow provider, short enough that a leaked state value is useless.
pub const PENDING_TTL_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub struct PendingAuth {
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub created_at: i64,
    /// The provider returns only `state`, so the destination waits here for the callback.
    pub next: Option<String>,
}

/// Not persisted: a restart mid-login just means logging in again.
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
        let mut m = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cutoff = crate::store::now() - PENDING_TTL_SECS;
        m.retain(|_, v| v.created_at > cutoff);
        m.insert(p.csrf.clone(), p);
    }

    /// Single use: a replayed `state` finds nothing.
    pub fn take(&self, csrf: &str) -> Option<PendingAuth> {
        let mut m = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// Empty lists admit nobody unless `open_registration` is set, since a new subject gets a tenant provisioned.
/// Admission by subject or group, never by e-mail: a provider may let a user set their own.
pub fn is_allowed(cfg: &OidcConfig, subject: &str, groups: &[String]) -> bool {
    if cfg.allowed_subs.is_empty() && cfg.allowed_groups.is_empty() {
        return cfg.open_registration;
    }
    if cfg.allowed_subs.iter().any(|s| s == subject) {
        return true;
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

/// Absent `groups` (Nextcloud sends it only with group provisioning on) deserializes to an empty list.
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

/// `CoreClient` with [`GroupClaims`] as its additional claims, so `groups` decodes.
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
    /// The client is not stored: its seventeen-parameter typestate would pin one openidconnect release.
    metadata: openidconnect::core::CoreProviderMetadata,
    /// No redirects: it carries credentials. See `discover`.
    http: openidconnect::reqwest::Client,
    cfg: OidcConfig,
    redirect_url: String,
    resource: String,
    jwks: tokio::sync::RwLock<Option<(jsonwebtoken::jwk::JwkSet, std::time::Instant)>>,
}

pub fn callback_url(base: &str) -> String {
    format!("{}/auth/callback", base.trim_end_matches('/'))
}

impl OidcClient {
    pub async fn discover(cfg: &OidcConfig, base: &str) -> Result<OidcClient> {
        use openidconnect::IssuerUrl;
        use openidconnect::core::CoreProviderMetadata;

        // Discovery follows redirects: Nextcloud's nginx recipe 301s the bare .well-known path.
        let discovery_http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| Error::Validation(e.to_string()))?;

        // No redirects after discovery: on 307/308 reqwest would replay `client_secret`,
        // the code or the access token to the redirect target.
        let http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Validation(e.to_string()))?;

        let issuer = IssuerUrl::new(cfg.issuer_url.clone())
            .map_err(|e| Error::Validation(format!("issuer_url: {e}")))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &discovery_http)
            .await
            .map_err(|e| Error::Validation(format!("OIDC discovery failed: {e}")))?;

        tracing::info!(issuer = %cfg.issuer_url, "OIDC provider discovered");
        Ok(OidcClient {
            metadata,
            http,
            cfg: cfg.clone(),
            redirect_url: callback_url(base),
            resource: format!("{}/mcp", base.trim_end_matches('/')),
            jwks: tokio::sync::RwLock::new(None),
        })
    }

    pub fn issuer(&self) -> &str {
        &self.cfg.issuer_url
    }

    pub fn resource(&self) -> &str {
        &self.resource
    }

    async fn jwks(&self, refresh: bool) -> Result<jsonwebtoken::jwk::JwkSet> {
        if let Some((set, at)) = self.jwks.read().await.as_ref()
            && (!refresh || at.elapsed() < std::time::Duration::from_secs(60))
        {
            return Ok(set.clone());
        }
        let uri = self.metadata.jwks_uri().url().clone();
        let body = self
            .http
            .get(uri)
            .send()
            .await
            .map_err(|e| Error::Validation(format!("JWKS unavailable: {e}")))?
            .bytes()
            .await
            .map_err(|e| Error::Validation(format!("JWKS unavailable: {e}")))?;
        let set: jsonwebtoken::jwk::JwkSet = serde_json::from_slice(&body)
            .map_err(|e| Error::Validation(format!("JWKS unreadable: {e}")))?;
        *self.jwks.write().await = Some((set.clone(), std::time::Instant::now()));
        Ok(set)
    }

    pub async fn verify_jwt(&self, token: &str) -> Result<Option<(String, String)>> {
        use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
        let Ok(header) = decode_header(token) else {
            return Ok(None);
        };
        let Some(kid) = header.kid.clone() else {
            return Ok(None);
        };
        let mut set = self.jwks(false).await?;
        if set.find(&kid).is_none() {
            set = self.jwks(true).await?;
        }
        let Some(jwk) = set.find(&kid) else {
            return Ok(None);
        };
        let Ok(key) = DecodingKey::from_jwk(jwk) else {
            return Ok(None);
        };
        // The algorithm comes from the key, never from the token alone.
        let alg = format!("{:?}", header.alg);
        let pinned = match jwk.common.key_algorithm {
            Some(ka) => format!("{ka:?}") == alg,
            None => [
                "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA",
            ]
            .contains(&alg.as_str()),
        };
        if !pinned {
            return Ok(None);
        }
        let mut v = Validation::new(header.alg);
        v.set_issuer(&[self.cfg.issuer_url.as_str()]);
        v.set_audience(&[self.cfg.client_id.as_str(), self.resource.as_str()]);
        #[derive(serde::Deserialize)]
        struct Claims {
            iss: String,
            sub: String,
        }
        match decode::<Claims>(token, &key, &v) {
            Ok(d) => Ok(Some((d.claims.iss, d.claims.sub))),
            Err(e) => {
                tracing::debug!(error = %e, "JWT bearer rejected");
                Ok(None)
            }
        }
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
            RedirectUrl::new(self.redirect_url.clone())
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
                // Set by the caller; see `web::auth_routes::login_page`.
                next: None,
            },
        ))
    }

    /// The crate verifies signature, issuer, audience and expiry; nonce and allowlist are checked here.
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
            RedirectUrl::new(self.redirect_url.clone())
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

        // Nextcloud sends `email` and `groups` only from userinfo, not in the ID token.
        // Best-effort: the signed ID token already vouches for the subject.
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

        if !is_allowed(&self.cfg, &subject, &groups) {
            tracing::warn!(%subject, ?groups, "sign-in denied: subject not on the allowlist");
            return Err(Error::Forbidden);
        }
        tracing::info!(%subject, "oidc sign-in");
        Ok(Identity {
            subject,
            email,
            session: None,
        })
    }
}

#[cfg(test)]
pub(crate) async fn fake_client(base: &str) -> (wiremock::MockServer, OidcClient) {
    use wiremock::matchers::path;
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    let issuer = server.uri();
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(&server)
        .await;
    Mock::given(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"keys": []})))
        .mount(&server)
        .await;
    let cfg = OidcConfig {
        issuer_url: issuer,
        client_id: "engram".into(),
        client_secret: None,
        scopes: vec!["openid".into()],
        open_registration: true,
        allowed_subs: vec![],
        allowed_groups: vec![],
    };
    let client = OidcClient::discover(&cfg, base)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    (server, client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OidcConfig;

    fn cfg(subs: &[&str], _emails: &[&str]) -> OidcConfig {
        cfg_with_groups(subs, &[], &[])
    }

    fn cfg_with_groups(subs: &[&str], _emails: &[&str], groups: &[&str]) -> OidcConfig {
        OidcConfig {
            issuer_url: "https://idp.example".into(),
            client_id: "engram".into(),
            client_secret: Some("s".into()),
            scopes: vec!["openid".into()],
            open_registration: false,
            allowed_subs: subs.iter().map(|s| s.to_string()).collect(),
            allowed_groups: groups.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_oidc_callback_is_derived_from_public_url() {
        assert_eq!(
            callback_url("https://engram.example"),
            "https://engram.example/auth/callback"
        );
        assert_eq!(
            callback_url("https://engram.example/"),
            "https://engram.example/auth/callback"
        );
    }

    #[test]
    fn an_empty_allowlist_admits_nobody_on_its_own() {
        assert!(!is_allowed(&cfg(&[], &[]), "sub-1", &[]));
    }

    #[test]
    fn open_registration_is_what_opens_an_empty_allowlist() {
        let mut c = cfg(&[], &[]);
        c.open_registration = true;
        assert!(is_allowed(&c, "sub-1", &[]));
    }

    #[test]
    fn a_populated_list_outranks_open_registration() {
        // The lists already state a narrower intent; the flag must not widen it.
        let mut c = cfg(&["sub-1"], &[]);
        c.open_registration = true;
        assert!(is_allowed(&c, "sub-1", &[]));
        assert!(!is_allowed(&c, "sub-2", &[]));
    }

    #[test]
    fn one_populated_list_closes_the_door_on_everyone_else() {
        // Naming one subject must not leave the other lists reading as "anyone".
        let c = cfg(&["sub-1"], &[]);
        assert!(!is_allowed(&c, "sub-2", &[]));
        assert!(!is_allowed(&c, "sub-2", &["engram-users".to_string()]));
    }

    #[test]
    fn a_listed_subject_is_allowed() {
        assert!(is_allowed(&cfg(&["sub-1"], &[]), "sub-1", &[]));
        assert!(!is_allowed(&cfg(&["sub-1"], &[]), "sub-2", &[]));
    }

    #[test]
    fn an_email_alone_admits_nobody() {
        assert!(!is_allowed(&cfg(&["sub-1"], &[]), "sub-9", &[]));
    }

    #[test]
    fn a_listed_group_is_allowed() {
        let c = cfg_with_groups(&[], &[], &["engram-users"]);
        assert!(is_allowed(
            &c,
            "sub-9",
            &["engram-users".to_string(), "other-group".to_string()]
        ));
        assert!(!is_allowed(&c, "sub-9", &["other-group".to_string()]));
        assert!(!is_allowed(&c, "sub-9", &[]));
    }

    #[test]
    fn pending_auth_is_single_use() {
        let store = PendingStore::new();
        let p = PendingAuth {
            csrf: "state-1".into(),
            nonce: "n".into(),
            pkce_verifier: "v".into(),
            created_at: crate::store::now(),
            next: Some("/ui/ops".into()),
        };
        store.put(p.clone());
        let back = store.take("state-1").expect("the attempt must come back");
        assert_eq!(
            back.next.as_deref(),
            Some("/ui/ops"),
            "the destination has to survive the round trip"
        );
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
            next: None,
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
