pub mod api;
pub mod assets;
pub mod auth_routes;
pub mod corpus_view;
pub mod markdown;
pub mod state;
pub mod ui;

use axum::Router;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use state::AppState;

pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

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
        .merge(crate::mcp::mcp_router(state.clone()))
        .nest("/api/v1", api::api_router())
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
        assert_eq!(MAX_BODY_BYTES, 8 * 1024 * 1024);
    }
}
