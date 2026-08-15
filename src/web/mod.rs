pub mod api;
pub mod assets;
pub mod auth_routes;
pub mod corpus_view;
pub mod extension;
pub mod judge;
pub mod markdown;
pub mod pair;
pub mod state;
pub mod ui;

use axum::Router;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use state::AppState;

/// Largest capture the server accepts.
///
/// Inherited from axum's 2 MB default until now, which was a number nobody
/// picked. Sized for a long chapter of prose with headroom, and small enough
/// that a runaway upload is refused rather than buffered.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A browser landing on a page it has no session for should end up in front
/// of the identity provider, not staring at `{"error":"unauthorized"}` with
/// no way to act on it. That includes a path nothing handles: no route in
/// this router answers `/`, so an unauthenticated visit to the bare domain
/// falls through to the same rejection as an expired-session page load, and
/// both should end the same way.
///
/// API tokens and the MCP endpoint need the real status code — a script
/// cannot follow a redirect into an interactive OIDC login — so this only
/// rewrites a plain page load: a GET outside `/api` and `/mcp`, and not an
/// htmx fetch, which always carries `HX-Request` and expects a fragment or a
/// real error, never a full-page redirect.
async fn redirect_unauthenticated_browsers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_page_load = req.method() == Method::GET
        && !req.headers().contains_key("hx-request")
        && !path.starts_with("/api/")
        && path != "/mcp";

    let res = next.run(req).await;
    if is_page_load && res.status() == StatusCode::UNAUTHORIZED {
        return Redirect::to("/auth/login?go=1").into_response();
    }
    res
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(assets::assets_router())
        .merge(auth_routes::auth_router())
        .merge(ui::ui_router())
        .merge(pair::pair_router())
        .merge(extension::extension_router())
        .merge(judge::judge_router())
        .merge(crate::mcp::mcp_router(state.clone()))
        .nest(
            "/api/v1",
            api::api_router(state.core.capture.image_max_bytes),
        )
        .layer(axum::middleware::from_fn(redirect_unauthenticated_browsers))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_limit_is_deliberate_and_large_enough_for_a_chapter() {
        // axum's default is 2 MB and was never chosen. A long chapter of prose
        // is well under this; a book-sized paste is refused with a message.
        assert_eq!(MAX_BODY_BYTES, 8 * 1024 * 1024);
    }
}
