pub mod api;
pub mod assets;
pub mod auth_routes;
pub mod corpus_view;
pub mod extension;
pub mod judge;
pub mod lineage_view;
pub mod markdown;
pub mod pair;
pub mod state;
#[cfg(test)]
pub(crate) mod test_support;
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
/// no way to act on it.
///
/// This rewrites a 401 and nothing else. An unmatched path is a 404 and stays
/// one — which is why the bare domain needs a route of its own rather than
/// relying on this: a request that matches nothing never reaches the
/// authentication that would have produced the 401 to rewrite. See `/` in
/// `ui::ui_router`.
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

    // Kept before the request is consumed: after the login this is the page to
    // come back to, and losing it means every bounced deep link lands on
    // Search instead. `auth_routes::safe_next` decides whether it is followed.
    let went_to: String = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(&path)
        .to_string();

    let res = next.run(req).await;
    if is_page_load && res.status() == StatusCode::UNAUTHORIZED {
        let go: String = url::form_urlencoded::byte_serialize(went_to.as_bytes()).collect();
        return Redirect::to(&format!("/auth/login?go={go}")).into_response();
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
            api::api_router(
                state.core.capture.image_max_bytes,
                state.core.capture.pdf_max_bytes,
            ),
        )
        .fallback(ui::not_found)
        .layer(axum::middleware::from_fn(redirect_unauthenticated_browsers))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_bare_domain_is_a_door_into_the_ui_and_not_a_404() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let get = |c: Option<&str>| {
            let mut r = axum::http::Request::builder().uri("/");
            if let Some(c) = c {
                r = r.header("cookie", c);
            }
            r.body(Body::empty()).unwrap()
        };

        let res = app.clone().oneshot(get(Some(&cookie))).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        assert_eq!(res.headers()["location"], "/ui/search");

        // And with no session at all: `/` is still a route, so the rejection
        // it produces is a 401 the middleware can turn into a login — which is
        // what an unmatched path never was.
        let res = app.oneshot(get(None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        assert_eq!(res.headers()["location"], "/ui/search");
    }

    #[tokio::test]
    async fn housekeeping_is_one_name_and_one_url() {
        // The nav says Housekeeping, the URL says /ui/ops, the page title said
        // Ops — and /ui/housekeeping, the name a reader would type, was the
        // browser's own error page.
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/housekeeping")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT, "{res:?}");
        assert_eq!(res.headers()["location"], "/ui/ops");
    }

    #[tokio::test]
    async fn an_unknown_ui_path_gets_the_apps_own_page() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/nothing-here")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = crate::web::test_support::body_of(res).await;
        assert!(
            body.contains("engram"),
            "the browser's error page, not ours: {body}"
        );
    }

    #[tokio::test]
    async fn a_missing_asset_is_a_missing_asset_and_not_a_login() {
        // The fallback is the whole application's, not the UI router's, so a
        // stylesheet nobody routed arrived at the page — which for a browser
        // with no session is a 401, which the redirect middleware then turns
        // into the login screen. A 303 to `/auth/login` in place of a 404 on a
        // `<link>` is not a missing stylesheet, it is a mystery.
        let core = crate::core::test_support::test_core().await;
        let (app, _cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/assets/nothing-here.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{res:?}");
    }

    #[tokio::test]
    async fn an_unrouted_mcp_path_is_not_a_web_page_either() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/mcp/nothing-here")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = crate::web::test_support::body_of(res).await;
        assert!(!body.contains("<html"), "an HTML document to parse: {body}");
    }

    #[tokio::test]
    async fn a_post_to_a_path_nobody_routed_is_not_handed_a_page() {
        // Nobody types a POST into a browser bar, so there is nobody to show a
        // page to; whatever sent it wants the status.
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/ui/nothing-here")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = crate::web::test_support::body_of(res).await;
        assert!(!body.contains("<html"), "an HTML document to parse: {body}");
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_still_not_a_web_page() {
        // The fallback is for people typing URLs. An agent asking the API for
        // a route that does not exist must not be handed a login-shaped HTML
        // document to parse.
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/nothing-here")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = crate::web::test_support::body_of(res).await;
        assert!(
            !body.contains("<html"),
            "an API 404 came back as a page: {body}"
        );
    }

    #[tokio::test]
    async fn an_unknown_path_is_behind_a_session_like_every_other_page() {
        // The fallback took no `Identity`, so the one path nobody had routed
        // was the one path that rendered the whole nav — `judge_pending`, a
        // live count out of the base, included — to a visitor with no session.
        let core = crate::core::test_support::test_core().await;
        let app = crate::web::test_support::router(core, None);
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/nothing-here")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        assert!(
            res.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with("/auth/login"),
            "an unknown path rendered rather than bouncing: {res:?}"
        );

        // And the API answer is unchanged: a caller with no credentials asking
        // for a route that does not exist is told the route does not exist,
        // not sent to an interactive login it cannot follow.
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/nothing-here")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{res:?}");
    }

    #[tokio::test]
    async fn a_bounced_page_load_tells_the_login_where_it_was_going() {
        let core = crate::core::test_support::test_core().await;
        let app = crate::web::test_support::router(core, None);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/corpora/abc?terms=x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        assert_eq!(
            res.headers()["location"],
            "/auth/login?go=%2Fui%2Fcorpora%2Fabc%3Fterms%3Dx"
        );
    }

    #[tokio::test]
    async fn a_page_nothing_serves_is_still_a_404() {
        // The bare domain gets a door; a typo does not get a redirect that
        // pretends the page exists.
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/nothing-here")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{res:?}");
    }

    #[test]
    fn the_body_limit_is_deliberate_and_large_enough_for_a_chapter() {
        // axum's default is 2 MB and was never chosen. A long chapter of prose
        // is well under this; a book-sized paste is refused with a message.
        assert_eq!(MAX_BODY_BYTES, 8 * 1024 * 1024);
    }
}
