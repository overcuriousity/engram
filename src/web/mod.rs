pub mod api;
pub mod markdown;
pub mod state;

use axum::Router;
use axum::routing::get;
use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api/v1", api::api_router())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
