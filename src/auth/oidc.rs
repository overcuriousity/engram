use super::Identity;
use crate::config::OidcConfig;
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// A login attempt older than this is abandoned. Long enough for a slow
/// identity provider, short enough that a leaked state value is useless.
pub const PENDING_TTL_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub struct PendingAuth {
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub created_at: i64,
    /// The page that asked for the login. The provider hands back only
    /// `state`, so the destination has to wait here for the callback.
    pub next: Option<String>,
}

/// In-memory, single-use, expiring store of in-flight login attempts. Not
/// persisted: a restart mid-login just means logging in again.
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

    /// Removes and returns the attempt. Single use: a replayed `state` value
    /// finds nothing.
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

/// Who may sign in. An empty allowlist denies everyone by design: without it,
/// every account the identity provider knows about could read the knowledge
/// base.
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

/// The one claim this reads beyond the OIDC standard set: which groups the
/// provider says the subject belongs to. A provider that never sends it — the
/// common case, since Nextcloud's OIDC provider app only includes `groups`
/// when an admin turns on group provisioning for the client — simply
/// deserializes to an empty list rather than failing the claim parse.
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

/// `openidconnect::core::CoreClient` with its additional-claims parameter
/// swapped from `EmptyAdditionalClaims` to [`GroupClaims`], so the `groups`
/// claim decodes rather than being discarded. Otherwise identical to `Core*`
/// — same algorithms, same error and token types.
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
    /// Discovery result, fetched once at startup.
    ///
    /// The constructed client is deliberately NOT stored: openidconnect
    /// encodes which endpoints are configured in a seventeen-parameter
    /// typestate, and naming that type in a struct field pins us to one patch
    /// release. Rebuilding per call costs nothing (no I/O) and lets inference
    /// carry the type.
    metadata: openidconnect::core::CoreProviderMetadata,
    /// The credential-bearing client: no redirects. The permissive one built
    /// for discovery is deliberately not kept — see `discover`.
    http: openidconnect::reqwest::Client,
    cfg: OidcConfig,
}

/// The host this deployment is publicly reachable under, taken from the
/// redirect URL — the one place the configuration already states its own
/// public address. The MCP door's Host guard names it explicitly.
pub(crate) fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
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

        // Two clients, because the two kinds of request carry different things.
        //
        // Discovery follows redirects: Nextcloud's documented nginx recipe 301s
        // the bare .well-known path to /index.php/.well-known/..., while
        // issuer_url stays the bare domain, since that is what the discovery
        // document declares as `issuer` and what ID tokens carry as `iss`. The
        // request is an unauthenticated GET, so a hop costs nothing but the
        // hop; the count is bounded anyway.
        let discovery_http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| Error::Validation(e.to_string()))?;

        // Everything after discovery refuses to follow one. The token exchange
        // POSTs `client_secret` and the authorization code, and on a 307/308
        // reqwest replays method and body at the redirect target — so a
        // provider that could be made to answer with a redirect would be
        // handed our credentials for an arbitrary host. userinfo is on this
        // client too: it bears the access token.
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
        })
    }

    /// The public host of this deployment, parsed from the redirect URL.
    pub fn public_host(&self) -> Option<String> {
        host_of(&self.cfg.redirect_url)
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
                // Filled in by the caller, which is the half that knows where
                // the browser was going. See `web::auth_routes::login_page`.
                next: None,
            },
        ))
    }

    /// Exchange the authorization code and validate the ID token. The
    /// openidconnect crate verifies the signature, issuer, audience and
    /// expiry; the nonce and the allowlist are checked here.
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

        // Nextcloud's OIDC provider app does not put `email` (or `groups`) in
        // the ID token even when the scope is granted — they only appear from
        // the userinfo endpoint. Without this, an allowlist keyed on either
        // always sees nothing and every sign-in is refused, however correctly
        // the config names the account.
        //
        // Best-effort: the subject from the ID token is already verified by
        // its signature, so an endpoint that is absent or unreachable must
        // not turn into a failed sign-in for someone the ID token itself
        // vouches for.
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
        Ok(Identity {
            subject,
            email,
            // The session is created from this, after it returns.
            session: None,
        })
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
    fn the_public_host_is_parsed_out_of_the_redirect_url() {
        assert_eq!(
            host_of("https://engram.example/auth/callback").as_deref(),
            Some("engram.example")
        );
        // An explicit port is the proxy's business, not the Host guard's: the
        // entry matches with or without one.
        assert_eq!(
            host_of("https://engram.example:8443/auth/callback").as_deref(),
            Some("engram.example")
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn an_empty_allowlist_denies_everyone() {
        // Defaulting open would hand the knowledge base to every account in
        // the identity provider.
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
