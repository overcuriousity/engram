//! One way to build the app under test, shared by every `web/` test module.

use crate::core::Core;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use tower::ServiceExt as _;

/// The real router over `core`, in local auth mode with no password
/// configured (`local`); pass `Some(cfg)` to test the login form itself.
///
/// A one-tenant registry around `core`, and the real router over it. Every
/// test written against the single-user app goes through here, which is the
/// point: if tenancy needed edits scattered across the web tests, the
/// extractor boundary would be in the wrong place, and this is where that
/// would show.
///
/// The user it registers holds the judge grant. A router alone cannot express
/// the ungranted case, because the gate is only reachable by someone signed
/// in — see `app_with_cookie_ungranted`, which carries a session.
pub async fn router(core: Core, local: Option<crate::config::LocalConfig>) -> axum::Router {
    let user = granted_user(&core, true).await;
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
            secure_cookies: false,
        }),
        config_path: std::sync::Arc::new(scratch_config()),
        ask_handoff: Default::default(),
    })
}

/// The registered user, with the grant written where the judge gate reads it.
///
/// Both places: the `User` the registry hands to a handler, and the row in the
/// control database the gate consults on every judge request. They have to
/// agree, and the row is the one that decides — see `web::tenant::CanJudge`.
async fn granted_user(core: &Core, can_judge: bool) -> crate::store::control::User {
    let subject = crate::store::TEST_SUBJECT;
    core.store.control.provision(subject, None).await.ok();
    core.store
        .control
        .set_can_judge(subject, can_judge)
        .await
        .expect("write the judge grant");
    crate::store::control::User {
        subject: subject.into(),
        email: None,
        slug: crate::store::control::slug_for(subject),
        can_judge,
        created_at: 0,
    }
}

/// An `AppState` over one already-open tenant, for the tests that need to hold
/// the state rather than only the router.
pub async fn state_over(core: Core, mode: crate::config::AuthMode) -> crate::web::state::AppState {
    let cfg = std::sync::Arc::new(crate::config::Config::test_default());
    let user = granted_user(&core, true).await;
    crate::web::state::AppState {
        tenants: std::sync::Arc::new(crate::tenants::Tenants::single(cfg.clone(), core, user)),
        config: cfg,
        auth: std::sync::Arc::new(crate::web::state::AuthContext {
            mode,
            local: None,
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            secure_cookies: mode == crate::config::AuthMode::Oidc,
        }),
        config_path: std::sync::Arc::new(scratch_config()),
        ask_handoff: Default::default(),
    }
}

/// A `config.toml` of its own per app under test.
///
/// The apply path writes the file the server was started with, so two tests
/// sharing one would be asserting against whichever ran last. One directory
/// for the whole test binary, one file per app in it.
pub(crate) fn scratch_config() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().expect("scratch config dir"));
    let path = dir.path().join(format!("{}.toml", crate::store::new_id()));
    std::fs::write(
        &path,
        "# a comment the apply path must not eat\n\
         [vector]\n\
         recency_weight = 0.05\n\
         per_source_cap = 3\n",
    )
    .expect("scratch config");
    path
}

/// A router over `core` plus a browser session cookie for `user-1`.
pub async fn app_with_cookie(core: Core) -> (axum::Router, String) {
    let (app, cookie, _) = app_with_state(core).await;
    (app, cookie)
}

/// The same, for a signed-in user who has not been granted the judge. The
/// session is real; only the grant is missing, which is the only thing the
/// gate is allowed to be answering.
pub async fn app_with_cookie_ungranted(core: Core) -> (axum::Router, String) {
    let (app, cookie, _) = app_with_state_as(core, false).await;
    (app, cookie)
}

/// `app_with_cookie`, plus the state behind it — what a test needs when it has
/// to read something a handler wrote outside the database, such as the
/// configuration file the apply path rewrites.
pub async fn app_with_state(core: Core) -> (axum::Router, String, crate::web::state::AppState) {
    app_with_state_as(core, true).await
}

async fn app_with_state_as(
    core: Core,
    can_judge: bool,
) -> (axum::Router, String, crate::web::state::AppState) {
    let cid = crate::store::new_id();
    core.store
        .control
        .insert_session(&cid, "user-1", None, 3600)
        .await
        .unwrap();
    let cfg = std::sync::Arc::new(crate::config::Config::test_default());
    let user = granted_user(&core, can_judge).await;
    let state = crate::web::state::AppState {
        tenants: std::sync::Arc::new(crate::tenants::Tenants::single(cfg.clone(), core, user)),
        config: cfg,
        auth: std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Local,
            local: None,
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            secure_cookies: false,
        }),
        config_path: std::sync::Arc::new(scratch_config()),
        ask_handoff: Default::default(),
    };
    (
        crate::web::router(state.clone()),
        format!("engram_session={cid}"),
        state,
    )
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

/// One file part in a multipart body. `mime` of `None` omits the part's
/// `Content-Type` header entirely, which is legal and which a client may do.
pub struct FilePart<'a> {
    pub field: &'a str,
    pub filename: &'a str,
    pub mime: Option<&'a str>,
    pub body: &'a [u8],
}

/// A minimal multipart POST — text fields first, then the file parts — with a
/// bearer token. Hand-rolled rather than pulling a builder in for a few tests.
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

