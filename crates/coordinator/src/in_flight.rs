//! Requests this coordinator has sent to each worker pod and not yet seen answered.
//!
//! Routing decides on `available_slots` from the discovery cache, which is up to one discovery
//! round (10 s) old — and the cache never learns about the requests the coordinator itself sends
//! in between. With small pods that is most of the information: at 2 slots per pod and requests
//! lasting 5-10 s, every request dispatched during one round was routed on a number that the
//! earlier requests of the same round had already made false. The measured result was a scope at
//! ~45% utilisation refusing ~15% of its requests with `no_slots`, while the pods that refused
//! were full and their neighbours idle.
//!
//! The correction is exact bookkeeping rather than a fresher cache:
//!
//! ```text
//! free now = available_slots at snapshot − (in flight now − in flight at snapshot)
//! ```
//!
//! Dispatches since the snapshot take slots the cache still shows as free; requests that were
//! running at the snapshot and have finished since return slots the cache still shows as taken.
//! No proto change is needed, and it holds in every session mode: the slot a `dedicated` session
//! keeps between its requests is already missing from `available_slots`, so requests continuing a
//! session are deliberately not counted (see the call site in `service.rs`).
//!
//! ⚠️ **Exact only because there is one coordinator.** Every request to a worker passes through
//! this process, so its count is the truth. A second coordinator replica would see only its own
//! share and the correction would shrink back toward the plain cache — never worse than that, but
//! no longer a fix. Running one coordinator is a recorded decision; revisit this module if it
//! changes.

use browser_hive_common::WorkerEndpoint;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Per-pod count of requests dispatched and not yet answered.
///
/// A plain `std::sync::Mutex`: the critical sections are a hash lookup, never held across an
/// `.await`, and the guard releases from `Drop`, which cannot be async.
#[derive(Debug, Default)]
pub struct InFlightTracker {
    by_pod: Mutex<HashMap<String, usize>>,
}

impl InFlightTracker {
    /// Requests currently in flight to `pod_name`.
    pub fn current(&self, pod_name: &str) -> usize {
        self.lock().get(pod_name).copied().unwrap_or(0)
    }

    /// Count one request to `pod_name` until the returned guard is dropped.
    ///
    /// The guard, not an explicit call, ends the count: the request future is dropped without
    /// returning when the client disconnects or its deadline fires, and a count that leaked there
    /// would make the pod look busier than it is for the life of the process.
    pub fn start(self: &Arc<Self>, pod_name: &str) -> InFlightGuard {
        *self.lock().entry(pod_name.to_string()).or_insert(0) += 1;
        InFlightGuard {
            tracker: Arc::clone(self),
            pod_name: pod_name.to_string(),
        }
    }

    /// Free slots `worker` has right now, as far as this coordinator can tell.
    pub fn free_slots(&self, worker: &WorkerEndpoint) -> usize {
        effective_free_slots(worker, self.current(&worker.pod_name))
    }

    fn finish(&self, pod_name: &str) {
        let mut by_pod = self.lock();
        if let Some(count) = by_pod.get_mut(pod_name) {
            *count = count.saturating_sub(1);
            // Pods come and go with every rollout; an entry is only worth keeping while non-zero.
            if *count == 0 {
                by_pod.remove(pod_name);
            }
        }
    }

    /// Nothing inside the lock can panic halfway through an update, so a poisoned map is still
    /// consistent — recovering it beats turning one panic into a coordinator that refuses to route.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.by_pod.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps one request counted against its pod. See [`InFlightTracker::start`].
#[must_use = "the request stops being counted as soon as the guard is dropped"]
#[derive(Debug)]
pub struct InFlightGuard {
    tracker: Arc<InFlightTracker>,
    pod_name: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.tracker.finish(&self.pod_name);
    }
}

/// `available_slots` corrected by what happened since the snapshot was taken. See the module docs.
///
/// Written as `available + baseline − now` so it stays in unsigned arithmetic; it saturates at 0
/// because a count can briefly run ahead of the worker (a request counted before it connects).
pub fn effective_free_slots(worker: &WorkerEndpoint, in_flight_now: usize) -> usize {
    (worker.stats.available_slots + worker.in_flight_at_snapshot).saturating_sub(in_flight_now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_hive_common::WorkerStats;

    fn worker(
        pod_name: &str,
        available_slots: usize,
        in_flight_at_snapshot: usize,
    ) -> WorkerEndpoint {
        WorkerEndpoint {
            pod_name: pod_name.to_string(),
            pod_ip: "10.0.0.1".to_string(),
            port: 50052,
            scope_name: "test_scope".to_string(),
            stats: WorkerStats {
                available_slots,
                ..Default::default()
            },
            in_flight_at_snapshot,
            is_terminating: false,
        }
    }

    #[test]
    fn a_guard_counts_until_dropped() {
        let tracker = Arc::new(InFlightTracker::default());

        let first = tracker.start("pod-a");
        let second = tracker.start("pod-a");
        assert_eq!(tracker.current("pod-a"), 2);
        assert_eq!(tracker.current("pod-b"), 0);

        drop(first);
        assert_eq!(tracker.current("pod-a"), 1);
        drop(second);
        assert_eq!(tracker.current("pod-a"), 0);
    }

    /// Pod names change on every rollout; a map that kept zero entries would grow for the life of
    /// the process.
    #[test]
    fn an_idle_pod_leaves_no_entry() {
        let tracker = Arc::new(InFlightTracker::default());
        drop(tracker.start("pod-a"));

        assert!(tracker.lock().is_empty());
    }

    /// The failure this module exists for: the cache says 2 free, the coordinator has since sent
    /// two requests there, so the pod is full whatever the cache says.
    #[test]
    fn dispatches_since_the_snapshot_take_slots() {
        let tracker = Arc::new(InFlightTracker::default());
        let pod = worker("pod-a", 2, 0);

        let _first = tracker.start("pod-a");
        assert_eq!(tracker.free_slots(&pod), 1);
        let _second = tracker.start("pod-a");
        assert_eq!(tracker.free_slots(&pod), 0);
    }

    /// The opposite staleness: a request that was running when the stats were taken has finished,
    /// so its slot is free although the cache still counts it as taken.
    #[test]
    fn requests_finished_since_the_snapshot_return_slots() {
        let tracker = Arc::new(InFlightTracker::default());
        let running_at_snapshot = tracker.start("pod-a");
        let pod = worker("pod-a", 1, tracker.current("pod-a"));

        assert_eq!(tracker.free_slots(&pod), 1);
        drop(running_at_snapshot);
        assert_eq!(tracker.free_slots(&pod), 2);
    }

    #[test]
    fn never_goes_below_zero() {
        let tracker = Arc::new(InFlightTracker::default());
        let pod = worker("pod-a", 0, 0);

        let _guard = tracker.start("pod-a");
        assert_eq!(tracker.free_slots(&pod), 0);
    }
}
