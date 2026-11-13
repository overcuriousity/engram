use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{FromFnLayer, Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};

use super::ClientIp;

const MAX_TRACKED: usize = 10_000;

#[derive(Clone)]
pub struct Buckets {
    per_second: f64,
    burst: f64,
    state: Arc<Mutex<HashMap<IpAddr, (f64, Instant)>>>,
}

/// One IPv6 client usually holds a whole /64.
fn bucket_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !(u128::MAX >> 64)).into()),
        v4 => v4,
    }
}

impl Buckets {
    /// `Ok` to proceed, or the seconds until the next request would be.
    fn take(&self, ip: IpAddr) -> Result<(), u64> {
        let mut map = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if map.len() > MAX_TRACKED {
            let (rate, burst) = (self.per_second, self.burst);
            map.retain(|_, (tokens, t)| {
                *tokens + now.duration_since(*t).as_secs_f64() * rate < burst
            });
            if map.len() > MAX_TRACKED * 10 {
                map.clear();
            }
        }
        let (tokens, last) = map.entry(bucket_key(ip)).or_insert((self.burst, now));
        *tokens =
            (*tokens + now.duration_since(*last).as_secs_f64() * self.per_second).min(self.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) / self.per_second).ceil().max(1.0) as u64)
        }
    }
}

async fn limit(State(b): State<Buckets>, req: Request, next: Next) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    if let Some(Err(wait)) = ip.map(|ip| b.take(ip)) {
        let mut res = (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts; wait a minute and try again.",
        )
            .into_response();
        if let Ok(v) = HeaderValue::from_str(&wait.to_string()) {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return res;
    }
    next.run(req).await
}

pub type RateLayer = FromFnLayer<
    fn(
        State<Buckets>,
        Request,
        Next,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
    Buckets,
    (State<Buckets>, Request),
>;

/// A per-client token bucket: `per_minute` sustained, `burst` at once.
pub fn rate_limit(per_minute: u32, burst: u32) -> RateLayer {
    let b = Buckets {
        per_second: f64::from(per_minute) / 60.0,
        burst: f64::from(burst),
        state: Arc::default(),
    };
    from_fn_with_state(b, |s, r, n| Box::pin(limit(s, r, n)))
}
