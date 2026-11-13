//! One way to build the app under test, shared by every `web/` test module.

use crate::core::Core;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use tower::ServiceExt as _;

/// The real router over a one-tenant registry; pass `Some(cfg)` to test the login form.
pub async fn router(core: Core, local: Option<crate::config::LocalConfig>) -> axum::Router {
    let user = registered_user(&core).await;
    let cfg = std::sync::Arc::new(crate::config::Config::test_default());
    let tenants = std::sync::Arc::new(crate::tenants::Tenants::single(cfg.clone(), core, user));
    crate::web::router(crate::web::state::AppState {
        tenants,
        config: cfg,
        auth: std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Local,
            local,
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            deployment: crate::web::security::Deployment::loopback(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                8080,
            ))),
        }),
        ask_handoff: Default::default(),
    })
}

async fn registered_user(core: &Core) -> crate::store::control::User {
    let subject = crate::store::TEST_SUBJECT;
    core.store.control.provision(subject, None).await.ok();
    crate::store::control::User {
        subject: subject.into(),
        email: None,
        slug: crate::store::control::slug_for(subject),
        created_at: 0,
    }
}

/// For tests that need to hold the state rather than only the router.
pub async fn state_over(core: Core, mode: crate::config::AuthMode) -> crate::web::state::AppState {
    let cfg = std::sync::Arc::new(crate::config::Config::test_default());
    let user = registered_user(&core).await;
    crate::web::state::AppState {
        tenants: std::sync::Arc::new(crate::tenants::Tenants::single(cfg.clone(), core, user)),
        config: cfg,
        auth: std::sync::Arc::new(crate::web::state::AuthContext {
            mode,
            local: None,
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            deployment: crate::web::test_support::deployment_for(mode),
        }),
        ask_handoff: Default::default(),
    }
}

/// A router over `core` plus a browser session cookie for `user-1`.
pub async fn app_with_cookie(core: Core) -> (axum::Router, String) {
    let control = core.store.control.clone();
    let cfg = std::sync::Arc::new(crate::config::Config::test_default());
    let user = registered_user(&core).await;
    let state = crate::web::state::AppState {
        tenants: std::sync::Arc::new(crate::tenants::Tenants::single(cfg.clone(), core, user)),
        config: cfg,
        auth: std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Local,
            local: None,
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            deployment: crate::web::security::Deployment::loopback(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                8080,
            ))),
        }),
        ask_handoff: Default::default(),
    };
    let cookie = session_cookie(&control, &state.auth.deployment, "user-1").await;
    (crate::web::router(state.clone()), cookie)
}

pub async fn session_cookie(
    control: &crate::store::control::Control,
    dep: &crate::web::security::Deployment,
    subject: &str,
) -> String {
    use tower_sessions::SessionStore;
    use tower_sessions::cookie::time::{Duration, OffsetDateTime};
    use tower_sessions::session::{Id, Record};
    let store = tower_sessions_sqlx_store::SqliteStore::new(control.pool.clone());
    let mut record = Record {
        id: Id::default(),
        data: [(
            crate::auth::SUBJECT_KEY.to_string(),
            serde_json::json!(subject),
        )]
        .into_iter()
        .collect(),
        expiry_date: OffsetDateTime::now_utc() + Duration::days(1),
    };
    store.create(&mut record).await.unwrap();
    format!("{}={}", crate::auth::session_cookie_name(dep), record.id)
}

/// A router over `core` plus a bearer token for `user-1`.
pub async fn app_with_token(core: Core) -> (axum::Router, String) {
    let (_, token) = crate::auth::tokens::mint(&core.store.control, "test", "user-1", None)
        .await
        .unwrap();
    (router(core, None).await, token)
}

pub async fn body_of(res: Response) -> String {
    let b = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8_lossy(&b).to_string()
}

pub async fn json_of(res: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// `mime` of `None` omits the part's `Content-Type`, which is legal.
pub struct FilePart<'a> {
    pub field: &'a str,
    pub filename: &'a str,
    pub mime: Option<&'a str>,
    pub body: &'a [u8],
}

