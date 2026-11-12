use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use engram::store::control::Control;
use tower::ServiceExt;

const HOST: &str = "engram.example";
const ORIGIN: &str = "https://engram.example";
const SUBJECT: &str = "sub-render";

struct MemoryFactory(
    std::sync::Mutex<std::collections::HashMap<String, Arc<engram::vector::memory::MemoryVectors>>>,
);

#[async_trait::async_trait]
impl engram::tenants::VectorFactory for MemoryFactory {
    async fn open(
        &self,
        alias: &str,
        _dim: usize,
    ) -> engram::error::Result<Arc<dyn engram::vector::VectorStore>> {
        let mut made = self.0.lock().unwrap();
        Ok(made
            .entry(alias.to_string())
            .or_insert_with(|| Arc::new(engram::vector::memory::MemoryVectors::new()))
            .clone())
    }
}

struct Seeded {
    app: axum::Router,
    cookie: String,
    corpus: String,
    artifact: String,
    _dir: tempfile::TempDir,
}

const BODY: &str = "# Mount the volume\n\n\
| Step | Command |\n| --- | --- |\n| one | `mount /dev/sdb1 /mnt` |\n\n\
The rate is $e^{i\\pi} + 1 = 0$, see [the base](https://engram.example/ui/search).\n";

async fn seeded() -> Seeded {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = engram::config::Config::test_default();
    cfg.store.dir = dir.path().to_string_lossy().to_string();
    cfg.server.public_url = Some(ORIGIN.into());
    let dep = cfg.deployment().unwrap();
    let cfg = Arc::new(cfg);
    let control = Control::memory().await.unwrap();
    let tenants = Arc::new(engram::tenants::Tenants::new(
        cfg.clone(),
        control.clone(),
        Arc::new(MemoryFactory(Default::default())),
    ));
    let tenant = tenants.get_or_provision(SUBJECT, None).await.unwrap();
    let core = &tenant.core;
    let out = core.ingest(BODY, "web", Some("Volumes")).await.unwrap();
    for _ in 0..20 {
        if !matches!(engram::jobs::run_one(core).await, Ok(true)) {
            break;
        }
    }
    let artifact = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
        .id
        .clone();
    let tomorrow = engram::store::now() + 86_400;
    core.edit_reminder(&artifact, tomorrow, "UTC", None)
        .await
        .unwrap();
    engram::auth::tokens::mint(&control, "laptop", SUBJECT, None)
        .await
        .unwrap();

    let cookie = {
        use tower_sessions::SessionStore;
        use tower_sessions::cookie::time::{Duration, OffsetDateTime};
        use tower_sessions::session::{Id, Record};
        let store = tower_sessions_sqlx_store::SqliteStore::new(control.pool.clone());
        let mut record = Record {
            id: Id::default(),
            data: [(
                engram::auth::SUBJECT_KEY.to_string(),
                serde_json::json!(SUBJECT),
            )]
            .into_iter()
            .collect(),
            expiry_date: OffsetDateTime::now_utc() + Duration::days(1),
        };
        store.create(&mut record).await.unwrap();
        format!("{}={}", engram::auth::session_cookie_name(&dep), record.id)
    };

    let state = engram::web::state::AppState {
        tenants,
        config: cfg,
        auth: Arc::new(engram::web::state::AuthContext {
            mode: engram::config::AuthMode::Local,
            local: Some(engram::config::LocalConfig {
                username: SUBJECT.into(),
                password_hash: engram::auth::local::hash_password("pw").unwrap(),
            }),
            oidc: None,
            pending: engram::auth::oidc::PendingStore::new(),
            deployment: dep.clone(),
        }),
        ask_handoff: Default::default(),
    };
    let app = engram::web::app(state, &dep)
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))));
    Seeded {
        app,
        cookie,
        corpus: out.id,
        artifact,
        _dir: dir,
    }
}

async fn page(app: &axum::Router, uri: &str, cookie: Option<&str>) -> (StatusCode, String, String) {
    let mut r = Request::get(uri).header(header::HOST, HOST);
    if let Some(c) = cookie {
        r = r.header(header::COOKIE, c);
    }
    let res = app
        .clone()
        .oneshot(r.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let csp = res
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = axum::body::to_bytes(res.into_body(), 1 << 24)
        .await
        .unwrap();
    (status, csp, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn every_page_passes_the_csp_rules() {
    let s = seeded().await;
    let signed_in = [
        "/".to_string(),
        "/ui".to_string(),
        "/ui/search?q=mount+the+volume".to_string(),
        "/ui/settings".to_string(),
        "/ui/insights".to_string(),
        format!("/ui/corpora/{}", s.corpus),
        format!("/ui/artifacts/{}", s.artifact),
        "/ui/day/today".to_string(),
        "/ui/pair?redirect_uri=https%3A%2F%2Fabc.chromiumapp.org%2F&state=s1".to_string(),
        "/ui/app".to_string(),
        "/extension/install".to_string(),
        "/ui/no-such-page".to_string(),
    ];
    let mut failures = Vec::new();
    let mut checked = 0;
    for uri in &signed_in {
        let (mut status, mut csp, mut body) = page(&s.app, uri, Some(&s.cookie)).await;
        if status.is_redirection() {
            continue;
        }
        if uri == "/" && status == StatusCode::SEE_OTHER {
            (status, csp, body) = page(&s.app, "/ui", Some(&s.cookie)).await;
        }
        if !body.contains("<html") {
            failures.push(format!("{uri}: {status} is not a page"));
            continue;
        }
        checked += 1;
        assert_eq!(csp, engram::web::security::CSP, "{uri}");
        for f in oc_check::check_html_for(&body, ORIGIN) {
            let line = body.lines().nth(f.line - 1).unwrap_or("").trim();
            failures.push(format!(
                "{uri}:{}: {}: {} | {line}",
                f.line, f.rule, f.message
            ));
        }
    }
    let (_, _, login) = page(&s.app, "/auth/login", None).await;
    for f in oc_check::check_html_for(&login, ORIGIN) {
        failures.push(format!("/auth/login:{}: {}: {}", f.line, f.rule, f.message));
    }
    assert!(checked >= 10, "only {checked} pages rendered");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
