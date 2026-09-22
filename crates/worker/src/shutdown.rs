//! Graceful shutdown: let in-flight requests finish before cancelling them.
//!
//! On SIGTERM the worker, in this order:
//! 1. reports itself unhealthy (`is_ready = false`) — the coordinator's health monitor polls every
//!    second and stops routing here; a request that still arrives is answered `TERMINATING`
//!    without being started, so the coordinator re-sends it elsewhere at no cost;
//! 2. waits at least [`MIN_UNROUTABLE_PAUSE`], even with nothing in flight, so the coordinator has
//!    seen the unhealthy answer before the gRPC server closes — otherwise it would keep connecting
//!    to a closed port for up to a health round;
//! 3. waits for the requests in flight to finish, at most [`DRAIN_TIMEOUT_ENV`] (a second signal
//!    ends the wait early);
//! 4. cancels what is left, which answers it `TERMINATING`, and waits for those handlers to return.
//!
//! Before this, step 4 came first: every rollout, KEDA scale-down and spot preemption aborted all
//! requests in flight, which the coordinator could retry only while ≥ 10 s of the client's
//! deadline remained.
//!
//! ⚠️ The bound must fit the time Kubernetes actually grants, which is not always
//! `terminationGracePeriodSeconds`: on a GKE Spot preemption a non-system pod gets at most 15 s
//! (unless the node pool's `shutdownGracePeriodSeconds` is raised). A SIGKILL that lands mid-drain
//! breaks the connections of the requests still running; the coordinator retries those on another
//! pod as an unreachable worker, so a short real grace period costs no more than cancelling early.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::signal;
use tokio::time::Instant;
use tokio_cancellation_ext::CancellationToken;
use tracing::{info, warn};

/// Upper bound on the wait for in-flight requests, in whole seconds.
pub const DRAIN_TIMEOUT_ENV: &str = "WORKER_SHUTDOWN_DRAIN_TIMEOUT_SECS";

/// Fits the Kubernetes default `terminationGracePeriodSeconds` (30 s). A default larger than the
/// grace period the pod really gets would end in SIGKILL mid-drain — no worse than cancelling at
/// once, but with no shutdown lines in the log. A deployment with a longer grace period sets it
/// explicitly (up to the longest request it serves).
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(25);

/// Shortest time between turning unhealthy and closing the gRPC server: two coordinator health
/// rounds (1 s each) plus slack.
const MIN_UNROUTABLE_PAUSE: Duration = Duration::from_secs(3);

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The drain bound from [`DRAIN_TIMEOUT_ENV`], or [`DEFAULT_DRAIN_TIMEOUT`] when unset or
/// malformed. `0` is allowed and restores the old behaviour (cancel right after the pause).
pub fn drain_timeout_from_env() -> Duration {
    parse_drain_timeout(std::env::var(DRAIN_TIMEOUT_ENV).ok().as_deref())
}

fn parse_drain_timeout(value: Option<&str>) -> Duration {
    match value {
        None => DEFAULT_DRAIN_TIMEOUT,
        Some(value) => match value.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => {
                warn!(
                    "{DRAIN_TIMEOUT_ENV}={value:?} is not a number of seconds - using {:?}",
                    DEFAULT_DRAIN_TIMEOUT
                );
                DEFAULT_DRAIN_TIMEOUT
            }
        },
    }
}

/// How the wait for in-flight requests ended.
#[derive(Debug, PartialEq, Eq)]
enum DrainOutcome {
    Drained,
    TimedOut { remaining: usize },
    Interrupted { remaining: usize },
}

/// Wait until no request is in flight and `min_pause` has passed, but no longer than `timeout`
/// (which also caps the pause) and no longer than until `interrupt` resolves.
async fn drain(
    active: &AtomicUsize,
    min_pause: Duration,
    timeout: Duration,
    interrupt: impl Future<Output = ()>,
) -> DrainOutcome {
    let start = Instant::now();
    let deadline = start + timeout;
    let pause_end = start + min_pause.min(timeout);
    tokio::pin!(interrupt);

    let mut last_logged = None;
    loop {
        let now = Instant::now();
        let remaining = active.load(Ordering::SeqCst);
        if remaining == 0 && now >= pause_end {
            return DrainOutcome::Drained;
        }
        if now >= deadline {
            return DrainOutcome::TimedOut { remaining };
        }
        // Only on change: a long request would otherwise log twice a second for minutes.
        if remaining > 0 && last_logged != Some(remaining) {
            info!(
                "Draining: {} request(s) in flight, {:?} left",
                remaining,
                deadline - now
            );
            last_logged = Some(remaining);
        }
        tokio::select! {
            _ = &mut interrupt => {
                return DrainOutcome::Interrupted { remaining: active.load(Ordering::SeqCst) };
            }
            _ = tokio::time::sleep_until((now + POLL_INTERVAL).min(deadline)) => {}
        }
    }
}