/// Text fields first, then file parts, with a bearer token.
pub fn multipart(
    uri: &str,
    token: &str,
    fields: &[(&str, &str)],
    files: &[FilePart<'_>],
) -> Request<Body> {
    const B: &str = "engramtestboundary";
    let mut buf: Vec<u8> = Vec::new();
    for (k, v) in fields {
        buf.extend_from_slice(
            format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    for f in files {
        let typed = match f.mime {
            Some(m) => format!("Content-Type: {m}\r\n"),
            None => String::new(),
        };
        buf.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n{typed}\r\n",
                f.field, f.filename
            )
            .as_bytes(),
        );
        buf.extend_from_slice(f.body);
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(format!("--{B}--\r\n").as_bytes());
    Request::builder()
        .uri(uri)
        .method("POST")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", format!("multipart/form-data; boundary={B}"))
        .body(Body::from(buf))
        .unwrap()
}

pub fn a_png() -> Vec<u8> {
    use image::{ImageBuffer, Rgb};
    let img = ImageBuffer::from_fn(24, 12, |x, _| Rgb([x as u8 * 10, 0, 0]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

// Pages must not overflow the window, which only a browser can measure.
// `#[ignore]`: needs node and a headless Chrome.

/// A copy of `tests/browser_ask.rs`'s locator: neither crate can call the other's.
pub(crate) fn chrome() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("ENGRAM_CHROME") {
        return Some(std::path::PathBuf::from(p));
    }
    let home = std::env::var("HOME").ok()?;
    let cache = std::path::PathBuf::from(home).join(".cache/ms-playwright");
    let mut found: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(&cache)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let bin = e
                .path()
                .join("chrome-headless-shell-linux64/chrome-headless-shell");
            bin.exists()
                .then(|| (playwright_build(&e.file_name()), bin))
        })
        .collect();
    // Sorted by Playwright build number, not by path, so the newest install wins.
    found.sort();
    found.pop().map(|(_, p)| p)
}

/// Zero where the name has no build number, so it sorts below every numbered one.
fn playwright_build(dir: &std::ffi::OsStr) -> u64 {
    dir.to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// A PDF is included: the corpus page's control row is longest there.
pub(crate) async fn every_page() -> Vec<(String, String)> {
    use crate::store::corpora::{CorpusStatus, Reading};

    let (app, cookie, core) = crate::web::test_support::app_session_and_core().await;
    let doc = core
        .store
        .insert_attached_corpus(
            "h",
            crate::core::ingest::ORIGIN_PDF,
            Some("Betriebssysteme und digitale Spuren — NTFS, Teil 1"),
            None,
            &serde_json::json!({"file": {"name": "FBUDS-03-NTFS-1.pdf"}}),
            Reading::EXTRACTION,
            &crate::store::attachments::NewFile {
                kind: "pdf",
                mime: "application/pdf",
                filename: Some("FBUDS-03-NTFS-1.pdf"),
                bytes: b"%PDF-1.4",
                preview: b"",
                width: None,
                height: None,
            },
        )
        .await
        .unwrap()
        .into_corpus();
    core.store
        .set_read_text(
            &doc.id,
            "NTFS is the New Technology File System.\n\n                 It shipped with Windows NT 3.1 and has been the default since.\n\n                 Every file on the volume has a record in the Master File Table.",
            vec![],
        )
        .await
        .unwrap();
    core.store
        .set_corpus_status(&doc.id, CorpusStatus::Ready)
        .await
        .unwrap();
    crate::jobs::synthesize::segment_all(&core, &doc.id).await;
    let artifact = core.store.artifacts_for_corpus(&doc.id).await.unwrap()[0]
        .id
        .clone();

    let mut out = Vec::new();
    for (name, uri) in [
        ("search", "/ui".to_string()),
        ("insights", "/ui/insights".into()),
        ("settings", "/ui/settings".into()),
        ("day", "/ui/day/2026-09-08".into()),
        ("corpus", format!("/ui/corpora/{}", doc.id)),
        ("artifact", format!("/ui/artifacts/{artifact}")),
    ] {
        let html = crate::web::test_support::get_body(&app, &cookie, &uri).await;
        out.push((name.to_string(), html));
    }
    out
}

pub(crate) fn measure(pages: &[(String, String)], width: &str) -> serde_json::Value {
    let chrome = chrome().expect(
        "no headless Chrome found. Set ENGRAM_CHROME to one, or install Playwright's \
         chrome-headless-shell, to run this test.",
    );
    let root = env!("CARGO_MANIFEST_DIR");
    let listed: Vec<serde_json::Value> = pages
        .iter()
        .map(|(name, html)| serde_json::json!({"name": name, "html": html}))
        .collect();
    let dir = tempfile::tempdir().expect("a place to hand the pages over");
    let list = dir.path().join("pages.json");
    std::fs::write(&list, serde_json::to_vec(&listed).unwrap()).unwrap();

    let out = std::process::Command::new("node")
        .arg(format!("{root}/tests/browser/page_width.js"))
        .arg(root)
        .arg(&chrome)
        .arg(&list)
        .arg(width)
        .output()
        .expect("node is needed to run this test");
    // The harness's exit and stderr first, so a node crash is reported as itself.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the width harness exited with {}: {stderr}",
        out.status
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .last()
        .unwrap_or_else(|| panic!("the harness printed nothing: {stdout}{stderr}"));
    let run: serde_json::Value =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
    assert_eq!(
        run["pages"].as_array().map(|p| p.len()),
        Some(pages.len()),
        "a result per page: {run}"
    );
    run
}

pub(crate) fn nothing_is_broken(run: &serde_json::Value, width: &str) {
    for p in run["pages"].as_array().unwrap() {
        assert!(p["error"].is_null(), "{p}");
        let doc = p["scrollWidth"].as_i64().unwrap();
        let window = p["clientWidth"].as_i64().unwrap();
        assert!(
            doc <= window,
            "at {width}px the {} page is {doc}px wide in a {window}px window, so the \
             whole page scrolls sideways. What sticks out: {}",
            p["name"],
            p["over"],
        );
        let w: i64 = width.parse().unwrap();
        let shown = p["tabShown"].as_bool().unwrap_or(false);
        let items = p["tabItems"].as_array().cloned().unwrap_or_default();
        let signed_in = !items.is_empty();
        if signed_in && w < 768 {
            assert!(shown, "at {width}px the {} page has no tab bar", p["name"]);
            assert!((3..=5).contains(&items.len()), "{}: {items:?}", p["name"]);
            for i in &items {
                assert!(
                    !i["label"].as_str().unwrap_or("").is_empty(),
                    "an unlabelled tab: {i}"
                );
                assert!(
                    i["height"].as_f64().unwrap_or(0.0) >= 44.0,
                    "a tab under 44px: {i}"
                );
            }
        }
        let fab = p["fabHeight"].as_f64().unwrap_or(0.0);
        let in_search = ["search", "artifact"].contains(&p["name"].as_str().unwrap_or(""));
        if signed_in && w < 768 && !in_search {
            assert!(
                fab >= 44.0,
                "at {width}px the {} page has no capture button: {p}",
                p["name"]
            );
        }
        if w >= 768 || in_search {
            assert!(
                fab == 0.0,
                "at {width}px the {} page shows the capture button",
                p["name"]
            );
        }
        if signed_in && w >= 768 {
            assert!(
                !shown,
                "at {width}px the {} page still shows the tab bar",
                p["name"]
            );
        }
        if let Some(focus) = p["focusRem"].as_f64() {
            assert!(
                focus <= 46.05,
                "at {width}px the {} column is {focus}rem wide",
                p["name"]
            );
        }
        let squeezed = p["squeezed"].as_array().expect("a squeeze list");
        assert!(
            squeezed.is_empty(),
            "at {width}px the {} page crushes text into a column narrower than its own \
             letters, which the browser then clips away entirely: {squeezed:?}",
            p["name"],
        );
    }
}

pub(crate) async fn app_with_session() -> (axum::Router, String) {
    let (app, cookie, _core) = app_session_and_core().await;
    (app, cookie)
}

pub(crate) async fn app_session_and_core() -> (axum::Router, String, crate::core::Core) {
    let core = crate::core::test_support::test_core().await;
    let handle = core.clone();
    let (app, cookie) = app_with_cookie(core).await;
    (app, cookie, handle)
}

pub(crate) async fn get_body(app: &axum::Router, cookie: &str, uri: &str) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "GET {uri}");
    body_of(res).await
}

pub(crate) fn form(uri: &str, cookie: &str, body: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method("POST")
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(crate) async fn searched_app() -> (axum::Router, String, crate::core::Core, String, String) {
    let mut core = crate::core::test_support::test_core().await;
    core.learn.enabled = true;
    let handle = core.clone();
    let (app, cookie) = app_with_cookie(core).await;
    let src = handle
        .store
        .insert_corpus("raw", "web", None)
        .await
        .unwrap();
    let a = handle
        .store
        .insert_artifacts(
            &src.id,
            &[crate::store::artifacts::NewArtifact {
                text: "mounting the image".into(),
                title: Some("mount".into()),
                ..Default::default()
            }],
        )
        .await
        .unwrap()[0]
        .id
        .clone();
    let event = handle
        .store
        .record_search(
            crate::store::feedback::NewEvent {
                fold_onto: None,
                query: "image will not mount".into(),
                door: crate::store::feedback::Door::Ui,
                scope: Some(crate::store::TEST_SUBJECT.into()),
                filters: "{}".into(),
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                candidates: vec![crate::store::feedback::NewCandidate {
                    artifact_id: a.clone(),
                    score: 1.0,
                    similarity: Some(0.5),
                    shown: true,
                    ..Default::default()
                }],
                answered: false,
                context: None,
            },
            0,
        )
        .await
        .unwrap();
    (app, cookie, handle, a, event)
}

pub(crate) async fn an_unread_image(core: &crate::core::Core) -> String {
    core.ingest_image(crate::core::ingest::ImageCapture {
        bytes: a_png(),
        filename: Some("p.png".into()),
        title_hint: None,
        note: None,
        lang: crate::infer::lang::Lang::default(),
    })
    .await
    .unwrap()
    .id
}

/// The ask door opens only over a held base.
pub(crate) async fn hold_something(core: &crate::core::Core) {
    core.ingest_capture(crate::core::ingest::Capture::new(
        "LevelDB tombstones survive compaction longer than the manual admits.",
        "ui",
    ))
    .await
    .unwrap();
}

pub(crate) async fn app_holding_something() -> (axum::Router, String) {
    let (app, cookie, core) = app_session_and_core().await;
    hold_something(&core).await;
    (app, cookie)
}

/// A separate builder: the router owns its own clone of the core, so a later flag flip would not reach it.
pub(crate) async fn app_session_and_core_with_feedback() -> (axum::Router, String, crate::core::Core)
{
    let mut core = crate::core::test_support::test_core().await;
    core.learn.enabled = true;
    let handle = core.clone();
    let (app, cookie) = app_with_cookie(core).await;
    (app, cookie, handle)
}

pub(crate) fn put_form(uri: &str, cookie: &str, body: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method("PUT")
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(crate) async fn post_ask(app: &axum::Router, cookie: &str, q: &str) -> String {
    let res = app
        .clone()
        .oneshot(form("/ui/ask", cookie, &format!("q={q}")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "the question was not parked");
    crate::web::test_support::json_of(res).await["id"]
        .as_str()
        .expect("parking hands back an id")
        .to_string()
}

pub(crate) async fn get_stream(app: &axum::Router, cookie: &str, id: &str) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .uri(format!("/ui/ask/{id}/stream"))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

pub(crate) async fn ask_over_sse(app: &axum::Router, cookie: &str, q: &str) -> String {
    let id = post_ask(app, cookie, q).await;
    let res = get_stream(app, cookie, &id).await;
    assert_eq!(res.status(), StatusCode::OK);
    body_of(res).await
}

/// The payload is JSON, so the fragment survives its blank lines.
pub(crate) fn done_html(body: &str) -> String {
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .find(|v| v.get("html").is_some())
        .unwrap_or_else(|| panic!("no done event in {body}"));
    data["html"].as_str().unwrap().to_string()
}

pub(crate) async fn app_with_embedded_corpus() -> (axum::Router, String) {
    let core = crate::core::test_support::test_core().await;
    let out = core
        .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
        .await
        .unwrap();
    crate::jobs::synthesize::segment_all(&core, &out.id).await;
    crate::jobs::embed::run_corpus(&core, &out.id)
        .await
        .unwrap();

    app_with_cookie(core).await
}

pub(crate) fn flat(html: &str) -> String {
    html.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Not `html.contains("load")`: the context offer and page prose carry it too.
pub(crate) fn trigger_of(html: &str) -> String {
    let form = html.split(r#"id="box-form""#).nth(1).expect("the box form");
    let trigger = form.split(r#"hx-trigger=""#).nth(1).expect("its trigger");
    trigger.split('"').next().unwrap().to_string()
}

pub(crate) async fn app_recommending() -> (axum::Router, String, crate::store::Store, String) {
    let mut core = crate::core::test_support::test_core().await;
    core.recommend.enabled = true;
    core.learn.enabled = true;
    let store = core.store.clone();
    let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
    let a = core
        .store
        .insert_artifacts(
            &src.id,
            &[crate::store::artifacts::NewArtifact {
                text: "when the recycling centre is open".into(),
                title: Some("recycling centre".into()),
                ..Default::default()
            }],
        )
        .await
        .unwrap()
        .remove(0);
    core.vectors
        .upsert(vec![crate::vector::VectorPoint {
            vector: vec![1.0; 8],
            sparse: Default::default(),
            payload: crate::vector::VectorPayload {
                artifact_id: a.id.clone(),
                corpus_id: src.id.clone(),
                text: a.text.clone(),
                title: Some("recycling centre".into()),
                ..Default::default()
            },
        }])
        .await
        .unwrap();
    let background = core.background.clone();
    let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
    BACKGROUND.with(|b| *b.borrow_mut() = Some(background));
    (app, cookie, store, a.id)
}

thread_local! {
    static BACKGROUND: std::cell::RefCell<Option<std::sync::Arc<crate::core::background::Background>>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) async fn drain() {
    let b = BACKGROUND.with(|b| b.borrow().clone());
    if let Some(b) = b {
        b.wait_idle().await;
    }
}

pub(crate) fn urlencoding_of(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

pub(crate) async fn artifacts(core: &crate::core::Core, titles: &[&str]) -> Vec<String> {
    let src = core.store.insert_corpus("x", "web", None).await.unwrap();
    let new: Vec<crate::store::artifacts::NewArtifact> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| crate::store::artifacts::NewArtifact {
            ordinal: i as i64,
            text: format!("body of {t}"),
            title: Some((*t).to_string()),
            ..Default::default()
        })
        .collect();
    core.store
        .insert_artifacts(&src.id, &new)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect()
}

pub(crate) fn rail_html(body: &str) -> String {
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .find(|v| v.get("rail").is_some())
        .unwrap_or_else(|| panic!("no citations event in {body}"));
    data["rail"].as_str().unwrap().to_string()
}

pub(crate) fn pulled(html: &str, open: &str, end: char) -> Vec<String> {
    html.match_indices(open)
        .map(|(at, m)| {
            html[at + m.len()..]
                .chars()
                .take_while(|c| *c != end)
                .collect()
        })
        .collect()
}

pub(crate) async fn session_with_an_artifact() -> (axum::Router, String, crate::core::Core, String)
{
    let (app, cookie, core) = app_session_and_core().await;
    let out = core
        .ingest_capture(crate::core::ingest::Capture::new(
            "The pool holds sixteen connections.",
            "ui",
        ))
        .await
        .unwrap();
    crate::jobs::test_support::drain(&core).await;
    let aid = core
        .store
        .artifacts_for_corpus(&out.id)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.in_results())
        .expect("a live artifact")
        .id;
    (app, cookie, core, aid)
}

pub(crate) fn row_on(
    subject: &str,
    kind: crate::store::actions::Kind,
) -> crate::store::actions::NewAction {
    crate::store::actions::NewAction {
        job: crate::store::actions::Job::Dedupe,
        kind,
        subject_id: subject.to_string(),
        survivor_id: None,
        detail: None,
        evidence: serde_json::json!({}),
        pair_score: None,
    }
}

/// `Chunk` has no `Default` on purpose, so the fixture names every field.
pub(crate) fn chunk_fixture(title: Option<&str>, text: &str) -> crate::store::artifacts::Chunk {
    crate::store::artifacts::Chunk {
        id: "a".into(),
        corpus_id: Some("s".into()),
        provenance: crate::store::artifacts::Provenance::Captured,
        source_count: 0,
        ordinal: 56,
        text: text.into(),
        corpus_span: None,
        title: title.map(str::to_string),
        category: None,
        tags: vec![],
        embed_state: crate::store::artifacts::EmbedState::Embedded,
        embed_model: None,
        created_at: 0,
        embed_rev: 0,
        segment_idx: None,
        flags: vec![],
        flag_detail: None,
        superseded_by: None,
        caveats: vec![],
        status: crate::store::artifacts::ArtifactStatus::Active,
        last_verified_at: None,
        cues: vec![],
        retired_at: None,
        reaped_at: None,
    }
}

/// Loopback for local mode, an https public URL for OIDC.
pub fn deployment_for(mode: crate::config::AuthMode) -> crate::web::security::Deployment {
    let mut d = crate::web::security::Deployment::loopback(std::net::SocketAddr::from((
        [127, 0, 0, 1],
        8080,
    )));
    if mode == crate::config::AuthMode::Oidc {
        d.public_url = url::Url::parse("https://engram.test").ok();
    }
    d
}
