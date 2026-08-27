//! The share sheet, which is the phone's own capture gesture.
//!
//! An installed app on Android may put itself in the system share sheet, and a
//! share arrives here as a multipart POST the platform composed — not a form on
//! a page of ours. The parts are the same ones `/api/v1/capture` reads, and
//! they are read by the same code; what differs is only the answer, which is a
//! page for a person rather than JSON for a client.

use crate::core::ingest::ORIGIN_SHARE;
use crate::error::{Error, Result};
use crate::tenants::Tenant;
use crate::web::api::{only_a_url, read_capture_parts};
use crate::web::state::AppState;
use axum::Router;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;

/// Store what was shared and land on the corpus it became.
///
/// The corpus page rather than a confirmation that closes itself, because it is
/// the one surface that can say *held for review* when a share is parked as a
/// near-duplicate (`Core::ingest_capture`). On a phone that is the only moment
/// the operator would ever learn that what they shared is stored but not
/// searchable.
async fn share(tenant: Tenant, multipart: axum::extract::Multipart) -> Result<Response> {
    let (mut fields, files) = read_capture_parts(multipart).await?;
    let title = fields.remove("title");

    // A share carries `url` and `text` for the same thing, and the link is the
    // better capture of the two: the text is usually the page's title repeated.
    let shared_url = fields.remove("url").or_else(|| {
        fields
            .get("text")
            .and_then(|t| only_a_url(t).map(|u| u.to_string()))
    });
    if let Some(raw) = shared_url {
        let u = url::Url::parse(&raw).map_err(|e| Error::Validation(format!("url: {e}")))?;
        if !matches!(u.scheme(), "http" | "https") {
            return Err(Error::Validation(format!(
                "url: `{}` is not a scheme a page is read over",
                u.scheme()
            )));
        }
        let out = tenant.core.ingest_url(&u, title, None).await?;
        return Ok(Redirect::to(&format!("/ui/corpora/{}", out.id)).into_response());
    }

    if let Some(text) = fields.remove("text") {
        let out = tenant
            .core
            .ingest_capture(crate::core::ingest::Capture::new(text, ORIGIN_SHARE).with_title(title))
            .await?;
        return Ok(Redirect::to(&format!("/ui/corpora/{}", out.id)).into_response());
    }

    // Several files land on the first of them: a list of four ids is not a
    // destination, and the queue on the capture page shows the rest arriving.
    let mut first = None;
    for f in files {
        let out = tenant
            .core
            .ingest_file(
                f.bytes.to_vec(),
                f.filename,
                title.clone(),
                None,
                ORIGIN_SHARE,
            )
            .await?;
        first.get_or_insert(out.id);
    }
    match first {
        Some(id) => Ok(Redirect::to(&format!("/ui/corpora/{id}")).into_response()),
        None => Err(Error::Validation(
            "that share carried nothing to capture".into(),
        )),
    }
}

/// The share door, with the ceiling a phone photo needs.
///
/// The global limit is `MAX_BODY_BYTES`, which is sized for prose; a share from
/// a camera roll is several times that. The route takes the widest of the
/// per-kind ceilings for the same reason `/api/v1/capture` does, and the ingest
/// path each file reaches re-imposes its own.
pub fn share_router(image_max_bytes: usize, pdf_max_bytes: usize) -> Router<AppState> {
    Router::new().route(
        "/ui/share",
        post(share).layer(axum::extract::DefaultBodyLimit::max(
            image_max_bytes.max(pdf_max_bytes),
        )),
    )
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    /// A multipart body with only text fields, in the shape a platform share
    /// sheet composes one.
    fn shared(fields: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (k, v) in fields {
            body.push_str(&format!(
                "--b\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
            ));
        }
        body.push_str("--b--\r\n");
        body
    }

    #[tokio::test]
    async fn a_share_without_a_session_is_refused() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/share")
                    .method("POST")
                    .header("content-type", "multipart/form-data; boundary=b")
                    .body(axum::body::Body::from(shared(&[(
                        "text",
                        "a procedure worth keeping",
                    )])))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            res.status(),
            StatusCode::SEE_OTHER,
            "an unauthenticated share must not store"
        );
    }

    #[tokio::test]
    async fn a_shared_note_lands_on_the_corpus_it_created() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core.clone()).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/share")
                    .method("POST")
                    .header("cookie", cookie)
                    .header("content-type", "multipart/form-data; boundary=b")
                    .body(axum::body::Body::from(shared(&[
                        ("title", "Mounting"),
                        ("text", "losetup takes the offset in bytes, not sectors"),
                    ])))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let to = res
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(to.starts_with("/ui/corpora/"), "landed on {to}");

        let id = to.trim_start_matches("/ui/corpora/");
        let stored = core.store.get_corpus(id).await.expect("stored");
        assert_eq!(
            stored.origin,
            crate::core::ingest::ORIGIN_SHARE,
            "a share says it came from a share sheet"
        );
    }

    #[test]
    fn the_manifest_declares_the_share_target() {
        let m = crate::web::assets::Assets::get("manifest.webmanifest").expect("manifest");
        let v: serde_json::Value = serde_json::from_slice(&m.data).expect("valid json");
        assert_eq!(v["share_target"]["action"], "/ui/share");
        assert_eq!(v["share_target"]["method"], "POST");
        assert_eq!(v["share_target"]["enctype"], "multipart/form-data");
        // The file part's name has to match what the door reads, or a shared
        // photo arrives as a field nobody looks at.
        assert_eq!(v["share_target"]["params"]["files"][0]["name"], "file");
    }
}
