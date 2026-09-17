//! Web Push as the current UnifiedPush specification has it: RFC 8291
//! `aes128gcm` encryption of a payload only the phone can read, RFC 8292
//! VAPID so the push service knows which deployment is speaking, and the
//! versioned JSON the phone decodes into a notification.
//!
//! What this module does *not* do is send. It builds the request; the
//! remind unit executes it on the client whose redirect policy
//! `remind::http_client` argues for.

use crate::error::{Error, Result};
use axum::http;
use base64::Engine;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The receiver's half of a registration, as the distributor handed it to
/// the app and the app handed it to `PUT /api/v1/push/unifiedpush`: the
/// P-256 public key and the 16-byte auth secret, both base64url.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPushKeys {
    pub p256dh: String,
    pub auth: String,
}

/// The receiver's keys decoded, or why they cannot be. Called at
/// registration so a bad key is refused at the door, and again at push,
/// where a stored key can no longer be bad.
pub fn parse_keys(p256dh: &str, auth: &str) -> Result<(p256::PublicKey, web_push_native::Auth)> {
    let point = B64
        .decode(p256dh)
        .map_err(|e| Error::Validation(format!("p256dh: {e}")))?;
    let public = p256::PublicKey::from_sec1_bytes(&point)
        .map_err(|_| Error::Validation("p256dh: not a P-256 point".into()))?;
    let secret = B64
        .decode(auth)
        .map_err(|e| Error::Validation(format!("auth: {e}")))?;
    let secret: [u8; 16] = secret.try_into().map_err(|v: Vec<u8>| {
        Error::Validation(format!("auth: {} bytes, and the secret is 16", v.len()))
    })?;
    Ok((public, web_push_native::Auth::from(secret)))
}

/// The instance's signing identity, RFC 8292.
#[derive(Clone)]
pub struct Vapid {
    signing: p256::ecdsa::SigningKey,
    /// The public key as the push service is shown it: base64url of the
    /// uncompressed point, the `k=` half of the header.
    pub public: String,
}

impl Vapid {
    pub fn from_keys(keys: &crate::store::control::VapidKeys) -> Result<Vapid> {
        let scalar = B64
            .decode(&keys.private)
            .map_err(|e| Error::Internal(format!("the instance's VAPID key: {e}")))?;
        let signing = p256::ecdsa::SigningKey::from_slice(&scalar)
            .map_err(|e| Error::Internal(format!("the instance's VAPID key: {e}")))?;
        Ok(Vapid {
            signing,
            public: keys.public.clone(),
        })
    }

    /// The `Authorization` header for one push: a JWT over the endpoint's
    /// origin, expiring at `exp`, signed ES256. `sub` is the contact RFC 8292
    /// says a sender SHOULD name; a deployment with no address for its user
    /// leaves it out rather than inventing one.
    ///
    /// The framing is two base64url JSON blobs and a signature, which is the
    /// whole of what a JWT library would add here — see `Cargo.toml` on why
    /// none was taken. The signature is `r || s`, 64 bytes, as JWS wants it,
    /// which is what `Signature::to_bytes` yields; the DER form is the one
    /// that would be wrong.
    pub fn authorization(&self, endpoint: &url::Url, sub: Option<&str>, exp: i64) -> String {
        use p256::ecdsa::signature::Signer;
        let mut claims = serde_json::json!({
            "aud": endpoint.origin().ascii_serialization(),
            "exp": exp,
        });
        if let Some(sub) = sub {
            claims["sub"] = sub.into();
        }
        let signing_input = format!(
            "{}.{}",
            B64.encode(r#"{"typ":"JWT","alg":"ES256"}"#),
            B64.encode(claims.to_string())
        );
        let sig: p256::ecdsa::Signature = self.signing.sign(signing_input.as_bytes());
        format!(
            "vapid t={signing_input}.{}, k={}",
            B64.encode(sig.to_bytes()),
            self.public
        )
    }
}

/// The largest message a push service is required to carry (RFC 8030 §7.2),
/// and so the one record this sends.
pub const MAX_RECORD: usize = 4096;
/// What fits in that record once the header (16 salt, 4 rs, 1 idlen, 65
/// key) and the delimiter and tag (1 + 16) have taken theirs.
pub const MAX_PLAINTEXT: usize = MAX_RECORD - (16 + 4 + 1 + 65) - (1 + 16);

/// How long the push service holds a message for a phone that is off.
/// A day: a reminder a day late is still a row on the band.
const TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3_600);

/// How long the VAPID token is good for, which is not the same question as
/// how long the message is held, though one constant used to answer both.
///
/// RFC 8292 §2 caps `exp` at 24 hours from the request, and the services that
/// matter reject *at* the boundary. Reusing `TTL` put every token exactly on
/// it, so a server clock a few seconds ahead of the push service turned every
/// push into a 401. Half the cap leaves no cliff to fall off.
const VAPID_EXP: std::time::Duration = std::time::Duration::from_secs(12 * 3_600);

