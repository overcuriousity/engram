use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};

struct TrashItem {
    id: String,
    name: String,
    when: String,
}

#[derive(Template)]
#[template(path = "trash.html")]
struct TrashTemplate {
    items: Vec<TrashItem>,
}

impl TrashTemplate {
    fn section(&self) -> &'static str {
        "settings"
    }
}

async fn trash_page(tenant: Tenant) -> UiResult<Response> {
    let items = tenant
        .core
        .store
        .trash()
        .await?
        .into_iter()
        .map(|(a, at)| TrashItem {
            name: crate::web::notice::name_of(a.title.as_deref(), &a.text),
            when: crate::fmt::ago(at),
            id: a.id,
        })
        .collect();
    Ok(HtmlTemplate(TrashTemplate { items }).into_response())
}

/// Out of Trash. From an undo notice the page reloads to show it again; on the Trash page its row goes.
async fn restore(tenant: Tenant, headers: HeaderMap, Path(id): Path<String>) -> UiResult<Response> {
    tenant.core.restore_artifact(&id).await?;
    let on_trash_page = headers
        .get("hx-current-url")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|u| u.contains("/ui/trash"));
    if on_trash_page {
        return Ok(Html(String::new()).into_response());
    }
    Ok(([("hx-refresh", "true")], Html(String::new())).into_response())
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/trash", get(trash_page))
        .route("/ui/artifacts/{id}/restore", post(restore))
}
