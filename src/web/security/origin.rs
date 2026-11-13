use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::Ctx;

/// Cookie-authenticated writes must come from our own origin.
pub(super) async fn origin_check(
    State(ctx): State<Arc<Ctx>>,
    req: Request,
    next: Next,
) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let h = req.headers();
    // A bearer without a cookie carries no ambient credential; the extension posts from its own origin.
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer "));
    if bearer && !h.contains_key(header::COOKIE) {
        return next.run(req).await;
    }
    // Under `no-referrer`, same-origin form posts send `Origin: null`.
    let origin = h
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| *o != "null");
    let site = h.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    let allowed = match (origin, site) {
        (Some(o), _) if !ctx.dep.origin_allowed(o) => false,
        (_, Some(s)) => matches!(s, "same-origin" | "none"),
        (Some(_), None) => true,
        (None, None) => !h.contains_key(header::COOKIE),
    };
    if allowed {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "This request came from another site and was blocked.",
        )
            .into_response()
    }
}
