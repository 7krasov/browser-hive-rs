//! Prometheus metrics for the coordinator.
//!
//! The worker's metrics answer "how busy is this pod"; these answer the question the worker
//! metrics structurally cannot: **how much demand was refused**. There is no queue anywhere in
//! Browser Hive — when no worker has a free slot the coordinator returns
//! `ERROR_CODE_NO_WORKERS_AVAILABLE` (5001) immediately and the request is gone. That rejection
//! never touches a worker, so it leaves no trace in any `browser_hive_worker_*` series: a fleet
//! at 100% utilization and a fleet turning away half its traffic look identical from the worker
//! side. `requests_rejected_total` is therefore the signal that says "add replicas", and the
//! only one that can distinguish "not enough capacity" from "discovery is broken" — hence the
//! `reason` label rather than a single counter.
//!
//! Two design points mirror the worker's `metrics.rs` deliberately:
//!
//! - **Gauges are refreshed on every scrape** (`refresh_cluster_gauges`), not in the request
//!   path, so they cannot drift when discovery or the health monitor changes state through a
//!   path nobody remembered to instrument. They expose the *coordinator's own view* of the
//!   fleet, which is what routing decides on — when it disagrees with the workers' own
//!   `available_slots`, the discovery cache is stale and that is exactly the bug to see.
//! - **Counters are recorded through an RAII guard** (`RequestMetrics`), because `scrape_page`
//!   returns from a dozen places and a future that is dropped mid-request (client disconnect)
//!   must still be counted.

use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use browser_hive_common::WorkerEndpoint;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Label value used when the request's scope does not exist.
///
/// The `scope` label is the only metric label fed from client input, and an unknown scope name is
/// by definition not from the configured set — a buggy or hostile client could otherwise mint an
/// unbounded number of time series. The real name is still in the logs (`span_scope`).
const UNKNOWN_SCOPE: &str = "unknown";

/// Why the coordinator refused a request without a worker ever seeing it.
///
/// Kept as a closed enum rather than free-form strings so the label stays bounded and so adding
/// a rejection path forces a decision about which bucket it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The requested scope is not known to discovery at all — a configuration or label mismatch,
    /// never a capacity problem. Reported under the `unknown` scope label.
    ScopeNotFound,
    /// The scope exists but has no worker pods, or none that routing could select.
    /// Usually means the deployment scaled to zero or every pod is unhealthy.
    NoWorkers,
    /// Workers exist but every slot is taken (confirmed against fresh worker stats).
    /// **This is the "add replicas" signal.**
    NoSlots,
    /// The client sent a `session_id` that has expired, or whose worker is gone.
    SessionNotFound,
    /// The coordinator itself is shutting down.
    Terminating,
    /// Routing picked a worker but the gRPC connection to it could not be established.
    WorkerUnreachable,
}

impl RejectReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ScopeNotFound => "scope_not_found",
            Self::NoWorkers => "no_workers",
            Self::NoSlots => "no_slots",
            Self::SessionNotFound => "session_not_found",
            Self::Terminating => "terminating",
            Self::WorkerUnreachable => "worker_unreachable",
        }
    }

    /// Whether the scope label must be replaced by [`UNKNOWN_SCOPE`]. See that constant.
    fn scope_is_untrusted(self) -> bool {
        matches!(self, Self::ScopeNotFound)
    }
}

/// All reasons, so every series exists from process start and a `rate()` over a rejection that
/// has not happened yet returns 0 instead of no data (which alerts and stacked graphs handle
/// very differently).
const ALL_REJECT_REASONS: [RejectReason; 6] = [
    RejectReason::ScopeNotFound,
    RejectReason::NoWorkers,
    RejectReason::NoSlots,
    RejectReason::SessionNotFound,
    RejectReason::Terminating,
    RejectReason::WorkerUnreachable,
];

#[derive(Clone)]
pub struct CoordinatorMetrics {
    pub registry: Arc<Registry>,
    requests_total: IntCounterVec,
    requests_rejected_total: IntCounterVec,
    request_duration_seconds: HistogramVec,
    scope_workers_total: IntGaugeVec,
    scope_workers_healthy: IntGaugeVec,
    scope_available_slots: IntGaugeVec,
}

