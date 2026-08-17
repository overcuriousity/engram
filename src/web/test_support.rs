//! One way to build the app under test, shared by every `web/` test module.

use crate::core::Core;
use axum::body::Body;
use axum::http::Request;
use axum::response::Response;

/// The real router over `core`, in local auth mode with no password
/// configured (`local`); pass `Some(cfg)` to test the login form itself.
pub fn router(core: Core, local: Option<crate::config::LocalConfig>) -> axum::Router {
    crate::web::router(crate::web::state::AppState {
        core,
        auth: std::sync::Arc::new(crate::web::state::AuthContext {
            mode: crate::config::AuthMode::Local,
            local,
            oidc: None,
            pending: crate::auth::oidc::PendingStore::new(),
            secure_cookies: false,
        }),
    })
}

/// A router over `core` plus a browser session cookie for `user-1`.
pub async fn app_with_cookie(core: Core) -> (axum::Router, String) {
    let cid = crate::store::new_id();
    core.store
        .insert_session(&cid, "user-1", None, 3600)
        .await
        .unwrap();
    (router(core, None), format!("engram_session={cid}"))
}

/// A router over `core` plus a bearer token for `user-1`.
pub async fn app_with_token(core: Core) -> (axum::Router, String) {
    let (_, token) = crate::auth::tokens::mint(&core.store, "test", "user-1", None)
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
