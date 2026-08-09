pub mod api;
pub mod assets;
pub mod auth_routes;
pub mod markdown;
pub mod state;
pub mod ui;

use axum::Router;
use axum::routing::get;
use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(assets::assets_router())
        .merge(auth_routes::auth_router())
        .merge(ui::ui_router())
        .nest("/api/v1", api::api_router())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}
