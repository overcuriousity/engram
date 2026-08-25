//! One way to build the app under test, shared by every `web/` test module.

use crate::core::Core;
use axum::body::Body;
use axum::http::Request;
use axum::response::Response;

/// The real router over `core`, in local auth mode with no password
/// configured (`local`); pass `Some(cfg)` to test the login form itself.
pub fn router(core: Core, local: Option<crate::config::LocalConfig>) -> axum::Router {
    router_as(core, local, true)
}

/// The same, for a user who has not been granted the judge.
pub fn router_ungranted(core: Core, local: Option<crate::config::LocalConfig>) -> axum::Router {
    router_as(core, local, false)
}

/// A one-tenant registry around `core`, and the real router over it.
///
/// Every test written against the single-user app goes through here, which is
/// the point: if tenancy needed edits scattered across the web tests, the
/// extractor boundary would be in the wrong place, and this is where that
/// would show.
fn router_as(
    core: Core,
    local: Option<crate::config::LocalConfig>,
    can_judge: bool,
) -> axum::Router {
    let user = crate::store::control::User {
        subject: crate::store::TEST_SUBJECT.into(),
        email: None,
        slug: crate::store::control::slug_for(crate::store::TEST_SUBJECT),
        can_judge,
        created_at: 0,
        last_seen_at: 0,
    };
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

/// An `AppState` over one already-open tenant, for the tests that need to hold
/// the state rather than only the router.
pub fn state_over(core: Core, mode: crate::config::AuthMode) -> crate::web::state::AppState {
    let cfg = std::sync::Arc::new(crate::config::Config::test_default());
    let user = crate::store::control::User {
        subject: crate::store::TEST_SUBJECT.into(),
        email: None,
        slug: crate::store::control::slug_for(crate::store::TEST_SUBJECT),
        can_judge: true,
        created_at: 0,
        last_seen_at: 0,
    };
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
    let user = crate::store::control::User {
        subject: crate::store::TEST_SUBJECT.into(),
        email: None,
        slug: crate::store::control::slug_for(crate::store::TEST_SUBJECT),
        can_judge,
        created_at: 0,
        last_seen_at: 0,
    };
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
    (router(core, None), token)
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