/// SIGTERM or Ctrl+C, whichever comes first. Each call waits for a new signal.
async fn termination_signal() -> &'static str {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => "Ctrl+C",
        _ = terminate => "SIGTERM",
    }
}

/// Resolves when the gRPC server may close: after the drain described in the module docs.
pub(crate) async fn shutdown_signal(
    active_requests: Arc<AtomicUsize>,
    is_ready: Arc<AtomicBool>,
    cancellation_token: CancellationToken,
    drain_timeout: Duration,
) {
    let signal_name = termination_signal().await;
    warn!("Received {} signal", signal_name);

    is_ready.store(false, Ordering::SeqCst);
    info!(
        "Worker marked unhealthy; taking no new requests, draining in-flight ones for up to {:?}",
        drain_timeout
    );

    let interrupt = async {
        let signal_name = termination_signal().await;
        warn!("Received a second {} signal, ending the drain", signal_name);
    };
    match drain(
        &active_requests,
        MIN_UNROUTABLE_PAUSE,
        drain_timeout,
        interrupt,
    )
    .await
    {
        DrainOutcome::Drained => info!("All in-flight requests completed"),
        DrainOutcome::TimedOut { remaining } => warn!(
            "Drain timeout ({:?}) reached with {} request(s) still in flight; cancelling them",
            drain_timeout, remaining
        ),
        DrainOutcome::Interrupted { remaining } => {
            warn!("Drain interrupted; cancelling {} request(s)", remaining)
        }
    }

    // Also stops background work that watches the token, so it runs even when nothing is left.
    cancellation_token.cancel();

    // Cancelled handlers answer TERMINATING and return promptly; the hard limit is SIGKILL.
    while active_requests.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    info!("Starting gRPC server shutdown");
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAUSE: Duration = Duration::from_secs(3);

    fn never() -> impl Future<Output = ()> {
        std::future::pending()
    }

    #[test]
    fn drain_timeout_parsing() {
        assert_eq!(parse_drain_timeout(None), DEFAULT_DRAIN_TIMEOUT);
        assert_eq!(parse_drain_timeout(Some("330")), Duration::from_secs(330));
        assert_eq!(parse_drain_timeout(Some(" 0 ")), Duration::ZERO);
        assert_eq!(parse_drain_timeout(Some("5m")), DEFAULT_DRAIN_TIMEOUT);
        assert_eq!(parse_drain_timeout(Some("")), DEFAULT_DRAIN_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_worker_still_waits_the_pause() {
        let active = AtomicUsize::new(0);
        let start = Instant::now();
        let outcome = drain(&active, PAUSE, Duration::from_secs(25), never()).await;
        assert_eq!(outcome, DrainOutcome::Drained);
        assert!(start.elapsed() >= PAUSE && start.elapsed() < PAUSE + POLL_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_request_is_allowed_to_finish() {
        let active = Arc::new(AtomicUsize::new(1));
        let finisher = active.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            finisher.fetch_sub(1, Ordering::SeqCst);
        });
        let start = Instant::now();
        let outcome = drain(&active, PAUSE, Duration::from_secs(25), never()).await;
        assert_eq!(outcome, DrainOutcome::Drained);
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_secs(10) && elapsed <= Duration::from_secs(11));
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_request_times_out_at_the_bound() {
        let active = AtomicUsize::new(2);
        let start = Instant::now();
        let outcome = drain(&active, PAUSE, Duration::from_secs(25), never()).await;
        assert_eq!(outcome, DrainOutcome::TimedOut { remaining: 2 });
        assert_eq!(start.elapsed(), Duration::from_secs(25));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_shorter_than_the_pause_caps_it() {
        let active = AtomicUsize::new(0);
        let start = Instant::now();
        let outcome = drain(&active, PAUSE, Duration::ZERO, never()).await;
        assert_eq!(outcome, DrainOutcome::Drained);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn second_signal_ends_the_drain() {
        let active = AtomicUsize::new(1);
        let start = Instant::now();
        let interrupt = tokio::time::sleep(Duration::from_secs(4));
        let outcome = drain(&active, PAUSE, Duration::from_secs(25), interrupt).await;
        assert_eq!(outcome, DrainOutcome::Interrupted { remaining: 1 });
        assert_eq!(start.elapsed(), Duration::from_secs(4));
    }
}
