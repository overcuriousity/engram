//! One bounded retry, for an inference call somebody is waiting on.
//!
//! The job workers have had backoff for as long as they have had a queue: a
//! unit that fails is re-armed, and the sweep's own cadence is the gap. Nothing
//! ever did that for the interactive path, because the interactive path has no
//! second chance to arrange — a person typed, and the answer is wanted now or
//! not at all. So a search whose embedding call met a rate limit failed
//! outright, and the box reported it.
//!
//! What makes a retry affordable here is that the answer is disposable. Search
//! runs on every debounced keystroke, and each result supersedes the one before
//! it, so the deadline is not a number anyone has to choose: it is the next
//! keystroke. The box already aborts the request in flight (`hx-sync` on
//! `#box-form`), which closes the connection, which drops the handler future —
//! and this loop dies with it, mid-sleep, without spending another attempt on
//! an answer nobody will read. Cancellation is the caller's, and it is free.
//!
//! [`INTERACTIVE_BUDGET`] is only the backstop for the person who typed and
//! then stopped, where no keystroke is coming to end the wait.

use crate::error::Result;
use std::future::Future;
use std::time::Duration;

/// How long a call a person is waiting on may spend being retried.
///
/// Past about a second the rail stops feeling like it belongs to the keystroke
/// that asked for it, and a search that arrives late is worse than one that
/// says it failed: the box has moved on and the results are answering a
/// question that is no longer on screen.
pub const INTERACTIVE_BUDGET: Duration = Duration::from_millis(1200);

/// The first gap, doubled per attempt. Each actual sleep is drawn from
/// `[0, gap]` — see [`jitter`].
const FIRST_GAP: Duration = Duration::from_millis(120);

/// Call, and call again while the failure is one that waiting can fix.
///
/// Retryability is [`crate::error::Error::retryable`]'s answer and not a second
/// opinion: the classification already exists, is made once where the status is
/// read, and is what the workers use. A rejection comes straight back.
///
/// The loop gives up by returning the *last* error rather than a summary of the
/// attempts, so what reaches the log and the page is what the endpoint actually
/// last said.
pub async fn transiently<T, F, Fut>(budget: Duration, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    // Tokio's clock rather than the standard library's, so a test can pause it
    // and a sleep costs no wall time.
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
        // Checked before sleeping rather than after: a loop that wakes up past
        // its deadline has already spent the time it was not allowed to spend.
        if started.elapsed() + wait > budget {
            return Err(e);
        }
        tokio::time::sleep(wait).await;
        gap *= 2;
    }
}

/// A duration drawn uniformly from `[0, gap]`.
///
/// Full jitter rather than a plain doubling, because the thing being backed off
/// from is a limiter shared by everything this server talks to. Two searches
/// refused in the same instant retry on the same curve unless something breaks
/// the tie, and a rate limit met by a synchronised retry is met again — which
/// is how a blip becomes a stampede.
///
/// No random-number dependency for it. `RandomState` is seeded per process and
/// stepped per instance, so hashing the clock through a fresh one differs
/// between two calls made in the same nanosecond, which is exactly the pair
/// this has to separate.
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

    /// The case this module exists for: the endpoint said "not now", and by the
    /// second ask it was ready.
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

    /// A refusal is the same answer however many times it is asked, and asking
    /// again spends a person's second on a foregone conclusion.
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

    /// The backstop. Nothing is coming back, and the loop still has to end
    /// inside the budget — the person who typed and then stopped is waiting on
    /// this, with a spinner.
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
        // Full jitter makes the count a range rather than a number: every gap
        // can come back as nearly zero. The claim worth pinning is that it
        // tried more than once and did not spin.
        let n = calls.load(Ordering::SeqCst);
        assert!((2..=32).contains(&n), "{n} attempts inside the budget");
    }

    /// The last word is the endpoint's, not a summary of the attempts: an
    /// operator reading the log needs what it actually said.
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

    /// A budget too small to spend is not a licence to skip the call — the
    /// first attempt is not a retry, and it always happens.
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
