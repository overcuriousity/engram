#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Two tenants through `web::router`, since the boundary under test is the `Tenant` extractor.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use engram::store::control::Control;
use std::sync::Arc;
use tower::ServiceExt;

struct MemoryFactory {
    made: std::sync::Mutex<
        std::collections::HashMap<String, Arc<engram::vector::memory::MemoryVectors>>,
    >,
}

#[async_trait::async_trait]
impl engram::tenants::VectorFactory for MemoryFactory {
    async fn open(
        &self,
        alias: &str,
        _dim: usize,
    ) -> engram::error::Result<Arc<dyn engram::vector::VectorStore>> {
        let mut made = self.made.lock().unwrap();
        let v = made
            .entry(alias.to_string())
            .or_insert_with(|| Arc::new(engram::vector::memory::MemoryVectors::new()))
            .clone();
        Ok(v)
    }
}

struct Signed {
    subject: String,
    token: String,
    token_id: String,
}

async fn two_tenant_app() -> (axum::Router, Signed, Signed, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("scratch tenant dir");
    let mut cfg = engram::config::Config::test_default();
    cfg.store.dir = dir.path().to_string_lossy().to_string();
    let cfg = Arc::new(cfg);

    let control = Control::memory().await.unwrap();
    let tenants = Arc::new(engram::tenants::Tenants::new(
        cfg.clone(),
        control.clone(),
        Arc::new(MemoryFactory {
            made: std::sync::Mutex::new(std::collections::HashMap::new()),
        }),
    ));

    let mut signed = Vec::new();
    for subject in ["sub-a", "sub-b"] {
        tenants.get_or_provision(subject, None).await.unwrap();
        let (row, token) = engram::auth::tokens::mint(&control, "test", subject, None)
            .await
            .unwrap();
        signed.push(Signed {
            subject: subject.to_string(),
            token,
            token_id: row.id,
        });
    }
    let b = signed.pop().unwrap();
    let a = signed.pop().unwrap();

    let state = engram::web::state::AppState {
        tenants,
        config: cfg,
        auth: Arc::new(engram::web::state::AuthContext {
            mode: engram::config::AuthMode::Oidc,
            local: None,
            oidc: None,
            pending: engram::auth::oidc::PendingStore::new(),
            deployment: engram::web::security::Deployment::loopback(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                8080,
            ))),
        }),
        ask_handoff: Default::default(),
    };
    (engram::web::router(state), a, b, dir)
}

async fn send(app: &axum::Router, who: &Signed, req: Request<Body>) -> axum::response::Response {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", who.token).parse().unwrap(),
    );
    app.clone()
        .oneshot(Request::from_parts(parts, body))
        .await
        .unwrap()
}