impl CoordinatorMetrics {
    pub fn new() -> anyhow::Result<Self> {
        let registry = Arc::new(Registry::new());

        let requests_total = IntCounterVec::new(
            Opts::new(
                "browser_hive_coordinator_requests_total",
                "Total scrape requests received by the coordinator",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        // The metric this module exists for: demand the coordinator turned away.
        let requests_rejected_total = IntCounterVec::new(
            Opts::new(
                "browser_hive_coordinator_requests_rejected_total",
                "Requests refused by the coordinator without reaching a worker, by reason",
            ),
            &["scope", "reason"],
        )?;
        registry.register(Box::new(requests_rejected_total.clone()))?;

        // Coordinator-side end-to-end duration: worker time plus routing, retries and the
        // fresh-stats round trip. Rejections land in the sub-second buckets, so the histogram
        // doubles as a check that rejections really are cheap.
        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "browser_hive_coordinator_request_duration_seconds",
                "End-to-end coordinator scrape_page duration in seconds",
            )
            .buckets(vec![
                0.005, 0.05, 0.25, 1.0, 2.0, 5.0, 8.0, 13.0, 21.0, 34.0, 60.0,
            ]),
            &["scope"],
        )?;
        registry.register(Box::new(request_duration_seconds.clone()))?;

        let scope_workers_total = IntGaugeVec::new(
            Opts::new(
                "browser_hive_coordinator_scope_workers_total",
                "Worker pods discovered per scope (the coordinator's own view)",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(scope_workers_total.clone()))?;

        let scope_workers_healthy = IntGaugeVec::new(
            Opts::new(
                "browser_hive_coordinator_scope_workers_healthy",
                "Discovered worker pods that passed the last health check, per scope",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(scope_workers_healthy.clone()))?;

        let scope_available_slots = IntGaugeVec::new(
            Opts::new(
                "browser_hive_coordinator_scope_available_slots",
                "Free slots per scope as seen by the coordinator's discovery cache",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(scope_available_slots.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            requests_rejected_total,
            request_duration_seconds,
            scope_workers_total,
            scope_workers_healthy,
            scope_available_slots,
        })
    }

    /// Pre-create the per-scope series so they exist before the first request.
    ///
    /// Driven from the scrape-time gauge refresh rather than from startup, because the
    /// coordinator learns its scopes from discovery and not from configuration.
    fn register_scope(&self, scope: &str) {
        let labels = [scope];
        self.requests_total.with_label_values(&labels);
        self.request_duration_seconds.with_label_values(&labels);
        for reason in ALL_REJECT_REASONS {
            self.requests_rejected_total
                .with_label_values(&[scope, reason.as_str()]);
        }
    }

    /// Refresh the fleet-view gauges from the discovery cache and the health set.
    ///
    /// Scopes that vanish from discovery keep their last value rather than being deleted: a
    /// scope whose pods all disappeared is precisely the state worth seeing, and a removed
    /// series would render as a gap indistinguishable from Prometheus losing the target.
    async fn refresh_cluster_gauges(
        &self,
        workers: &Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>>,
        healthy: &Arc<RwLock<HashSet<String>>>,
    ) {
        let workers = workers.read().await;
        let healthy = healthy.read().await;

        for (scope, endpoints) in workers.iter() {
            self.register_scope(scope);

            let labels = [scope.as_str()];
            self.scope_workers_total
                .with_label_values(&labels)
                .set(endpoints.len() as i64);
            self.scope_workers_healthy.with_label_values(&labels).set(
                endpoints
                    .iter()
                    .filter(|w| healthy.contains(&w.pod_name))
                    .count() as i64,
            );
            self.scope_available_slots.with_label_values(&labels).set(
                endpoints
                    .iter()
                    .map(|w| w.stats.available_slots as i64)
                    .sum(),
            );
        }
    }

    /// Start the HTTP server exposing `/metrics`.
    pub async fn start_server(
        self,
        port: u16,
        workers: Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>>,
        healthy: Arc<RwLock<HashSet<String>>>,
    ) -> anyhow::Result<()> {
        let state = MetricsState {
            metrics: self,
            workers,
            healthy,
        };

        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;

        tracing::info!("Coordinator metrics server listening on {}", addr);

        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Per-request metrics recorder.
///
/// Records on `Drop` rather than at the return sites: `scrape_page` returns from a dozen places
/// and, more importantly, its future is dropped without returning when the client disconnects or
/// the gRPC deadline fires — a request that was abandoned still consumed capacity and must be
/// counted. `reject()` only stores the reason; nothing is emitted until the guard drops, so the
/// reason and the duration are always recorded together.
pub struct RequestMetrics {
    metrics: Option<CoordinatorMetrics>,
    scope: String,
    reject_reason: Option<RejectReason>,
    started_at: std::time::Instant,
}

impl RequestMetrics {
    /// `metrics` is `None` when `COORDINATOR_ENABLE_METRICS` is off, which makes every method
    /// on the guard a no-op and keeps the call sites free of conditionals.
    pub fn new(metrics: Option<CoordinatorMetrics>, scope: &str) -> Self {
        Self {
            metrics,
            scope: scope.to_string(),
            reject_reason: None,
            started_at: std::time::Instant::now(),
        }
    }

    /// Re-point the request at the scope it is actually served from.
    ///
    /// A request carrying a `session_id` is routed by the session's scope, not the one in the
    /// request body; the span records the same override.
    pub fn set_scope(&mut self, scope: &str) {
        self.scope = scope.to_string();
    }

    pub fn reject(&mut self, reason: RejectReason) {
        self.reject_reason = Some(reason);
    }
}

impl Drop for RequestMetrics {
    fn drop(&mut self) {
        let Some(metrics) = &self.metrics else {
            return;
        };

        let scope = match self.reject_reason {
            Some(reason) if reason.scope_is_untrusted() => UNKNOWN_SCOPE,
            _ => self.scope.as_str(),
        };

        metrics.requests_total.with_label_values(&[scope]).inc();
        metrics
            .request_duration_seconds
            .with_label_values(&[scope])
            .observe(self.started_at.elapsed().as_secs_f64());

        if let Some(reason) = self.reject_reason {
            metrics
                .requests_rejected_total
                .with_label_values(&[scope, reason.as_str()])
                .inc();
        }
    }
}

#[derive(Clone)]
struct MetricsState {
    metrics: CoordinatorMetrics,
    workers: Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>>,
    healthy: Arc<RwLock<HashSet<String>>>,
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<MetricsState>,
) -> impl IntoResponse {
    state
        .metrics
        .refresh_cluster_gauges(&state.workers, &state.healthy)
        .await;

    let encoder = TextEncoder::new();
    let metric_families = state.metrics.registry.gather();

    let mut buffer = vec![];
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => (StatusCode::OK, buffer),
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gather_value(metrics: &CoordinatorMetrics, name: &str, labels: &[(&str, &str)]) -> f64 {
        for family in metrics.registry.gather() {
            if family.get_name() != name {
                continue;
            }
            for m in family.get_metric() {
                let matches = labels.iter().all(|(k, v)| {
                    m.get_label()
                        .iter()
                        .any(|l| l.get_name() == *k && l.get_value() == *v)
                });
                if matches {
                    return m.get_counter().get_value();
                }
            }
        }
        0.0
    }

    #[test]
    fn records_on_drop_without_rejection() {
        let metrics = CoordinatorMetrics::new().unwrap();
        drop(RequestMetrics::new(Some(metrics.clone()), "s1"));

        assert_eq!(
            gather_value(
                &metrics,
                "browser_hive_coordinator_requests_total",
                &[("scope", "s1")]
            ),
            1.0
        );
        assert_eq!(
            gather_value(
                &metrics,
                "browser_hive_coordinator_requests_rejected_total",
                &[("scope", "s1")]
            ),
            0.0
        );
    }

    #[test]
    fn records_rejection_reason() {
        let metrics = CoordinatorMetrics::new().unwrap();
        let mut guard = RequestMetrics::new(Some(metrics.clone()), "s1");
        guard.reject(RejectReason::NoSlots);
        drop(guard);

        assert_eq!(
            gather_value(
                &metrics,
                "browser_hive_coordinator_requests_rejected_total",
                &[("scope", "s1"), ("reason", "no_slots")]
            ),
            1.0
        );
    }

    // An unknown scope comes straight from client input; it must not create a new time series.
    #[test]
    fn unknown_scope_is_collapsed() {
        let metrics = CoordinatorMetrics::new().unwrap();
        let mut guard = RequestMetrics::new(Some(metrics.clone()), "typo-from-client");
        guard.reject(RejectReason::ScopeNotFound);
        drop(guard);

        assert_eq!(
            gather_value(
                &metrics,
                "browser_hive_coordinator_requests_rejected_total",
                &[("scope", UNKNOWN_SCOPE), ("reason", "scope_not_found")]
            ),
            1.0
        );
        assert_eq!(
            gather_value(
                &metrics,
                "browser_hive_coordinator_requests_total",
                &[("scope", "typo-from-client")]
            ),
            0.0
        );
    }

    #[test]
    fn disabled_metrics_are_a_noop() {
        let mut guard = RequestMetrics::new(None, "s1");
        guard.reject(RejectReason::NoSlots);
        drop(guard);
    }
}