/// The push, built and not sent. Refuses a body that would need a second
/// record rather than splitting it: a phone reads one record, and a truncated
/// JSON payload is a notification that fails to parse.
pub fn request(
    endpoint: &str,
    keys: &WebPushKeys,
    vapid: &Vapid,
    sub: Option<&str>,
    body: &[u8],
) -> Result<http::Request<Vec<u8>>> {
    if body.len() > MAX_PLAINTEXT {
        return Err(Error::Validation(format!(
            "a push payload of {} bytes is past the {MAX_PLAINTEXT} one record holds",
            body.len()
        )));
    }
    let url = url::Url::parse(endpoint).map_err(|e| Error::Validation(format!("endpoint: {e}")))?;
    let uri: http::Uri = endpoint
        .parse()
        .map_err(|e| Error::Validation(format!("endpoint: {e}")))?;
    let (public, auth) = parse_keys(&keys.p256dh, &keys.auth)?;
    let exp = crate::store::now() + VAPID_EXP.as_secs() as i64;
    let mut req = web_push_native::WebPushBuilder::new(uri, public, auth)
        .with_valid_duration(TTL)
        .build(body.to_vec())
        .map_err(|e| Error::Internal(format!("web push: {e}")))?;
    let header = vapid.authorization(&url, sub, exp);
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&header)
            .map_err(|e| Error::Internal(format!("vapid header: {e}")))?,
    );
    Ok(req)
}

/// One reminder as the payload names it: enough for the phone to draw a row
/// and call `done` or `snooze` on it, and nothing a notification does not show.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PayloadMoment {
    pub id: String,
    pub title: String,
    pub at: i64,
}

