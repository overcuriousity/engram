//! `ETag` on every JSON read, and `If-None-Match` honoured.
//!
//! A phone on a bad link revalidating a cache is the normal case for the API,
//! and a `304` is the difference between a usable app on a train and a
//! spinner. So this is one layer over the whole API router rather than a
//! decision each handler makes: a route added next year revalidates without
//! anybody remembering that it should.
//!
//! The tag is a hash of the body. The handler still runs — what a `304` saves
//! is the transfer, which is the part that costs on the far side of a VPN. A
//! per-route version counter would save the compute too, and would be a
//! second account of whether the data changed, maintained by hand beside the
//! data. A hash of what was about to be sent cannot disagree with it.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// Store it, and ask before using it. Not `no-store`: the whole point is that
/// the client keeps the body and spends one round trip learning it is current.
pub const REVALIDATE: &str = "private, no-cache";

/// A strong tag over these bytes, quoted as the header wants it. Half a
/// SHA-256: the tag distinguishes versions of one resource for one reader, and
/// 128 bits is already far past any chance of two of them colliding.
pub fn tag_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("\"{}\"", hex::encode(&digest[..16]))
}

/// Whether an `If-None-Match` value names this tag.
///
/// The header is a list, a member may be weak, and `*` matches anything that
/// exists. The comparison for `If-None-Match` is the weak one (RFC 9110
/// §13.1.2), so `W/` is dropped before comparing rather than refused.
pub fn matches(if_none_match: &str, tag: &str) -> bool {
    if_none_match
        .split(',')
        .map(str::trim)
        .any(|m| m == "*" || m.strip_prefix("W/").unwrap_or(m) == tag)
}

/// The `304` for a tag that matched: the tag again, the caching rule again,
/// and no body.
pub fn not_modified(tag: &str) -> Response {
    let mut res = StatusCode::NOT_MODIFIED.into_response();
    stamp(&mut res, tag);
    res
}

/// Put the tag and the caching rule on a response.
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

    // Only a successful JSON answer. An error is not a version of the
    // resource; a stream has no whole body to hash; and a handler that tagged
    // its own answer — the byte routes, from a hash they already hold — knew
    // something this layer does not.
    let json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if res.status() != StatusCode::OK || !json || res.headers().contains_key(header::ETAG) {
        return res;
    }
    // Nor one that stated its own caching rule. `/vectors/sample` says
    // `no-store` because a browser cache outlives a sign-out and is keyed on
    // the URL alone; replacing that with "store it and revalidate" would undo
    // the reason it was written, and a tag on an answer nobody may keep is a
    // tag nobody can send back.
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
