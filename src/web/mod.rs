#[cfg(test)]
mod android_fixtures;
pub mod api;
pub mod app;
pub mod artifact;
pub mod assets;
pub mod auth_routes;
pub mod client;
pub mod corpus;
pub mod corpus_view;
pub mod day;
pub mod decide;
pub mod due;
pub mod etag;
pub mod extension;
pub mod icons;
pub mod insights;
pub mod judge;
pub mod lineage_view;
pub mod live;
pub mod markdown;
pub mod notice;
pub mod ops;
pub mod page;
pub mod pair;
pub mod prefs;
pub mod push;
pub mod security;
pub mod settings;
pub mod state;
pub mod tenant;
#[cfg(test)]
pub(crate) mod test_support;
pub mod trash;
pub mod ui;
pub mod ui_error;
pub mod vbg;
pub mod workspace;

use axum::Router;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use state::AppState;

/// Sized for a long chapter of prose; a runaway upload is refused rather than buffered.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// `--theme-color` from 00-tokens.css, for the one place a token cannot reach: a meta tag.
pub const THEME_LIGHT: &str = "#f8f6f1";
pub const THEME_DARK: &str = "#0e1015";

pub const API_PER_MINUTE: u32 = 600;
pub const API_BURST: u32 = 120;

/// Rewrites a 401 on a plain page load into the login redirect, and nothing else.
/// Not under `/api` or `/mcp`, and not an htmx fetch: those need the real status.
async fn redirect_unauthenticated_browsers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_page_load = req.method() == Method::GET
        && !req.headers().contains_key("hx-request")
        && !path.starts_with("/api/")
        && path != "/mcp";

    // Kept before the request is consumed; `auth_routes::safe_next` decides whether it is followed.
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

/// `Vary: HX-Request`: one URL answers page or fragment, and Back may reuse a cached fragment as a page.
/// `private, no-store`: each page is one person's base. Only `text/html`, never over a handler's own `Cache-Control`.
async fn declare_html_uncacheable(req: Request, next: Next) -> Response {
    use axum::http::header;
    let mut res = next.run(req).await;
    let html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if !html {
        return res;
    }
    let h = res.headers_mut();
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("private, no-store"),
        );
    }
    // Appended, so a handler's own `Vary` is kept.
    h.append(header::VARY, header::HeaderValue::from_static("HX-Request"));
    res
}

pub fn router(state: AppState) -> Router {
    let sessions = crate::auth::session_layer(state.tenants.control(), &state.auth.deployment);
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(assets::assets_router())
        .merge(auth_routes::auth_router())
        .merge(workspace::routes())
        .merge(decide::routes())
        .merge(ui::ui_router())
        .merge(settings::routes())
        .merge(corpus::routes())
        .merge(artifact::routes())
        .merge(trash::routes())
        .merge(prefs::routes())
        .route("/ui/live", get(live::live))
        .merge(ops::routes())
        .merge(pair::pair_router())
        .merge(extension::extension_router())
        .merge(app::app_router())
        .merge(insights::routes())
        .merge(due::routes())
        .merge(day::routes())
        .merge(crate::mcp::mcp_router(state.clone()))
        .nest(
            "/api/v1",
            api::api_router(
                state.config.capture.image_max_bytes,
                state.config.capture.pdf_max_bytes,
            )
            .layer(security::rate_limit(API_PER_MINUTE, API_BURST)),
        )
        .fallback(ui::not_found)
        .layer(axum::middleware::from_fn(redirect_unauthenticated_browsers))
        .layer(axum::middleware::from_fn(declare_html_uncacheable))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            prefs::render_prefs,
        ))
        .layer(sessions)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(
            tower_http::sensitive_headers::SetSensitiveResponseHeadersLayer::new([
                axum::http::header::SET_COOKIE,
            ]),
        )
        .layer(
            tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer::new([
                axum::http::header::AUTHORIZATION,
                axum::http::header::COOKIE,
            ]),
        )
        .with_state(state)
}

pub fn app(state: AppState, dep: &security::Deployment) -> Router {
    security::secure(router(state), dep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn a_page_that_answers_two_ways_says_what_it_answers_to() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha bravo charlie", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        let id = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let ask = |hx: bool| {
            let mut r = axum::http::Request::builder()
                .uri(format!("/ui/artifacts/{id}"))
                .header("cookie", &cookie);
            if hx {
                r = r.header("hx-request", "true");
            }
            app.clone().oneshot(r.body(Body::empty()).unwrap())
        };

        for hx in [false, true] {
            let res = ask(hx).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{res:?}");
            let h = res.headers();
            assert!(
                h.get_all("vary")
                    .iter()
                    .any(|v| v.to_str().unwrap_or("").eq_ignore_ascii_case("HX-Request")),
                "hx={hx}: the header the body depends on has to be in the cache key: {h:?}"
            );
            assert_eq!(h["cache-control"], "private, no-store", "hx={hx}");
        }

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/assets/app.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.headers()["cache-control"]
                .to_str()
                .unwrap()
                .contains("max-age"),
            "{:?}",
            res.headers()
        );
    }

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
        assert_eq!(res.status(), StatusCode::OK, "{res:?}");

        let res = app.oneshot(get(None)).await.unwrap();
        assert_ne!(res.status(), StatusCode::NOT_FOUND, "{res:?}");
    }

    #[tokio::test]
    async fn housekeeping_is_one_name_and_one_url() {
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
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{res:?}");
        assert_eq!(res.headers()["location"], "/ui/insights");
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
        // The fallback is the whole application's, so an unrouted stylesheet must be a 404, not a login redirect.
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
        // An API caller must not be handed a login-shaped HTML document.
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
        let core = crate::core::test_support::test_core().await;
        let app = crate::web::test_support::router(core, None).await;
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
        let app = crate::web::test_support::router(core, None).await;
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

    #[tokio::test]
    async fn authorization_and_cookie_headers_are_marked_sensitive() {
        let inner = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|h: axum::http::HeaderMap| async move {
                    format!(
                        "{} {}",
                        h["authorization"].is_sensitive(),
                        h["cookie"].is_sensitive()
                    )
                }),
            )
            .layer(
                tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer::new([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::COOKIE,
                ]),
            );
        let res = inner
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("authorization", "Bearer x")
                    .header("cookie", "a=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 100).await.unwrap();
        assert_eq!(&body[..], b"true true");
        let src = include_str!("mod.rs");
        let code = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        assert!(code.contains("SetSensitiveRequestHeadersLayer::new(["));
        assert!(code.contains("SetSensitiveResponseHeadersLayer::new(["));
    }

    #[test]
    fn the_body_limit_is_deliberate_and_large_enough_for_a_chapter() {
        assert_eq!(MAX_BODY_BYTES, 8 * 1024 * 1024);
    }

    #[tokio::test]
    #[ignore = "needs node and a headless Chrome; see web::test_support"]
    async fn no_page_runs_off_the_side_of_a_phone() {
        let pages = crate::web::test_support::every_page().await;
        for width in ["390", "700", "1280", "1500", "2560"] {
            let run = crate::web::test_support::measure(&pages, width);
            crate::web::test_support::nothing_is_broken(&run, width);
        }
    }

    #[tokio::test]
    async fn the_retired_share_door_answers_nothing() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ui/share")
                    .method("POST")
                    .header("cookie", &cookie)
                    .header("content-type", "multipart/form-data; boundary=b")
                    .body(Body::from(
                        "--b\r\nContent-Disposition: form-data; name=\"text\"\r\n\r\nx\r\n--b--\r\n",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                res.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "POST /ui/share answered {}",
            res.status()
        );
    }
}
