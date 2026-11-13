//! `ETag` on every JSON read, and `If-None-Match` honoured, as one layer over the API router.
//! The tag hashes the body: the handler still runs, but a hash cannot disagree with what is sent.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// Not `no-store`: the client keeps the body and revalidates in one round trip.
pub const REVALIDATE: &str = "private, no-cache";

/// Half a SHA-256, quoted; 128 bits is ample for versions of one resource.
pub fn tag_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("\"{}\"", hex::encode(&digest[..16]))
}

/// The header is a list, members may be weak, and `*` matches anything; weak comparison per RFC 9110 §13.1.2.
pub fn matches(if_none_match: &str, tag: &str) -> bool {
    if_none_match
        .split(',')
        .map(str::trim)
        .any(|m| m == "*" || m.strip_prefix("W/").unwrap_or(m) == tag)
}

/// The tag again, the caching rule again, and no body.
pub fn not_modified(tag: &str) -> Response {
    let mut res = StatusCode::NOT_MODIFIED.into_response();
    stamp(&mut res, tag);
    res
}

pub fn stamp(res: &mut Response, tag: &str) {
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(tag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE));
}

pub async fn layer(req: Request, next: Next) -> Response {
    if req.method() != Method::GET {
        return next.run(req).await;
    }
    let asked = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let res = next.run(req).await;

    // Only a successful JSON answer not already tagged by its handler; a stream has no whole body.
    let json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if res.status() != StatusCode::OK || !json || res.headers().contains_key(header::ETAG) {
        return res;
    }
    // Nor one with its own caching rule: `/vectors/sample` is `no-store` because a cache outlives sign-out.
    if res.headers().contains_key(header::CACHE_CONTROL) {
        return res;
    }

    let (parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            return crate::error::Error::Internal(format!("buffering a body to tag it: {e}"))
                .into_response();
        }
    };
    let tag = tag_of(&bytes);
    if asked.is_some_and(|a| matches(&a, &tag)) {
        return not_modified(&tag);
    }
    let mut res = Response::from_parts(parts, Body::from(bytes));
    stamp(&mut res, &tag);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_quoted_and_stable() {
        assert_eq!(tag_of(b"x"), tag_of(b"x"));
        assert_ne!(tag_of(b"x"), tag_of(b"y"));
        assert!(tag_of(b"x").starts_with('"') && tag_of(b"x").ends_with('"'));
    }

    #[test]
    fn if_none_match_takes_a_list_a_weak_tag_and_a_star() {
        let t = tag_of(b"x");
        assert!(matches(&t, &t));
        assert!(matches(&format!("\"other\", W/{t}"), &t));
        assert!(matches("*", &t));
        assert!(!matches("\"other\"", &t));
    }
}
