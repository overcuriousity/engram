//! One bounded retry for interactive calls. The real deadline is the next keystroke: `hx-sync` aborts
//! the request, which drops this future mid-sleep. [`INTERACTIVE_BUDGET`] backs up the person who stopped typing.

use crate::error::Result;
use std::future::Future;
use std::time::Duration;

/// Past about a second a late search answers a question no longer on screen.
pub const INTERACTIVE_BUDGET: Duration = Duration::from_millis(1200);

/// Doubled per attempt; each sleep is drawn from `[0, gap]` (see [`jitter`]).
const FIRST_GAP: Duration = Duration::from_millis(120);

/// Retryability is [`crate::error::Error::retryable`]'s; the last error is returned as the endpoint said it.
pub async fn transiently<T, F, Fut>(budget: Duration, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    // Tokio's clock so a test can pause it.
    let started = tokio::time::Instant::now();
    let mut gap = FIRST_GAP;
    loop {
        let e = match call().await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        if !e.retryable() {
            return Err(e);
        }
        let wait = jitter(gap);
        // Checked before sleeping: waking past the deadline has already overspent.
        if started.elapsed() + wait > budget {
            return Err(e);
        }
        tokio::time::sleep(wait).await;
        gap *= 2;
    }
}

/// Full jitter, so retries against a shared rate limiter do not synchronise into a stampede.
/// A fresh `RandomState` differs even between two calls in the same nanosecond.
fn jitter(gap: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let micros = gap.as_micros() as u64;
    if micros == 0 {
        return Duration::ZERO;
    }
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    Duration::from_micros(h.finish() % (micros + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn busy() -> Error {
        Error::InferenceBusy {
            role: "embed",
            detail: "HTTP 429 Too Many Requests".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_endpoint_is_asked_again() {
        let calls = AtomicUsize::new(0);
        let got: Result<&str> = transiently(INTERACTIVE_BUDGET, || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(busy()),
                _ => Ok("vector"),
            }
        })
        .await;
        assert_eq!(got.unwrap(), "vector");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the retry never happened");
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejection_is_not_retried() {
        let calls = AtomicUsize::new(0);
        let got: Result<&str> = transiently(INTERACTIVE_BUDGET, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::InferenceRejected {
                role: "embed",
                detail: "model does not exist".into(),
            })
        })
        .await;
        assert!(matches!(got, Err(Error::InferenceRejected { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a refusal was re-asked");
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_bounds_the_whole_call() {
        let started = tokio::time::Instant::now();
        let calls = AtomicUsize::new(0);
        let got: Result<&str> = transiently(INTERACTIVE_BUDGET, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(busy())
        })
        .await;
        assert!(matches!(got, Err(Error::InferenceBusy { .. })));
        assert!(
            started.elapsed() <= INTERACTIVE_BUDGET,
            "the loop ran {:?} past a {:?} budget",
            started.elapsed(),
            INTERACTIVE_BUDGET
        );
        // Full jitter can make every gap near zero, so only "more than once, no spin" is pinned.
        let n = calls.load(Ordering::SeqCst);
        assert!((2..=32).contains(&n), "{n} attempts inside the budget");
    }

    #[tokio::test(start_paused = true)]
    async fn the_error_returned_is_the_last_one() {
        let calls = AtomicUsize::new(0);
        let got: Result<&str> = transiently(Duration::from_millis(400), || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::Inference {
                role: "embed",
                detail: format!("attempt {n}"),
            })
        })
        .await;
        let Err(Error::Inference { detail, .. }) = got else {
            panic!("expected the inference failure back");
        };
        let last = calls.load(Ordering::SeqCst) - 1;
        assert_eq!(detail, format!("attempt {last}"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_attempt_is_never_the_one_that_is_skipped() {
        let calls = AtomicUsize::new(0);
        let got: Result<&str> = transiently(Duration::ZERO, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(busy())
        })
        .await;
        assert!(got.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn jitter_stays_inside_its_gap() {
        for _ in 0..64 {
            assert!(jitter(FIRST_GAP) <= FIRST_GAP);
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }
}
