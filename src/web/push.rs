//! The push registration, over the API: what an app needs before it
//! registers with a distributor, and where it puts what the distributor
//! handed back.
//!
//! Under `/api/v1/push`. A person still types an endpoint into Settings by
//! hand; these are the same row arrived at programmatically, which is why
//! the Settings page says which device registered — the two ways in are
//! both visible there.

use crate::error::{Error, Result};
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, put};
use axum::{Json, Router};

/// The instance's VAPID public key, which a connector hands to its
/// distributor as the `applicationServerKey` — before it has an endpoint to
/// register, which is why this needs nothing but a credential.
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

/// Register, or re-register: a connector's endpoint changes, and the row is
/// replaced rather than added to. The keys are decoded here so a bad one is
/// refused at the door with its field named, and the endpoint is vetted the
/// way the Settings field is — a push goes out from the server's network
/// position, and an address on the server's own machine is not a push
/// service.
async fn register(
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
    notify["unifiedpush"] = serde_json::json!({
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
    // A channel just configured is what makes the unit worth arming — the
    // same re-arm the Settings form makes.
    tenant.core.store.rearm_remind().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Unregister. Answers the same whether or not a row stood: the state the
/// caller asked for is the state it gets.
async fn unregister(tenant: Tenant) -> Result<StatusCode> {
    let control = &tenant.core.store.control;
    let mut notify = control.notify(&tenant.user.subject).await?;
    if let Some(map) = notify.as_object_mut()
        && map.remove("unifiedpush").is_some()
    {
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
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let secret = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        serde_json::json!({
            "endpoint": endpoint,
            "p256dh": b64.encode(secret.public_key().to_encoded_point(false).as_bytes()),
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
        assert_eq!(
            notify["unifiedpush"]["endpoint"],
            "https://push.example/up/1"
        );
        assert_eq!(notify["unifiedpush"]["p256dh"], reg["p256dh"]);
        assert_eq!(notify["unifiedpush"]["auth"], reg["auth"]);
        assert_eq!(
            notify["unifiedpush"]["device"],
            "engram-android/0.1 (Pixel 8)"
        );
        assert!(notify["unifiedpush"]["registered_at"].is_i64());

        // Registering again replaces: the connector's endpoint moved.
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
        assert_eq!(
            notify["unifiedpush"]["endpoint"],
            "https://push.example/up/2"
        );
        assert_eq!(notify["unifiedpush"]["p256dh"], again["p256dh"]);

        // And the row reads back as a keyed target.
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
