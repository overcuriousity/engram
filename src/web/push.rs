//! The push registration over the API, under `/api/v1/push`; Settings shows which device registered.

use crate::error::{Error, Result};
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, put};
use axum::{Json, Router};

/// Handed to the distributor as `applicationServerKey` before there is an endpoint, so it needs only a credential.
async fn vapid(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    let keys = tenant.core.store.control.vapid().await?;
    Ok(Json(serde_json::json!({ "public_key": keys.public })))
}

#[derive(serde::Deserialize)]
pub struct Registration {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// What a registration is filed under: the token that paired the device, so two phones keep two.
fn device_key(id: &crate::auth::Identity) -> String {
    id.token.clone().unwrap_or_else(|| "browser".into())
}

/// Replaces this device's own; keys are decoded so a bad one is named, and the endpoint is vetted as in Settings.
async fn register(
    id: crate::auth::Identity,
    tenant: Tenant,
    headers: HeaderMap,
    Json(reg): Json<Registration>,
) -> Result<StatusCode> {
    let endpoint = reg.endpoint.trim();
    if endpoint.is_empty() {
        return Err(Error::Validation("endpoint: empty".into()));
    }
    crate::web::settings::push_url("endpoint", endpoint)?;
    crate::jobs::webpush::parse_keys(reg.p256dh.trim(), reg.auth.trim())?;
    let control = &tenant.core.store.control;
    let mut notify = control.notify(&tenant.user.subject).await?;
    // A registration from before devices were kept apart sits where the typed endpoint does.
    if notify["unifiedpush"]["endpoint"] == endpoint
        && let Some(map) = notify.as_object_mut()
    {
        map.remove("unifiedpush");
    }
    notify["devices"][device_key(&id)] = serde_json::json!({
        "endpoint": endpoint,
        "p256dh": reg.p256dh.trim(),
        "auth": reg.auth.trim(),
        "device": headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|ua| !ua.is_empty()),
        "registered_at": tenant.core.clock.now(),
    });
    control.set_notify(&tenant.user.subject, &notify).await?;
    // A channel just configured makes the unit worth arming, as the Settings form does.
    tenant.core.store.rearm_remind().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Answers the same whether or not a row stood. Only this device's own goes; one from before
/// devices were kept apart cannot be told from another phone's, so it goes only where this
/// device has no row of its own.
async fn unregister(id: crate::auth::Identity, tenant: Tenant) -> Result<StatusCode> {
    let control = &tenant.core.store.control;
    let mut notify = control.notify(&tenant.user.subject).await?;
    let own = notify["devices"]
        .as_object_mut()
        .is_some_and(|d| d.remove(&device_key(&id)).is_some());
    let earlier = !own
        && notify["unifiedpush"]["p256dh"].is_string()
        && notify
            .as_object_mut()
            .is_some_and(|m| m.remove("unifiedpush").is_some());
    if own || earlier {
        control.set_notify(&tenant.user.subject, &notify).await?;
        tenant.core.store.rearm_remind().await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/push/vapid", get(vapid))
        .route("/push/unifiedpush", put(register).delete(unregister))
}

#[cfg(test)]
mod tests {
    use crate::web::test_support::{app_with_token, json_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn req(
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Request<Body> {
        let mut b = Request::builder().uri(uri).method(method);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b = b.header("user-agent", "engram-android/0.1 (Pixel 8)");
        match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        }
    }

    fn a_registration(endpoint: &str) -> serde_json::Value {
        use base64::Engine;
        use p256::elliptic_curve::{Generate, sec1::ToSec1Point};
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let secret = p256::SecretKey::generate();
        serde_json::json!({
            "endpoint": endpoint,
            "p256dh": b64.encode(secret.public_key().to_sec1_point(false).as_bytes()),
            "auth": b64.encode([1u8; 16]),
        })
    }

    #[tokio::test]
    async fn the_vapid_key_is_the_instances_and_needs_a_credential() {
        let core = crate::core::test_support::test_core().await;
        let expected = core.store.control.vapid().await.unwrap().public;
        let (app, token) = app_with_token(core).await;
        let res = app
            .clone()
            .oneshot(req("GET", "/api/v1/push/vapid", None, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app
            .oneshot(req("GET", "/api/v1/push/vapid", Some(&token), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(json_of(res).await["public_key"], expected);
    }

    fn endpoints(notify: &serde_json::Value) -> Vec<String> {
        let mut found: Vec<String> = crate::jobs::remind::notify_targets(notify)
            .into_iter()
            .filter_map(|t| match t {
                crate::jobs::remind::Target::UnifiedPush { endpoint, .. } => Some(endpoint),
                _ => None,
            })
            .collect();
        found.sort();
        found
    }

    #[tokio::test]
    async fn two_phones_each_keep_their_own_registration() {
        let core = crate::core::test_support::test_core().await;
        let (app, first) = app_with_token(core.clone()).await;
        let (second_row, second) =
            crate::auth::tokens::mint(&core.store.control, "tablet", "user-1", None)
                .await
                .unwrap();
        for (token, endpoint) in [
            (&first, "https://push.example/up/phone"),
            (&second, "https://push.example/up/tablet"),
        ] {
            let res = app
                .clone()
                .oneshot(req(
                    "PUT",
                    "/api/v1/push/unifiedpush",
                    Some(token),
                    Some(a_registration(endpoint)),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NO_CONTENT);
        }
        let notify = || async { core.store.control.notify("user-1").await.unwrap() };
        assert_eq!(
            endpoints(&notify().await),
            [
                "https://push.example/up/phone",
                "https://push.example/up/tablet"
            ],
            "the second phone took the first one's place"
        );

        // Unpairing one phone takes its own registration and no other.
        let res = app
            .clone()
            .oneshot(req(
                "DELETE",
                "/api/v1/push/unifiedpush",
                Some(&first),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            endpoints(&notify().await),
            ["https://push.example/up/tablet"]
        );

        // A revoked token's phone is no longer pushed to.
        crate::auth::tokens::revoke(&core.store.control, &second_row.id, "user-1")
            .await
            .unwrap();
        assert!(endpoints(&notify().await).is_empty());
    }

    #[tokio::test]
    async fn registering_stores_the_keys_and_names_the_device() {
        let core = crate::core::test_support::test_core().await;
        let (app, token) = app_with_token(core.clone()).await;
        let reg = a_registration("https://push.example/up/1");
        let res = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(reg.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        // Filed under the token that registered it, one entry however often it registers.
        let own = |notify: &serde_json::Value| {
            let devices = notify["devices"].as_object().expect("a device registered");
            assert_eq!(devices.len(), 1);
            devices.values().next().cloned().unwrap()
        };
        let stored = own(&notify);
        assert_eq!(stored["endpoint"], "https://push.example/up/1");
        assert_eq!(stored["p256dh"], reg["p256dh"]);
        assert_eq!(stored["auth"], reg["auth"]);
        assert_eq!(stored["device"], "engram-android/0.1 (Pixel 8)");
        assert!(stored["registered_at"].is_i64());

        let again = a_registration("https://push.example/up/2");
        app.clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(again.clone()),
            ))
            .await
            .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        let stored = own(&notify);
        assert_eq!(stored["endpoint"], "https://push.example/up/2");
        assert_eq!(stored["p256dh"], again["p256dh"]);

        let targets = crate::jobs::remind::notify_targets(&notify);
        assert!(matches!(
            &targets[..],
            [crate::jobs::remind::Target::UnifiedPush { keys: Some(_), .. }]
        ));
    }

    #[tokio::test]
    async fn a_registration_leaves_gotify_alone() {
        let core = crate::core::test_support::test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"gotify": {"url": "https://g/message", "token": "t"}}),
            )
            .await
            .unwrap();
        let (app, token) = app_with_token(core.clone()).await;
        let reg = a_registration("https://push.example/up/1");
        app.clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(reg),
            ))
            .await
            .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert_eq!(notify["gotify"]["token"], "t");
        app.oneshot(req(
            "DELETE",
            "/api/v1/push/unifiedpush",
            Some(&token),
            None,
        ))
        .await
        .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert_eq!(notify["gotify"]["token"], "t");
        assert!(notify.get("unifiedpush").is_none());
    }

    #[tokio::test]
    async fn a_bad_key_or_an_inward_endpoint_is_refused_with_its_field_named() {
        let core = crate::core::test_support::test_core().await;
        let (app, token) = app_with_token(core.clone()).await;
        let mut bad_key = a_registration("https://push.example/up/1");
        bad_key["p256dh"] = "AAAA".into();
        let res = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(bad_key),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_of(res).await["error"]
                .as_str()
                .unwrap()
                .contains("p256dh")
        );

        let mut short_auth = a_registration("https://push.example/up/1");
        short_auth["auth"] = "AAAA".into();
        let res = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(short_auth),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_of(res).await["error"]
                .as_str()
                .unwrap()
                .contains("auth")
        );

        let inward = a_registration("http://127.0.0.1:9/up");
        let res = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(inward),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            json_of(res).await["error"]
                .as_str()
                .unwrap()
                .contains("endpoint")
        );

        assert!(
            core.store
                .control
                .notify(&core.store.subject)
                .await
                .unwrap()["unifiedpush"]
                .is_null(),
            "nothing refused was stored"
        );
    }

    #[tokio::test]
    async fn unregistering_removes_the_row_and_is_quiet_when_there_is_none() {
        let core = crate::core::test_support::test_core().await;
        let (app, token) = app_with_token(core.clone()).await;
        let res = app
            .clone()
            .oneshot(req(
                "DELETE",
                "/api/v1/push/unifiedpush",
                Some(&token),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let reg = a_registration("https://push.example/up/1");
        app.clone()
            .oneshot(req(
                "PUT",
                "/api/v1/push/unifiedpush",
                Some(&token),
                Some(reg),
            ))
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(req(
                "DELETE",
                "/api/v1/push/unifiedpush",
                Some(&token),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert!(notify.get("unifiedpush").is_none());
        assert!(crate::jobs::remind::notify_targets(&notify).is_empty());
    }
}
