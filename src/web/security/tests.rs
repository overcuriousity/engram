use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::Html;
use axum::routing::{get, post};
use tower::ServiceExt;

use super::*;

fn dep(public_url: Option<&str>) -> Deployment {
    Deployment {
        public_url: public_url.map(|u| u.parse().unwrap()),
        allow_plain_http: public_url.is_some_and(|u| u.starts_with("http://")),
        ..Deployment::loopback("127.0.0.1:8080".parse().unwrap())
    }
}

fn app_for(d: &Deployment, peer: [u8; 4]) -> Router {
    let router = Router::new()
        .route("/", get(|| async { Html("<p>hi</p>") }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/write", post(|_b: String| async { "ok" }))
        .route(
            "/ip",
            get(|ClientIp(ip): ClientIp| async move { ip.to_string() }),
        );
    secure(router, d).layer(MockConnectInfo(SocketAddr::from((peer, 4000))))
}

fn local() -> Router {
    app_for(&dep(None), [127, 0, 0, 1])
}

fn get_req(host: &str, uri: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

async fn body(res: axum::response::Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(res.into_body(), 1000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn every_html_response_carries_the_exact_csp() {
    let res = local()
        .oneshot(get_req("localhost:8080", "/"))
        .await
        .unwrap();
    assert_eq!(res.headers()["content-security-policy"], CSP);
    assert!(CSP.contains("default-src 'none'") && !CSP.contains("unsafe"));
    let plain = local()
        .oneshot(get_req("localhost:8080", "/healthz"))
        .await
        .unwrap();
    assert!(plain.headers().get("content-security-policy").is_none());
}

#[tokio::test]
async fn nosniff_referrer_policy_and_coop_are_always_set() {
    for uri in ["/", "/healthz", "/missing"] {
        let res = local()
            .oneshot(get_req("localhost:8080", uri))
            .await
            .unwrap();
        assert_eq!(res.headers()["x-content-type-options"], "nosniff", "{uri}");
        assert_eq!(res.headers()["referrer-policy"], "no-referrer", "{uri}");
        assert_eq!(
            res.headers()["cross-origin-opener-policy"],
            "same-origin",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn hsts_is_sent_only_when_public_url_is_https() {
    let https = app_for(&dep(Some("https://engram.example")), [127, 0, 0, 1])
        .oneshot(get_req("engram.example", "/"))
        .await
        .unwrap();
    assert!(https.headers().contains_key("strict-transport-security"));
    let plain = app_for(&dep(Some("http://engram.lan")), [127, 0, 0, 1])
        .oneshot(get_req("engram.lan", "/"))
        .await
        .unwrap();
    assert!(!plain.headers().contains_key("strict-transport-security"));
    let loopback = local()
        .oneshot(get_req("localhost:8080", "/"))
        .await
        .unwrap();
    assert!(!loopback.headers().contains_key("strict-transport-security"));
}

#[tokio::test]
async fn referrer_policy_is_same_origin_over_plain_http() {
    let res = app_for(&dep(Some("http://engram.lan")), [127, 0, 0, 1])
        .oneshot(get_req("engram.lan", "/"))
        .await
        .unwrap();
    assert_eq!(res.headers()["referrer-policy"], "same-origin");
}

#[tokio::test]
async fn a_foreign_host_header_gets_421() {
    let res = local().oneshot(get_req("evil.example", "/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    let public = app_for(&dep(Some("https://engram.example")), [127, 0, 0, 1]);
    let res = public
        .clone()
        .oneshot(get_req("localhost:8080", "/"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);
    let res = public
        .oneshot(get_req("evil.example", "/healthz"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "/healthz is exempt");
}

#[tokio::test]
async fn loopback_hosts_pass_without_a_public_url() {
    for host in ["localhost:8080", "127.0.0.1:8080", "[::1]:8080"] {
        let res = local().oneshot(get_req(host, "/")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{host}");
    }
    let res = local()
        .oneshot(get_req("localhost:9999", "/"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn x_forwarded_host_is_ignored() {
    let r = Request::get("/")
        .header(header::HOST, "evil.example")
        .header("x-forwarded-host", "localhost:8080")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        local().oneshot(r).await.unwrap().status(),
        StatusCode::MISDIRECTED_REQUEST
    );
}

#[tokio::test]
async fn x_forwarded_for_counts_only_from_a_trusted_proxy() {
    let r = || {
        Request::get("/ip")
            .header(header::HOST, "localhost:8080")
            .header("x-forwarded-for", "203.0.113.9")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        body(local().oneshot(r()).await.unwrap()).await,
        "203.0.113.9"
    );
    let direct = app_for(&dep(None), [10, 0, 0, 5]);
    assert_eq!(body(direct.oneshot(r()).await.unwrap()).await, "10.0.0.5");
}

#[test]
fn base_is_the_public_origin_or_the_loopback_bind() {
    assert_eq!(
        dep(Some("https://engram.example/")).base(),
        "https://engram.example"
    );
    assert_eq!(dep(None).base(), "http://127.0.0.1:8080");
    assert_eq!(dep(None).authority(), "127.0.0.1:8080");
}
