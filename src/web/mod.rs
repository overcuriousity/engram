pub mod api;
pub mod assets;
pub mod auth_routes;
pub mod corpus_view;
pub mod day;
pub mod due;
pub mod extension;
pub mod insights;
pub mod lineage_view;
pub mod markdown;
pub mod pair;
pub mod share;
pub mod state;
pub mod tenant;
#[cfg(test)]
pub(crate) mod test_support;
pub mod ui;
pub mod vbg;
pub mod workspace;

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
///
/// `POST /ui/share` is the one exception to the GET rule, and it is here
/// rather than in the handler because this is where the rewrite lives. A
/// share is dispatched by the platform, not by a page of ours, so it is the
/// one POST that can arrive without the `SameSite=Lax` session cookie ever
/// having been sent — and the reader is a person staring at a share sheet's
/// webview, for whom `{"error":"unauthorized"}` is the end of the road and
/// the shared content is gone. `303` rather than `307`, so the browser
/// follows it as a GET onto the login page; the share itself is not replayed
/// afterwards, which is the honest outcome — it was never stored.
async fn redirect_unauthenticated_browsers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_share = req.method() == Method::POST && path == "/ui/share";
    let is_page_load = (req.method() == Method::GET || is_share)
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

/// Say what an HTML response depends on, and that it is nobody's to keep.
///
/// Half the UI's routes answer one URL with two different bodies, chosen by a
/// request header: `/ui/artifacts/{id}` returns the standalone page to a
/// browser and the bare fragment to htmx, and several others do the same. The
/// rail's links carry `hx-push-url="true"`, so that URL is in the address bar
/// having only ever been fetched as a fragment — and nothing said so. A
/// history navigation is precisely where a browser may reuse a stored response
/// without revalidating, so pressing Back rendered the fragment as a whole
/// document: no `<head>`, no stylesheet, the page in Times New Roman with the
/// icons at their intrinsic size. `Vary` is what tells a cache that the header
/// is part of the key.
///
/// `no-store` beside it, because the negotiation is not the only reason. Every
/// one of these pages is one person's own base rendered into HTML, and without
/// this it sits in the disk cache under their profile after they sign out.
/// `private` says the same thing to anything in between.
///
/// `text/html` only, and never over a `Cache-Control` a handler set for itself:
/// the assets router serves the stylesheet and the tokenizer under a year-long
/// `max-age`, and that is the whole reason those files are cheap.
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
    // Appended rather than set: a handler that already varies on something of
    // its own keeps it.
    h.append(header::VARY, header::HeaderValue::from_static("HX-Request"));
    res
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(assets::assets_router())
        .merge(auth_routes::auth_router())
        .merge(workspace::routes())
        .merge(ui::ui_router())
        .merge(pair::pair_router())
        .merge(extension::extension_router())
        .merge(share::share_router(
            state.config.capture.image_max_bytes,
            state.config.capture.pdf_max_bytes,
        ))
        .merge(insights::routes())
        .merge(due::routes())
        .merge(day::routes())
        .merge(crate::mcp::mcp_router(state.clone()))
        .nest(
            "/api/v1",
            api::api_router(
                state.config.capture.image_max_bytes,
                state.config.capture.pdf_max_bytes,
            ),
        )
        .fallback(ui::not_found)
        .layer(axum::middleware::from_fn(redirect_unauthenticated_browsers))
        .layer(axum::middleware::from_fn(declare_html_uncacheable))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    /// The one URL that answers with two different documents, and what has to
    /// be said about it. Without `Vary` a browser is entitled to reuse the
    /// fragment htmx fetched — and history navigation is exactly where it does
    /// — so pressing Back after following a link out of the workspace rendered
    /// the bare partial as a whole page: no `<head>`, no stylesheet, the app in
    /// Times New Roman.
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
            // And nobody's to keep either way: this is one person's own base
            // rendered into HTML, and it outlived their session in the disk
            // cache.
            assert_eq!(h["cache-control"], "private, no-store", "hx={hx}");
        }

        // The stylesheet is not a page and keeps the year-long `max-age` that
        // is the whole reason it is cheap.
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

        // The bare domain is the workspace itself now, not a redirect to it:
        // there is one page, so there is nothing left to redirect to.
        let res = app.clone().oneshot(get(Some(&cookie))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{res:?}");

        // And with no session at all: `/` is still a route, so the rejection
        // it produces is one the middleware can turn into a login — which is
        // what an unmatched path never was.
        let res = app.oneshot(get(None)).await.unwrap();
        assert_ne!(res.status(), StatusCode::NOT_FOUND, "{res:?}");
    }

    #[tokio::test]
    async fn housekeeping_is_one_name_and_one_url() {
        // /ui/housekeeping is the name a reader was shown for a while, and it
        // goes straight to the page — one hop, not a chain through the
        // /ui/ops shim.
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
        // was the one path that rendered the whole nav to a visitor with no
        // session.
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
