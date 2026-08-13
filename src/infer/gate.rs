//! One pacer in front of every inference call.
//!
//! Four jobs that all answer the same question — may a background call start
//! now? The cooldown protects a desktop GPU from unbroken load. The turn keeps
//! that answer true for the whole system rather than per worker. The
//! interactive lease keeps the worker from piling work onto the endpoint while
//! a person is waiting on `ask`. The breaker stops thirty-four units from each
//! spending a fifteen-minute timeout discovering the same dead endpoint.
//!
//! It sits around calls rather than around jobs, so the two stages that make no
//! inference call — planning a corpus into windows, and the consolidation sweep
//! — never wait for a cooldown they did not earn.

use crate::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore, SemaphorePermit};
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
    /// One background call at a time, held for the duration of the call.
    ///
    /// The cooldown on its own paces each worker without bounding the whole:
    /// `server.workers` defaults to 2, and two workers read the same unchanged
    /// `last_finished` in the same instant — neither has finished — and put two
    /// generations on the one GPU. That is the load the cooldown is configured
    /// to prevent, so the gap has to be between calls rather than between one
    /// worker's calls.
    turn: Semaphore,
    cooldown: Duration,
    breaker_after: usize,
    probe_after: Duration,
}

impl InferenceGate {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            resumed: Notify::new(),
            turn: Semaphore::new(1),
            cooldown,
            // Off unless configured, so a test that did not ask for a breaker
            // cannot be surprised by one.
            breaker_after: usize::MAX,
            probe_after: Duration::ZERO,
        }
    }

    /// `after = 0` turns the breaker off, the way `cooldown_secs = 0` turns the
    /// cooldown off. Clamping it up to 1 instead would read the operator's
    /// "don't do this" as the most aggressive setting there is — one failed call
    /// holding every background call for the whole probe window.
    pub fn with_breaker(mut self, after: usize, probe: Duration) -> Self {
        self.breaker_after = if after == 0 { usize::MAX } else { after };
        self.probe_after = probe;
        self
    }

    /// Returns the right to make one background inference call, once one may
    /// start. Hold the permit for the duration of the call.
    pub async fn background(&self) -> BackgroundPermit<'_> {
        // The turn is taken before the wait rather than after it, so whoever
        // holds it re-reads the cooldown once the call ahead has finished.
        // Checking first and taking the turn afterwards would let every waiter
        // clear the same stale `last_finished` and then merely queue up.
        let turn = self
            .turn
            .acquire()
            .await
            .expect("the gate's turn is never closed");
        loop {
            // Built before the lock is taken and registered before it is
            // dropped. `notify_waiters` wakes only waiters that are already
            // registered and leaves no permit behind, so a lease ending in the
            // gap between dropping the guard and first polling this would have
            // woken nobody — and background work would have waited for the
            // *next* question instead of for this one to end, which on a
            // single-user base can be hours.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);

            // The lock is never held across an await: the wait is computed, the
            // guard dropped, and only then slept on.
            let wait = {
                let st = self.state.lock().expect("gate state");
                if st.interactive > 0 {
                    resumed.as_mut().enable();
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
                        _ => break,
                    }
                }
            };
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                // Held off by an interactive call, which has no deadline. The
                // lease wakes us when it drops.
                None => resumed.await,
            }
        }
        BackgroundPermit {
            gate: self,
            _turn: turn,
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
            // The count is left where it is rather than reset, so the one call
            // let through after the probe window re-opens the breaker the moment
            // it fails. Resetting cost `breaker_after` full timeouts — three
            // quarters of an hour at the default `timeout_secs` — to rediscover
            // the same dead endpoint on every probe cycle, which is the exact
            // cost this exists to avoid. `call_succeeded` is what clears it.
            tracing::warn!(
                probe_secs = self.probe_after.as_secs(),
                "inference endpoint failed repeatedly; holding background calls"
            );
        }
    }
}

/// The right to make one background inference call, held for as long as the
/// call runs.
///
/// Report the outcome through it — both methods consume the permit, so the
/// cooldown starts and the turn passes on at the moment the call ended rather
/// than whenever the caller got around to saying so.
pub struct BackgroundPermit<'a> {
    gate: &'a InferenceGate,
    _turn: SemaphorePermit<'a>,
}

impl BackgroundPermit<'_> {
    pub fn succeeded(self) {
        self.gate.call_succeeded();
    }

    pub fn failed(self, e: &Error) {
        self.gate.call_failed(e);
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
        let waiter = tokio::spawn(async move {
            g2.background().await;
        });

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
    async fn two_workers_do_not_put_two_generations_on_the_gpu_at_once() {
        // `server.workers` defaults to 2, and the cooldown alone is a check
        // rather than a turn: both workers read the same unchanged
        // `last_finished` — neither has finished — and both proceed. The gap the
        // operator configured then bounds each worker rather than the endpoint.
        let g = gate(5);
        let first = g.background().await;

        let g2 = Arc::clone(&g);
        let second = tokio::spawn(async move {
            g2.background().await;
            tokio::time::Instant::now()
        });
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(
            !second.is_finished(),
            "a second call started while the first was still running"
        );

        // And the waiter serves out the cooldown from when that call *ended*,
        // rather than from a stamp it read before it started.
        first.succeeded();
        let released = tokio::time::Instant::now();
        let acquired = second.await.unwrap();
        assert_eq!(acquired - released, Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_call_hands_the_turn_on() {
        // The turn is released by the permit, so a call that errored must not
        // hold the endpoint shut behind it.
        let g = gate(0);
        let permit = g.background().await;
        permit.failed(&Error::Inference {
            role: "chunk",
            detail: "502".into(),
        });
        tokio::time::timeout(Duration::from_secs(1), g.background())
            .await
            .expect("the turn was never handed on");
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
    async fn one_failure_after_the_probe_window_re_opens_the_breaker() {
        // The probe is one call let through to ask whether the endpoint is back.
        // If it is not, the answer is already in — making the queue earn the
        // whole run again spends `breaker_after` full timeouts, three quarters
        // of an hour at the default, rediscovering it on every cycle.
        let g =
            Arc::new(InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)));
        for _ in 0..3 {
            g.call_failed(&Error::Inference {
                role: "chunk",
                detail: "502".into(),
            });
        }
        tokio::time::advance(Duration::from_secs(61)).await;

        // The probe goes out, and the endpoint is still down.
        g.background().await.failed(&Error::Inference {
            role: "chunk",
            detail: "502".into(),
        });

        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(60),
            "the queue was let back onto a dead endpoint"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_breaker_configured_to_zero_is_off() {
        // Zero reads as "don't do this", the way `cooldown_secs = 0` does.
        // Clamping it up to one made it the most aggressive setting there is.
        let g =
            Arc::new(InferenceGate::new(Duration::ZERO).with_breaker(0, Duration::from_secs(60)));
        for _ in 0..10 {
            g.call_failed(&Error::Inference {
                role: "chunk",
                detail: "502".into(),
            });
        }
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
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
