use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ipnet::IpNet;

use super::Ctx;

/// The TCP peer, or the right-most untrusted `X-Forwarded-For` hop when the peer is a trusted proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

fn peer(parts: &Parts) -> Option<IpAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .or_else(|| {
            parts
                .extensions
                .get::<MockConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
        })
}

fn resolve(parts: &Parts, proxies: &[IpNet]) -> Option<IpAddr> {
    let peer = peer(parts)?;
    let trusted = |ip: &IpAddr| proxies.iter().any(|n| n.contains(ip));
    if !trusted(&peer) {
        return Some(peer);
    }
    let forwarded: Vec<IpAddr> = parts
        .headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    Some(
        forwarded
            .into_iter()
            .rev()
            .find(|ip| !trusted(ip))
            .unwrap_or(peer),
    )
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ClientIp>()
            .copied()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

pub(super) async fn host_check(State(ctx): State<Arc<Ctx>>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    // X-Forwarded-Host is ignored: the proxy passes the client's Host through unchanged.
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| parts.uri.authority().map(|a| a.as_str()));
    let exempt = parts.uri.path() == "/healthz";
    if !exempt && !host.is_some_and(|h| ctx.dep.host_allowed(h)) {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "This server does not answer for that host name.",
        )
            .into_response();
    }
    if let Some(ip) = resolve(&parts, &ctx.dep.trusted_proxies) {
        parts.extensions.insert(ClientIp(ip));
    }
    next.run(Request::from_parts(parts, body)).await
}
