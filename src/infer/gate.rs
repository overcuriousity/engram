//! One pacer in front of every inference call.
//!
//! Three jobs that all answer the same question — may a background call start
//! now? The cooldown protects a desktop GPU from unbroken load. The turn keeps
//! that answer true for the whole system rather than per worker. The
//! interactive lease keeps the worker from piling work onto the endpoint while
//! a person is waiting on `ask`. A dead endpoint is the job queue's backoff to
//! handle: the turn already serialises the discovery to one call at a time.
//!
//! That last one is a load-bearing claim rather than an aside, and it holds
//! only while nothing re-arms a unit that is already backing off. `enqueue`
//! re-arms whatever state it finds, so anything automatic that runs on a timer
//! has to use `rearm_idle_seq` instead — `Core::heal_store_drift` is the one
//! that reaches embed jobs, and it did reset them, which is what turned an
//! unreachable endpoint into a full-timeout call on every sweep, forever.
//!
//! It sits around calls rather than around jobs, so the two stages that make no
//! inference call — planning a corpus into windows, and the consolidation sweep
//! — never wait for a cooldown they did not earn.

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
}

impl InferenceGate {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            resumed: Notify::new(),
            turn: Semaphore::new(1),
            cooldown,
        }
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
                    match st.last_finished.map(|t| t + self.cooldown) {
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

    /// A call ended — well, badly, or refused. It occupied the GPU either way,
    /// so it starts the cooldown.
    pub fn call_finished(&self) {
        self.state.lock().expect("gate state").last_finished = Some(Instant::now());
    }
}

/// The right to make one background inference call, held for as long as the
/// call runs.
///
pub struct BackgroundPermit<'a> {
    gate: &'a InferenceGate,
    _turn: SemaphorePermit<'a>,
}

impl BackgroundPermit<'_> {
    /// The call ended, however it ended: the cooldown starts and the turn
    /// passes on at the moment the call ended rather than whenever the caller
    /// got around to saying so.
    pub fn finished(self) {
        self.gate.call_finished();
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
            // `last_finished` is deliberately not stamped. It was, and it made
            // the cooldown restart on every interactive call — which for search
            // means every keystroke's embed. A person paging through the UI held
            // background work off forever: each page pushed the gap out again,
            // and nothing here ages or bounds that. The cooldown paces batch
            // work against the GPU, and the interactive lane is already the
            // exception to it — `[pacing]` says so in as many words.
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
        g.call_finished();
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
        g.call_finished();
        let started = tokio::time::Instant::now();
        let _lease = g.interactive();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_person_paging_through_the_ui_cannot_starve_background_work() {
        // The lease used to stamp `last_finished` on the way out, so every
        // interactive call restarted the cooldown — and for search that is every
        // keystroke's embed. Each page pushed the gap out again, with nothing here
        // ageing the background work that was waiting, so on a long enough
        // browsing session it never ran at all.
        let g = gate(600);
        // A background call has just ended, so the cooldown is what the waiter
        // below is waiting on.
        g.call_finished();
        let t0 = tokio::time::Instant::now();
        let g2 = Arc::clone(&g);
        let waiter = tokio::spawn(async move {
            g2.background().await;
            tokio::time::Instant::now()
        });

        // Ten pages of results, each one an interactive call that begins and
        // ends, spread across the cooldown the waiter is already serving.
        for _ in 0..10 {
            let lease = g.interactive();
            tokio::time::advance(Duration::from_secs(60)).await;
            drop(lease);
        }

        let started_at = waiter.await.unwrap();
        assert_eq!(
            started_at.duration_since(t0),
            Duration::from_secs(600),
            "the searches pushed the cooldown out from under work that was already waiting"
        );
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
        first.finished();
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
        permit.finished();
        tokio::time::timeout(Duration::from_secs(1), g.background())
            .await
            .expect("the turn was never handed on");
    }
}