async fn get(app: &axum::Router, who: &Signed, path: &str) -> axum::response::Response {
    send(
        app,
        who,
        Request::builder().uri(path).body(Body::empty()).unwrap(),
    )
    .await
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn capture(app: &axum::Router, who: &Signed, text: &str) -> String {
    let res = send(
        app,
        who,
        Request::builder()
            .method("POST")
            .uri("/api/v1/corpora")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "text": text, "source": "web" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CREATED, "capture was refused");
    json(res).await["id"].as_str().unwrap().to_string()
}

fn ids_in(v: &serde_json::Value) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    if k == "id"
                        && let Some(s) = val.as_str()
                    {
                        out.push(s.to_string());
                    }
                    walk(val, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

#[tokio::test]
async fn neither_tenant_can_see_the_others_corpus() {
    let (app, a, b, _dir) = two_tenant_app().await;

    let a_id = capture(&app, &a, "the same words in both bases").await;
    let b_id = capture(&app, &b, "the same words in both bases").await;
    assert_ne!(a_id, b_id, "one base served two captures");

    let a_list = ids_in(&json(get(&app, &a, "/api/v1/corpora").await).await);
    assert!(a_list.contains(&a_id), "a cannot see their own capture");
    assert!(!a_list.contains(&b_id), "a can see b's capture in the list");

    let b_list = ids_in(&json(get(&app, &b, "/api/v1/corpora").await).await);
    assert!(b_list.contains(&b_id));
    assert!(!b_list.contains(&a_id));
}

#[tokio::test]
async fn fetching_the_other_tenants_id_is_a_404_and_not_a_403() {
    let (app, a, b, _dir) = two_tenant_app().await;
    let b_id = capture(&app, &b, "something only b captured").await;

    let res = get(&app, &a, &format!("/api/v1/corpora/{b_id}")).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // The 404 above is about who asked, not about the id.
    assert_eq!(
        get(&app, &b, &format!("/api/v1/corpora/{b_id}"))
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_capture_by_one_tenant_queues_work_only_for_them() {
    let (app, a, b, _dir) = two_tenant_app().await;
    capture(&app, &a, "something to chew on").await;

    let a_status = json(get(&app, &a, "/api/v1/status").await).await;
    let b_status = json(get(&app, &b, "/api/v1/status").await).await;
    let queued = |v: &serde_json::Value| {
        v.get("jobs")
            .and_then(serde_json::Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|r| r.get(0).and_then(serde_json::Value::as_str) == Some("pending"))
                    .filter_map(|r| r.get(1).and_then(serde_json::Value::as_i64))
                    .sum::<i64>()
            })
            .unwrap_or(0)
    };
    assert!(
        queued(&a_status) > 0,
        "{} captured and nothing was queued: {a_status}",
        a.subject
    );
    assert_eq!(
        queued(&b_status),
        0,
        "{}'s capture showed up in {}'s queue: {b_status}",
        a.subject,
        b.subject
    );
}

#[tokio::test]
async fn a_tenant_that_was_evicted_comes_back_with_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = engram::config::Config::test_default();
    cfg.store.dir = dir.path().to_string_lossy().to_string();
    // Opening the second tenant evicts the first.
    cfg.store.max_open_tenants = 1;
    let cfg = Arc::new(cfg);

    let control = Control::memory().await.unwrap();
    let tenants = Arc::new(engram::tenants::Tenants::new(
        cfg,
        control,
        Arc::new(MemoryFactory {
            made: std::sync::Mutex::new(std::collections::HashMap::new()),
        }),
    ));

    let a = tenants.get_or_provision("sub-a", None).await.unwrap();
    let captured = a
        .core
        .ingest("something a wrote down", "web", None)
        .await
        .unwrap();
    drop(a);

    tenants.get_or_provision("sub-b", None).await.unwrap();
    assert_eq!(tenants.open_count(), 1, "the cap did not evict");

    let a_again = tenants.get_or_provision("sub-a", None).await.unwrap();
    assert!(
        a_again.core.store.get_corpus(&captured.id).await.is_ok(),
        "a reopened tenant lost what it had written"
    );
}

async fn text(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn settings_lists_this_tenants_tokens_and_not_the_others() {
    // `api_tokens` is instance-wide: an unfiltered listing exposes every tenant's tokens.
    let (app, a, b, _dir) = two_tenant_app().await;
    let page = text(get(&app, &a, "/ui/settings").await).await;
    assert!(
        page.contains(&a.token_id),
        "the settings page did not list the caller's own token"
    );
    assert!(
        !page.contains(&b.token_id),
        "the settings page listed another tenant's token"
    );
}

#[tokio::test]
async fn a_tenant_cannot_revoke_the_others_token() {
    // Revoking by bare id would kill another tenant's extension pairing.
    let (app, a, b, _dir) = two_tenant_app().await;
    let res = send(
        &app,
        &a,
        Request::builder()
            .method("POST")
            .uri(format!("/ui/ops/tokens/{}/revoke", b.token_id))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "another tenant's token id was accepted"
    );
    // A made-up id gets the same answer, or the route enumerates token ids.
    let invented = send(
        &app,
        &a,
        Request::builder()
            .method("POST")
            .uri("/ui/ops/tokens/01JZZZZZZZZZZZZZZZZZZZZZZZ/revoke")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(invented.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        get(&app, &b, "/api/v1/corpora").await.status(),
        StatusCode::OK,
        "the other tenant's token stopped working"
    );
}

#[tokio::test]
async fn a_tenant_can_revoke_its_own_token() {
    let (app, a, _b, _dir) = two_tenant_app().await;
    let res = send(
        &app,
        &a,
        Request::builder()
            .method("POST")
            .uri(format!("/ui/ops/tokens/{}/revoke", a.token_id))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        res.status().is_success(),
        "revoking one's own token answered {}",
        res.status()
    );
    assert_eq!(
        get(&app, &a, "/api/v1/corpora").await.status(),
        StatusCode::UNAUTHORIZED,
        "the revoked token still works"
    );
}
