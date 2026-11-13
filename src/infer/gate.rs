//! One pacer before every inference call: a cooldown for the GPU, one system-wide turn, and an interactive lease.
//! A dead endpoint is the queue's backoff to handle, which holds only while timers use `rearm_idle_seq`, not `enqueue`.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore, SemaphorePermit};
use tokio::time::Instant;

#[derive(Default)]
struct GateState {
    /// A count, not a flag: `ask` makes more than one call and the UI can have two queries open.
    interactive: usize,
    last_finished: Option<Instant>,
}

pub struct InferenceGate {
    state: Mutex<GateState>,
    resumed: Notify,
    /// System-wide, not per worker: two workers read the same `last_finished` and would both start.
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

    /// Hold the permit for the duration of the call.
    pub async fn background(&self) -> BackgroundPermit<'_> {
        self.acquire(true).await
    }

    /// The turn without the cooldown, for embedding: a seconds-long batch should not serve a generation's gap.
    /// Still takes the turn and yields to the lease; does not stamp `last_finished`.
    pub async fn background_light(&self) -> BackgroundPermit<'_> {
        self.acquire(false).await
    }

    /// `paced`: serve the cooldown before and start one after; a call exempt from one is exempt from both.
    async fn acquire(&self, paced: bool) -> BackgroundPermit<'_> {
        // Turn before the wait, so the holder re-reads the cooldown after the call ahead finishes.
        let turn = self.turn.acquire().await.ok();
        loop {
            // Registered before the guard drops: `notify_waiters` wakes only registered waiters and leaves no permit.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);

            // The lock is never held across an await.
            let wait = {
                let st = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if st.interactive > 0 {
                    resumed.as_mut().enable();
                    None
                } else if !paced {
                    break;
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
                // No deadline; the lease wakes us when it drops.
                None => resumed.await,
            }
        }
        BackgroundPermit {
            gate: self,
            _turn: turn,
            paced,
        }
    }

    pub fn interactive(self: &Arc<Self>) -> InteractiveLease {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interactive += 1;
        InteractiveLease {
            gate: Arc::clone(self),
        }
    }

    /// It occupied the GPU either way, so it starts the cooldown.
    pub fn call_finished(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_finished = Some(Instant::now());
    }
}

pub struct BackgroundPermit<'a> {
    gate: &'a InferenceGate,
    // `None` only if the semaphore were closed, which nothing does.
    _turn: Option<SemaphorePermit<'a>>,
    paced: bool,
}

impl BackgroundPermit<'_> {
    /// The cooldown starts when the call ended, not when the caller says so.
    pub fn finished(self) {
        if self.paced {
            self.gate.call_finished();
        }
    }
}

pub struct InteractiveLease {
    gate: Arc<InferenceGate>,
}

impl Drop for InteractiveLease {
    fn drop(&mut self) {
        {
            let mut st = self
                .gate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            st.interactive = st.interactive.saturating_sub(1);
            // No `last_finished` stamp: otherwise every search keystroke's embed restarts the cooldown and starves background work.
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
        let g = gate(600);
        g.call_finished();
        let started = tokio::time::Instant::now();
        let _lease = g.interactive();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_person_paging_through_the_ui_cannot_starve_background_work() {
        // Interactive calls must not restart the cooldown, or browsing starves background work.
        let g = gate(600);
        g.call_finished();
        let t0 = tokio::time::Instant::now();
        let g2 = Arc::clone(&g);
        let waiter = tokio::spawn(async move {
            g2.background().await;
            tokio::time::Instant::now()
        });

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
        // `server.workers` defaults to 2; a cooldown without the turn bounds each worker, not the endpoint.
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

        // The cooldown runs from when the call ended, not from a stamp read before it.
        first.finished();
        let released = tokio::time::Instant::now();
        let acquired = second.await.unwrap();
        assert_eq!(acquired - released, Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_light_call_does_not_serve_the_cooldown() {
        let g = gate(600);
        g.call_finished();
        let started = tokio::time::Instant::now();
        g.background_light().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_light_call_does_not_start_a_cooldown_behind_it() {
        let g = gate(600);
        let permit = g.background_light().await;
        permit.finished();
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_light_call_still_takes_its_turn() {
        // Exempt from the gap, not from one-at-a-time: they share a GPU.
        let g = gate(0);
        let heavy = g.background().await;

        let g2 = Arc::clone(&g);
        let light = tokio::spawn(async move {
            g2.background_light().await;
        });
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(
            !light.is_finished(),
            "an embed batch ran alongside a generation"
        );

        heavy.finished();
        light.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_light_call_still_yields_to_someone_waiting_on_ask() {
        let g = gate(0);
        let lease = g.interactive();
        let g2 = Arc::clone(&g);
        let light = tokio::spawn(async move {
            g2.background_light().await;
        });

        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            !light.is_finished(),
            "an embed batch ran while an ask was in flight"
        );

        drop(lease);
        light.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_call_hands_the_turn_on() {
        let g = gate(0);
        let permit = g.background().await;
        permit.finished();
        tokio::time::timeout(Duration::from_secs(1), g.background())
            .await
            .expect("the turn was never handed on");
    }
}
