//! One pacer in front of every inference call.
//!
//! Three jobs that all answer the same question — may a background call start
//! now? The cooldown protects a desktop GPU from unbroken load. The interactive
//! lease keeps the worker from piling work onto the endpoint while a person is
//! waiting on `ask`. The breaker stops thirty-four units from each spending a
//! fifteen-minute timeout discovering the same dead endpoint.
//!
//! It sits around calls rather than around jobs, so the two stages that make no
//! inference call — planning a corpus into windows, and the consolidation sweep
//! — never wait for a cooldown they did not earn.

use crate::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

#[derive(Default)]
struct GateState {
    /// Interactive calls in flight. Background work waits while this is above
    /// zero; a count rather than a flag because `ask` makes more than one call
    /// and the UI can have two queries open.
    interactive: usize,
    last_finished: Option<Instant>,
    consecutive_transport_failures: usize,
    breaker_open_until: Option<Instant>,
}

pub struct InferenceGate {
    state: Mutex<GateState>,
    resumed: Notify,
    cooldown: Duration,
    breaker_after: usize,
    probe_after: Duration,
}

impl InferenceGate {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            resumed: Notify::new(),
            cooldown,
            // Off unless configured, so a test that did not ask for a breaker
            // cannot be surprised by one.
            breaker_after: usize::MAX,
            probe_after: Duration::ZERO,
        }
    }

    pub fn with_breaker(mut self, after: usize, probe: Duration) -> Self {
        self.breaker_after = after.max(1);
        self.probe_after = probe;
        self
    }

    /// Returns when a background inference call may start.
    pub async fn background(&self) {
        loop {
            // The lock is never held across an await: the wait is computed, the
            // guard dropped, and only then slept on.
            let wait = {
                let st = self.state.lock().expect("gate state");
                if st.interactive > 0 {
                    None
                } else {
                    let now = Instant::now();
                    let ready_at = [
                        st.last_finished.map(|t| t + self.cooldown),
                        st.breaker_open_until,
                    ]
                    .into_iter()
                    .flatten()
                    .max();
                    match ready_at {
                        Some(t) if t > now => Some(t - now),
                        _ => return,
                    }
                }
            };
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                // Held off by an interactive call, which has no deadline. The
                // lease wakes us when it drops.
                None => self.resumed.notified().await,
            }
        }
    }

    /// A lease for an interactive call. Returns immediately, always.
    pub fn interactive(self: &Arc<Self>) -> InteractiveLease {
        self.state.lock().expect("gate state").interactive += 1;
        InteractiveLease {
            gate: Arc::clone(self),
        }
    }

    pub fn call_succeeded(&self) {
        let mut st = self.state.lock().expect("gate state");
        st.last_finished = Some(Instant::now());
        st.consecutive_transport_failures = 0;
        st.breaker_open_until = None;
    }

    /// A failed call still occupied the GPU, so it still starts the cooldown.
    /// Only a transport failure counts toward the breaker: `MalformedLlmOutput`
    /// means the endpoint answered and this window's text is the problem.
    pub fn call_failed(&self, e: &Error) {
        let mut st = self.state.lock().expect("gate state");
        st.last_finished = Some(Instant::now());
        if !matches!(e, Error::Inference { .. }) {
            return;
        }
        st.consecutive_transport_failures += 1;
        if st.consecutive_transport_failures >= self.breaker_after {
            st.breaker_open_until = Some(Instant::now() + self.probe_after);
            // Reset, so one further failure after the probe re-opens it rather
            // than every subsequent call re-arming from an already-tripped count.
            st.consecutive_transport_failures = 0;
            tracing::warn!(
                probe_secs = self.probe_after.as_secs(),
                "inference endpoint failed repeatedly; holding background calls"
            );
        }
    }
}

pub struct InteractiveLease {
    gate: Arc<InferenceGate>,
}

impl Drop for InteractiveLease {
    fn drop(&mut self) {
        {
            let mut st = self.gate.state.lock().expect("gate state");
            st.interactive = st.interactive.saturating_sub(1);
            st.last_finished = Some(Instant::now());
        }
        self.gate.resumed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn gate(cooldown_secs: u64) -> Arc<InferenceGate> {
        Arc::new(InferenceGate::new(Duration::from_secs(cooldown_secs)))
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_gate_with_no_cooldown_lets_work_through_at_once() {
        let g = gate(0);
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cooldown_is_measured_from_when_the_last_call_ended() {
        let g = gate(5);
        g.call_succeeded();
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn background_work_waits_while_an_interactive_call_holds_a_lease() {
        let g = gate(0);
        let lease = g.interactive();
        let g2 = Arc::clone(&g);
        let waiter = tokio::spawn(async move { g2.background().await });

        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            !waiter.is_finished(),
            "background ran while an ask was in flight"
        );

        drop(lease);
        waiter.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_interactive_call_never_waits_even_during_a_cooldown() {
        // The point of the lane: a person is waiting, and the pacer exists to
        // protect the GPU from batch work, not from them.
        let g = gate(600);
        g.call_succeeded();
        let started = tokio::time::Instant::now();
        let _lease = g.interactive();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn three_transport_failures_in_a_row_open_the_breaker() {
        let g =
            Arc::new(InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)));
        for _ in 0..3 {
            g.call_failed(&Error::Inference {
                role: "chunk",
                detail: "502".into(),
            });
        }
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_closes_the_breaker_again() {
        let g =
            Arc::new(InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)));
        for _ in 0..2 {
            g.call_failed(&Error::Inference {
                role: "chunk",
                detail: "502".into(),
            });
        }
        g.call_succeeded();
        g.call_failed(&Error::Inference {
            role: "chunk",
            detail: "502".into(),
        });

        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO, "the run was not reset");
    }

    #[tokio::test(start_paused = true)]
    async fn unreadable_output_is_not_an_endpoint_failure() {
        // The distinction the whole design rests on: the model answered, so the
        // endpoint is fine. Tripping the breaker here would stop the queue over
        // one document's punctuation.
        let g =
            Arc::new(InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)));
        for _ in 0..5 {
            g.call_failed(&Error::MalformedLlmOutput("duplicate field `tags`".into()));
        }
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