/// What the phone decodes. `v` is the version the phone checks first; a
/// version it does not know still rings, as a plain "something is due".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Kind {
    /// The ladder's message: what this wake owes. `more` is what the body
    /// would have counted past `BODY_LINES`; the band has them all.
    Due {
        at: i64,
        moments: Vec<PayloadMoment>,
        more: usize,
    },
    /// Not on the ladder: a confirmation, or the Settings test button.
    Notice {
        at: i64,
        title: String,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Payload {
    pub v: u8,
    #[serde(flatten)]
    pub kind: Kind,
}

pub const PAYLOAD_VERSION: u8 = 1;

impl Payload {
    pub fn new(kind: Kind) -> Payload {
        Payload {
            v: PAYLOAD_VERSION,
            kind,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("strings and integers always serialise")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        B64.decode(s).unwrap()
    }

    /// RFC 8291 §5, the published example: the receiver's key and auth
    /// secret, and the ciphertext the sender produced from a known ephemeral
    /// key and salt. Decrypting is the whole of RFC 8291 run backwards — the
    /// same ECDH, the same HKDF info strings, the same record layer — and an
    /// AEAD tag that verifies is not a coincidence.
    #[test]
    fn the_rfc_8291_example_decrypts_to_its_plaintext() {
        let secret =
            p256::SecretKey::from_slice(&b64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"))
                .unwrap();
        let auth: [u8; 16] = b64("BTBZMqHH6r4Tts7J_aSIgg").try_into().unwrap();
        let auth = web_push_native::Auth::from(auth);
        let ciphertext = b64(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        );
        // The vector's own ephemeral public key sits in the header as keyid.
        assert_eq!(
            &ciphertext[21..86],
            &b64(
                "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
            )[..]
        );
        let plain = web_push_native::decrypt(ciphertext, &secret, &auth).unwrap();
        assert_eq!(plain, b"When I grow up, I want to be a watermelon");
    }

    fn a_receiver() -> (p256::SecretKey, web_push_native::Auth, WebPushKeys) {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let secret = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let auth = web_push_native::Auth::from([7u8; 16]);
        let keys = WebPushKeys {
            p256dh: B64.encode(secret.public_key().to_encoded_point(false).as_bytes()),
            auth: B64.encode(auth),
        };
        (secret, auth, keys)
    }

    async fn a_vapid() -> Vapid {
        let c = crate::store::control::Control::memory().await.unwrap();
        Vapid::from_keys(&c.vapid().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn what_request_encrypts_the_receiver_decrypts_under_the_headers_the_rfcs_name() {
        let (secret, auth, keys) = a_receiver();
        let vapid = a_vapid().await;
        let req = request(
            "https://push.example/abc",
            &keys,
            &vapid,
            Some("mailto:x@example"),
            b"hello",
        )
        .unwrap();
        assert_eq!(req.method(), http::Method::POST);
        assert_eq!(req.headers()["content-encoding"], "aes128gcm");
        assert_eq!(req.headers()["ttl"], "86400");
        let authz = req.headers()["authorization"].to_str().unwrap().to_string();
        assert!(authz.starts_with("vapid t="), "{authz}");
        assert!(authz.ends_with(&format!(", k={}", vapid.public)), "{authz}");
        let body = req.into_body();
        let rs = u32::from_be_bytes(body[16..20].try_into().unwrap()) as usize;
        assert_eq!(
            rs,
            5 + 17,
            "one record, sized to the plaintext plus delimiter and tag"
        );
        assert_eq!(body[20], 65, "the ephemeral key is the keyid");
        assert!(
            !body.windows(5).any(|w| w == b"hello"),
            "nothing in the clear"
        );
        assert_eq!(
            web_push_native::decrypt(body, &secret, &auth).unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn the_vapid_header_is_an_es256_jwt_over_the_endpoint_origin() {
        use p256::ecdsa::signature::Verifier;
        let vapid = a_vapid().await;
        let url = url::Url::parse("https://push.example:8443/x/y?z").unwrap();
        let header = vapid.authorization(&url, Some("mailto:x@example"), 1_800_000_000);
        let (t, k) = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        let [h, c, s] = t.split('.').collect::<Vec<_>>()[..] else {
            panic!("three parts: {t}")
        };
        assert_eq!(b64(h), br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims: serde_json::Value = serde_json::from_slice(&b64(c)).unwrap();
        assert_eq!(claims["aud"], "https://push.example:8443");
        assert_eq!(claims["exp"], 1_800_000_000);
        assert_eq!(claims["sub"], "mailto:x@example");
        let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&b64(k)).unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&b64(s)).unwrap();
        key.verify(format!("{h}.{c}").as_bytes(), &sig).unwrap();

        let header = vapid.authorization(&url, None, 1);
        let c = header.split('.').nth(1).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&b64(c)).unwrap();
        assert!(
            claims.get("sub").is_none(),
            "no contact, no claim: {claims}"
        );
    }

    /// RFC 8292 §2 caps `exp` at 24 hours from the request, and FCM and
    /// Mozilla reject at the boundary. `exp` was `now + TTL`, which is exactly
    /// 24 hours: a server clock seconds ahead of the push service turned every
    /// push into a 401.
    #[tokio::test]
    async fn the_vapid_token_expires_well_inside_the_cap_that_rfc_8292_sets() {
        let (_, _, keys) = a_receiver();
        let vapid = a_vapid().await;
        let before = crate::store::now();
        let req = request("https://push.example/abc", &keys, &vapid, None, b"x").unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&b64(req.headers()["authorization"]
                .to_str()
                .unwrap()
                .strip_prefix("vapid t=")
                .unwrap()
                .split('.')
                .nth(1)
                .unwrap()))
            .unwrap();
        let ahead = claims["exp"].as_i64().unwrap() - before;
        assert!(ahead > 0, "in the future: {ahead}s");
        assert!(
            ahead <= 24 * 3_600 - 3_600,
            "an hour of clock skew must not reach the cap: {ahead}s"
        );
        assert_eq!(
            req.headers()["ttl"],
            "86400",
            "retention is its own question"
        );
    }

    #[tokio::test]
    async fn a_payload_past_one_record_is_refused_not_truncated() {
        let (_, _, keys) = a_receiver();
        let vapid = a_vapid().await;
        let full = vec![b'x'; MAX_PLAINTEXT];
        let req = request("https://push.example/abc", &keys, &vapid, None, &full).unwrap();
        assert_eq!(req.into_body().len(), MAX_RECORD);
        let over = vec![b'x'; MAX_PLAINTEXT + 1];
        let err = request("https://push.example/abc", &keys, &vapid, None, &over).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err}");
    }

    #[test]
    fn a_bad_key_is_refused_with_its_field_named() {
        let (_, _, keys) = a_receiver();
        assert!(
            parse_keys("not base64!", &keys.auth)
                .unwrap_err()
                .to_string()
                .contains("p256dh")
        );
        assert!(
            parse_keys(&keys.auth, &keys.auth)
                .unwrap_err()
                .to_string()
                .contains("p256dh")
        );
        assert!(
            parse_keys(&keys.p256dh, "AAAA")
                .unwrap_err()
                .to_string()
                .contains("auth")
        );
        assert!(parse_keys(&keys.p256dh, &keys.auth).is_ok());
    }

    #[test]
    fn the_payload_is_versioned_json_that_names_its_kind() {
        let due = Payload::new(Kind::Due {
            at: 10,
            moments: vec![PayloadMoment {
                id: "m1".into(),
                title: "Send the invoice".into(),
                at: 20,
            }],
            more: 2,
        });
        let json: serde_json::Value = serde_json::from_slice(&due.to_bytes()).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["kind"], "due");
        assert_eq!(json["at"], 10);
        assert_eq!(json["moments"][0]["id"], "m1");
        assert_eq!(json["more"], 2);
        let back: Payload = serde_json::from_value(json).unwrap();
        assert_eq!(back, due);

        let notice = Payload::new(Kind::Notice {
            at: 1,
            title: "engram".into(),
            body: "A test from Settings.".into(),
        });
        let json: serde_json::Value = serde_json::from_slice(&notice.to_bytes()).unwrap();
        assert_eq!(json["kind"], "notice");
        assert_eq!(json["title"], "engram");
    }
}
