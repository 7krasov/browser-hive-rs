//! Bounded synchronous CDP calls.
//!
//! Every headless_chrome call blocks its thread until the browser answers, and the only bound on
//! that wait is `idle_browser_timeout` — one hour (see `BrowserPool::new`). A wedged renderer
//! answers nothing, so an unbounded call on a runtime thread freezes the runtime and one under a
//! pool lock freezes the pool (production 2026-09-18, see TODO.md). [`bounded`] moves the call to
//! the blocking pool and stops *waiting* for it after [`CALL_TIMEOUT`].
//!
//! It cannot stop the call itself: the blocking thread stays parked inside headless_chrome until
//! the browser answers or the crate gives up. That costs a thread and a few KiB, never the pool
//! lock or a runtime thread, and their number is bounded — a caller that sees a stall removes the
//! context it was working on, so nothing stuck is handed out again.

use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::warn;

/// How long a caller waits for one CDP call.
///
/// Nothing measured it: a healthy call takes milliseconds, but a CPU-throttled browser can take
/// seconds to open a tab, and giving up on a healthy browser costs a request. [`SLOW_CALL`]
/// produces the data to tune it.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A call slower than this is logged with its name and duration, whether or not anyone still
/// waits for it.
const SLOW_CALL: Duration = Duration::from_secs(5);

/// A CDP call that did not answer within [`CALL_TIMEOUT`], or panicked.
///
/// Callers tell it apart from an error the browser answered with through [`is_stalled`]: the
/// browser refusing a call says nothing about the tab, a call nobody answered says the tab (or
/// the browser) stopped responding.
#[derive(Debug)]
pub struct StalledCall {
    call: &'static str,
    reason: StallReason,
}

#[derive(Debug)]
enum StallReason {
    TimedOut(Duration),
    Panicked,
}

impl std::fmt::Display for StalledCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            StallReason::TimedOut(after) => write!(
                f,
                "CDP call {} did not answer within {:?} - the browser stopped responding",
                self.call, after
            ),
            StallReason::Panicked => write!(f, "CDP call {} panicked", self.call),
        }
    }
}

impl std::error::Error for StalledCall {}

/// Whether `error` is a [`StalledCall`] rather than an answer from the browser.
pub fn is_stalled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<StalledCall>().is_some()
}

/// Run a blocking CDP call on the blocking pool and wait for it at most [`CALL_TIMEOUT`].
///
/// `call` names the CDP method(s) for the log and the error. Use [`bounded_with_cleanup`] when the
/// call creates something in the browser.
pub async fn bounded<T, F>(call: &'static str, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    bounded_with_cleanup(call, f, |_| {}).await
}

/// [`bounded`] for a call that creates something in the browser (a context, a tab).
///
/// If the call succeeds after the caller stopped waiting — it timed out, or its future was
/// dropped — nobody will ever receive the result, so `cleanup` runs with it on the same blocking
/// thread instead of leaking it in Chrome.
pub async fn bounded_with_cleanup<T, F, C>(
    call: &'static str,
    f: F,
    cleanup: C,
) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    C: FnOnce(T) + Send + 'static,
    T: Send + 'static,
{
    bounded_within(CALL_TIMEOUT, call, f, cleanup).await
}

async fn bounded_within<T, F, C>(
    timeout: Duration,
    call: &'static str,
    f: F,
    cleanup: C,
) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    C: FnOnce(T) + Send + 'static,
    T: Send + 'static,
{
    let (tx, mut rx) = oneshot::channel();
    // The span does not cross spawn_blocking; re-enter it so the call's own lines keep ray_id.
    let span = tracing::Span::current();

    tokio::task::spawn_blocking(move || {
        let _guard = span.enter();
        let started = Instant::now();
        let result = f();

        let elapsed = started.elapsed();
        if elapsed > SLOW_CALL {
            warn!("CDP call {} took {:?}", call, elapsed);
        }

        // A failed send means the waiter is gone: whatever was created is nobody's now.
        if let Err(Ok(orphan)) = tx.send(result) {
            cleanup(orphan);
        }
    });

    match tokio::time::timeout(timeout, &mut rx).await {
        Ok(Ok(result)) => result,
        // The sender was dropped without sending: the closure panicked.
        Ok(Err(_)) => Err(StalledCall {
            call,
            reason: StallReason::Panicked,
        }
        .into()),
        Err(_) => {
            // Close before the final look, so a result is either taken here or cleaned up by the
            // closure — never dropped in the channel between the two.
            rx.close();
            match rx.try_recv() {
                Ok(result) => result,
                Err(_) => Err(StalledCall {
                    call,
                    reason: StallReason::TimedOut(timeout),
                }
                .into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn passes_the_answer_through() {
        let answer = bounded("Test.ok", || Ok(42)).await.unwrap();
        assert_eq!(answer, 42);

        let error = bounded::<(), _>("Test.err", || Err(anyhow::anyhow!("refused")))
            .await
            .unwrap_err();
        assert!(!is_stalled(&error), "an answer is not a stall");
    }

    #[tokio::test]
    async fn a_panicking_call_is_a_stall() {
        let error = bounded::<(), _>("Test.panic", || panic!("boom"))
            .await
            .unwrap_err();
        assert!(is_stalled(&error));
        assert!(error.to_string().contains("Test.panic"));
    }

    #[tokio::test]
    async fn times_out_and_cleans_up_a_late_answer() {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleaned_by_call = cleaned.clone();

        let error = bounded_within(
            Duration::from_millis(50),
            "Test.slow",
            move || {
                released.recv().unwrap();
                Ok("created")
            },
            move |_| cleaned_by_call.store(true, Ordering::SeqCst),
        )
        .await
        .unwrap_err();
        assert!(is_stalled(&error));
        assert!(error.to_string().contains("Test.slow"));

        // The call finishes after the caller gave up: its result must be cleaned up.
        release.send(()).unwrap();
        for _ in 0..100 {
            if cleaned.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("late result was not cleaned up");
    }
}
