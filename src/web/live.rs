//! Live updates over server-sent events, one channel per tenant.

use std::convert::Infallible;
use std::time::Duration;

use axum::http::{HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[derive(Clone)]
pub struct Broadcaster<T: Clone + Send + 'static> {
    tx: broadcast::Sender<T>,
}

impl<T: Clone + Send + 'static> Broadcaster<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::channel(capacity).0,
        }
    }

    pub fn send(&self, v: T) {
        let _ = self.tx.send(v);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<T> {
        self.tx.subscribe()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveEvent {
    Queue,
    Idle,
}

impl LiveEvent {
    pub fn name(self) -> &'static str {
        match self {
            LiveEvent::Queue => "queue",
            LiveEvent::Idle => "idle",
        }
    }
}

pub type Live = Broadcaster<LiveEvent>;

impl Default for Live {
    fn default() -> Self {
        Broadcaster::new(64)
    }
}

impl Live {
    /// A unit of work finished or arrived: both fragments may have moved.
    pub fn changed(&self) {
        self.send(LiveEvent::Queue);
        self.send(LiveEvent::Idle);
    }
}

/// A client that falls behind gets `resync`, so it refetches instead of missing a change.
pub fn sse_response(rx: broadcast::Receiver<LiveEvent>) -> Response {
    let stream = BroadcastStream::new(rx).map(|item| {
        let name = match item {
            Ok(e) => e.name(),
            Err(BroadcastStreamRecvError::Lagged(_)) => "resync",
        };
        // EventSource drops an event with no data line.
        Ok::<_, Infallible>(Event::default().event(name).data("-"))
    });
    let mut res = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    res.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

pub async fn live(tenant: crate::tenants::Tenant) -> Response {
    sse_response(tenant.core.live.subscribe())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn first_frame(res: Response) -> String {
        let mut s = res.into_body().into_data_stream();
        while let Some(chunk) = s.next().await {
            let text = String::from_utf8_lossy(&chunk.unwrap()).into_owned();
            if text.contains("event:") {
                return text;
            }
        }
        String::new()
    }

    #[tokio::test]
    async fn a_queue_change_reaches_a_subscriber_as_an_event_with_a_data_line() {
        let live = Live::default();
        let res = sse_response(live.subscribe());
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(res.headers()["x-accel-buffering"], "no");
        live.send(LiveEvent::Queue);
        let frame = first_frame(res).await;
        assert!(frame.contains("event: queue"), "{frame}");
        assert!(frame.contains("data: -"), "{frame}");
    }

    #[tokio::test]
    async fn one_tenant_never_sees_anothers_events() {
        let a = crate::core::test_support::test_core().await;
        let b = crate::core::test_support::test_core().await;
        let mut rx = b.live.subscribe();
        a.live.changed();
        assert!(rx.try_recv().is_err());
        b.live.send(LiveEvent::Idle);
        assert_eq!(rx.try_recv().unwrap(), LiveEvent::Idle);
    }
}
