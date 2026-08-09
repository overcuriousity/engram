pub mod api;
pub mod assets;
pub mod auth_routes;
pub mod markdown;
pub mod state;
pub mod ui;

use axum::Router;
use axum::routing::get;
use state::AppState;

/// Largest capture the server accepts.
///
/// Inherited from axum's 2 MB default until now, which was a number nobody
/// picked. Sized for a long chapter of prose with headroom, and small enough
/// that a runaway upload is refused rather than buffered.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(assets::assets_router())
        .merge(auth_routes::auth_router())
        .merge(ui::ui_router())
        .merge(crate::mcp::mcp_router(state.clone()))
        .nest("/api/v1", api::api_router())
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