/// A small PNG for the image door.
pub fn a_png() -> Vec<u8> {
    use image::{ImageBuffer, Rgb};
    let img = ImageBuffer::from_fn(24, 12, |x, _| Rgb([x as u8 * 10, 0, 0]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

// ── A signed-in app, and the shapes a page test asks for ────────────────────
//
// These grew inside `web::ui`'s test module and were reachable only from it,
// which is part of why that module became the place every page's tests
// lived. Here they belong to every `web/` module, and a page split out of
// `ui.rs` keeps its tests instead of leaving them behind.

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

/// A recording session, one artifact, and one captured search of this
/// user's whose pool holds it.
pub(crate) async fn searched_app() -> (axum::Router, String, crate::core::Core, String, String) {
    searched_app_tuned(None).await
}

/// `searched_app`, with the judgement floor low enough that a verdict on
/// the bar can cross it — the bar is the labeller now, so the bar is what
/// pays for a sweep.
pub(crate) async fn searched_app_tuned(
    floor: Option<i64>,
) -> (axum::Router, String, crate::core::Core, String, String) {
    let mut core = crate::core::test_support::test_core().await;
    core.learn.enabled = true;
    if let Some(n) = floor {
        core.feedback.tune.min_judgements = n;
    }
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

/// A captured photograph whose vision read has not landed.
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

/// One source, so the base is not empty.
///
/// The ask door only opens over a held base — with nothing stored it
/// redirects to the plain page, because the workspace renders no Ask verb
/// there and the door would be a question in a box with no way to send it.
/// Every test below that wants the ask *page* wants a base with something
/// in it first.
pub(crate) async fn hold_something(core: &crate::core::Core) {
    core.ingest_capture(crate::core::ingest::Capture::new(
        "LevelDB tombstones survive compaction longer than the manual admits.",
        "ui",
    ))
    .await
    .unwrap();
}

/// A session over a base holding one source. See `hold_something`.
pub(crate) async fn app_holding_something() -> (axum::Router, String) {
    let (app, cookie, core) = app_session_and_core().await;
    hold_something(&core).await;
    (app, cookie)
}

/// A session whose core records searches, which is what the association
/// features are gated on. `app_session_and_core` cannot be reused: the
/// router owns its own clone of the core, so flipping a flag afterwards
/// changes the handle and not the app.
pub(crate) async fn app_session_and_core_with_feedback() -> (axum::Router, String, crate::core::Core)
{
    let mut core = crate::core::test_support::test_core().await;
    core.learn.enabled = true;
    let handle = core.clone();
    let (app, cookie) = app_with_cookie(core).await;
    (app, cookie, handle)
}

/// The same, for the one route that takes a `PUT`: editing an artifact.
pub(crate) fn put_form(uri: &str, cookie: &str, body: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method("PUT")
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The first half of the two-request ask: park the question, take the id.
/// `q` is form-encoded, as it is in the body it goes into.
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

/// The second half: spend the id and stream.
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

/// One whole ask over the wire, as the page performs it.
pub(crate) async fn ask_over_sse(app: &axum::Router, cookie: &str, q: &str) -> String {
    let id = post_ask(app, cookie, q).await;
    let res = get_stream(app, cookie, &id).await;
    assert_eq!(res.status(), StatusCode::OK);
    body_of(res).await
}

/// The HTML the page swaps in, pulled out of the `done` frame the way the
/// browser reads it: the payload is JSON, so the fragment survives the
/// blank lines its markdown carries.
pub(crate) fn done_html(body: &str) -> String {
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .find(|v| v.get("html").is_some())
        .unwrap_or_else(|| panic!("no done event in {body}"));
    data["html"].as_str().unwrap().to_string()
}

/// A session plus a corpus that has been through synthesis and embedding,
/// which is the only state in which there is anything to facet or to find a
/// neighbour among.
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

/// Markup with every run of whitespace collapsed, so an assertion about an
/// attribute pair does not also assert where the template wrapped a line.
pub(crate) fn flat(html: &str) -> String {
    html.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The box form's `hx-trigger`, on its own. `html.contains("load")` is
/// not the same question: the context offer carries `hx-trigger="load"`
/// too, and so does the word inside half the prose on the page.
pub(crate) fn trigger_of(html: &str) -> String {
    let form = html.split(r#"id="box-form""#).nth(1).expect("the box form");
    let trigger = form.split(r#"hx-trigger=""#).nth(1).expect("its trigger");
    trigger.split('"').next().unwrap().to_string()
}

/// A session with the recommender on, plus one artifact old enough and
/// unseen enough that `resurface` returns it.
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
    // Held so a test can drain the recording writes rather than sleep.
    BACKGROUND.with(|b| *b.borrow_mut() = Some(background));
    (app, cookie, store, a.id)
}

// Where `app_recommending` parks the background handle for `drain` to find.
thread_local! {
    static BACKGROUND: std::cell::RefCell<Option<std::sync::Arc<crate::core::background::Background>>> =
        const { std::cell::RefCell::new(None) };
}

/// The recording writes run off the request path. Drain them rather than
/// sleeping and hoping.
pub(crate) async fn drain() {
    let b = BACKGROUND.with(|b| b.borrow().clone());
    if let Some(b) = b {
        b.wait_idle().await;
    }
}

/// Percent-encoding for the handful of characters these test bodies carry.
pub(crate) fn urlencoding_of(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

/// One corpus with `n` artifacts, titled so the ops page can be searched
/// for them.
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

/// The excerpt list, out of the `citations` frame, the way the page reads
/// it. Keyed apart from the answer's `html` so `done_html` above cannot pick
/// this frame up by mistake.
pub(crate) fn rail_html(body: &str) -> String {
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .find(|v| v.get("rail").is_some())
        .unwrap_or_else(|| panic!("no citations event in {body}"));
    data["rail"].as_str().unwrap().to_string()
}

/// Every run that follows `open`, up to the next `end`, in document order.
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

/// One live artifact on a fresh session.
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
