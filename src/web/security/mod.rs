mod client_ip;
mod headers;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use ipnet::IpNet;
use url::Url;

pub use client_ip::ClientIp;
pub use headers::CSP;

const LOOPBACK_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Where this instance is reached from, as the security layer needs to know it.
#[derive(Debug, Clone)]
pub struct Deployment {
    pub public_url: Option<Url>,
    pub trusted_proxies: Vec<IpNet>,
    pub allow_plain_http: bool,
    pub bind: SocketAddr,
}

pub fn default_trusted_proxies() -> Vec<IpNet> {
    ["127.0.0.1/32", "::1/128"]
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl Deployment {
    /// Reached only on loopback, with no public address.
    pub fn loopback(bind: SocketAddr) -> Self {
        Self {
            public_url: None,
            trusted_proxies: default_trusted_proxies(),
            allow_plain_http: false,
            bind,
        }
    }

    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match &self.public_url {
            None => {
                let port = self.bind.port();
                LOOPBACK_NAMES
                    .iter()
                    .any(|n| host == format!("{n}:{port}") || (port == 80 && host == *n))
            }
            Some(u) => {
                let expected = u.host_str().unwrap_or_default();
                match u.port() {
                    Some(p) => host == format!("{expected}:{p}"),
                    None => host == expected,
                }
            }
        }
    }

    pub fn origin_allowed(&self, origin: &str) -> bool {
        match &self.public_url {
            None => {
                let port = self.bind.port();
                LOOPBACK_NAMES
                    .iter()
                    .any(|n| origin == format!("http://{n}:{port}"))
            }
            Some(u) => origin == u.origin().ascii_serialization(),
        }
    }

    pub fn secure_cookies(&self) -> bool {
        self.public_url
            .as_ref()
            .is_some_and(|u| u.scheme() == "https")
    }

    /// Served over http to something other than this machine, where browsers send no fetch metadata.
    pub fn plain_http_remote(&self) -> bool {
        self.public_url.as_ref().is_some_and(|u| {
            u.scheme() == "http" && !is_loopback_host(u.host_str().unwrap_or_default())
        })
    }

    /// The origin clients reach, without a trailing slash: the public URL's, or the loopback bind's.
    pub fn base(&self) -> String {
        if let Some(u) = &self.public_url {
            return u.origin().ascii_serialization();
        }
        let host = match self.bind.ip() {
            IpAddr::V6(_) => "[::1]".to_string(),
            IpAddr::V4(v4) if v4.is_unspecified() => "127.0.0.1".to_string(),
            IpAddr::V4(v4) => v4.to_string(),
        };
        format!("http://{host}:{}", self.bind.port())
    }

    /// `host[:port]` as clients address this instance.
    pub fn authority(&self) -> String {
        let base = self.base();
        base.split_once("://")
            .map_or(base.clone(), |(_, a)| a.to_string())
    }
}

pub(crate) struct Ctx {
    pub dep: Deployment,
}

/// Wraps the whole app: response headers and the host check.
pub fn secure(router: Router, dep: &Deployment) -> Router {
    let ctx = Arc::new(Ctx { dep: dep.clone() });
    router
        .layer(from_fn_with_state(ctx.clone(), client_ip::host_check))
        .layer(from_fn_with_state(ctx, headers::security_headers))
}
