use crate::browser_pool::BrowserPool;
use crate::browser_resources::{self, BrowserTargetProbe, PROCESS_TYPES, TARGET_TYPES};
use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// How long one scrape waits for the browser snapshot before exposing the gauges without it.
///
/// A probe that runs past this keeps going on its blocking thread and the next scrape skips it
/// (see `BrowserTargetProbe::snapshot`), so a wedged browser costs one thread and absent series,
/// never a `/metrics` that stops answering.
const BROWSER_SNAPSHOT_BUDGET: Duration = Duration::from_secs(3);

fn register_gauge(
    registry: &Registry,
    name: &str,
    help: &str,
    labels: &[&str],
) -> anyhow::Result<IntGaugeVec> {
    let gauge = IntGaugeVec::new(Opts::new(name, help), labels)?;
    registry.register(Box::new(gauge.clone()))?;
    Ok(gauge)
}

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,
    pub total_contexts: IntGaugeVec,
    pub active_contexts: IntGaugeVec,
    pub claimed_contexts: IntGaugeVec,
    pub available_slots: IntGaugeVec,
    pub total_slots: IntGaugeVec,
    pub requests_total: IntCounterVec,
    pub requests_failed: IntCounterVec,
    pub request_duration_seconds: HistogramVec,
    browser_processes: IntGaugeVec,
    browser_process_pss_bytes: IntGaugeVec,
    browser_process_max_pss_bytes: IntGaugeVec,
    browser_targets: IntGaugeVec,
    browser_contexts: IntGaugeVec,
    process_resident_memory_bytes: IntGaugeVec,
    process_threads: IntGaugeVec,
    target_probe: Arc<BrowserTargetProbe>,
    scope_name: String,
}

impl Metrics {
    pub fn new(scope_name: &str) -> anyhow::Result<Self> {
        let registry = Arc::new(Registry::new());

        // Total browser contexts currently in the pool (created on demand,
        // may be lower than total_slots)
        let total_contexts = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_total_contexts",
                "Total number of browser contexts in the pool",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(total_contexts.clone()))?;

        // Active (busy) browser contexts
        let active_contexts = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_active_contexts",
                "Number of browser contexts currently processing requests",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(active_contexts.clone()))?;

        // Slots that cannot be given to a new client. Added alongside active_contexts rather
        // than replacing it, since dashboards and KEDA triggers already reference that gauge.
        //
        // This is the gauge to autoscale on, in every mode. `active_contexts` counts contexts
        // that are busy *right now*, and in `dedicated` a slot held by a session that is idle
        // between two requests is not busy — an autoscaler reading active/total_slots would see
        // an empty worker whose every slot is spoken for. Claimed equals active in the other
        // modes, so one trigger works everywhere. See METRICS.md.
        let claimed_contexts = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_claimed_contexts",
                "Number of slots that cannot be handed to a new client (busy contexts, plus \
                 idle-but-owned session contexts in dedicated mode)",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(claimed_contexts.clone()))?;

        // Available slots (max_contexts - claimed contexts)
        let available_slots = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_available_slots",
                "Number of available browser tab slots",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(available_slots.clone()))?;

        // Maximum capacity (configured max_contexts) - denominator for utilization
        let total_slots = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_total_slots",
                "Maximum number of concurrent browser contexts (configured capacity)",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(total_slots.clone()))?;

        // Total requests processed
        let requests_total = IntCounterVec::new(
            Opts::new(
                "browser_hive_worker_requests_total",
                "Total number of scraping requests processed",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        // Failed requests
        let requests_failed = IntCounterVec::new(
            Opts::new(
                "browser_hive_worker_requests_failed",
                "Total number of failed scraping requests",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(requests_failed.clone()))?;

        // Request duration histogram - end-to-end scrape_page latency in seconds.
        // Enables robust concurrency sizing via Little's Law
        // (sum(rate(_sum)) = average in-flight requests, immune to scrape sampling)
        // and latency SLOs (p50/p95/p99 via histogram_quantile).
        // Buckets are tuned for browser scraping (sub-second to ~1 minute).
        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "browser_hive_worker_request_duration_seconds",
                "End-to-end scrape request duration in seconds",
            )
            .buckets(vec![
                0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0, 34.0, 60.0,
            ]),
            &["scope"],
        )?;
        registry.register(Box::new(request_duration_seconds.clone()))?;

        // Browser resources. The memory limit is spent by the browser's processes, not by the
        // worker, and container memory is only their sum — these split it by process type and
        // count what the browser holds. Collected on scrape; see `browser_resources.rs`.
        let browser_processes = register_gauge(
            &registry,
            "browser_hive_worker_browser_processes",
            "Browser processes descending from the worker, by Chromium process type",
            &["scope", "type"],
        )?;
        let browser_process_pss_bytes = register_gauge(
            &registry,
            "browser_hive_worker_browser_process_pss_bytes",
            "Proportional set size of browser processes, summed by process type",
            &["scope", "type"],
        )?;
        let browser_process_max_pss_bytes = register_gauge(
            &registry,
            "browser_hive_worker_browser_process_max_pss_bytes",
            "Proportional set size of the largest browser process of each type",
            &["scope", "type"],
        )?;
        let browser_targets = register_gauge(
            &registry,
            "browser_hive_worker_browser_targets",
            "CDP targets reported by the browser, by target type",
            &["scope", "type"],
        )?;
        let browser_contexts = register_gauge(
            &registry,
            "browser_hive_worker_browser_contexts",
            "CDP browser contexts that exist in the browser, the default context excluded",
            &["scope"],
        )?;
        let process_resident_memory_bytes = register_gauge(
            &registry,
            "browser_hive_worker_process_resident_memory_bytes",
            "Resident set size of the worker process itself (the browser excluded)",
            &["scope"],
        )?;
        let process_threads = register_gauge(
            &registry,
            "browser_hive_worker_process_threads",
            "Threads of the worker process",
            &["scope"],
        )?;

        // Initialize all metrics with scope label so they are exposed immediately
        total_contexts.with_label_values(&[scope_name]).set(0);
        active_contexts.with_label_values(&[scope_name]).set(0);
        claimed_contexts.with_label_values(&[scope_name]).set(0);
        available_slots.with_label_values(&[scope_name]).set(0);
        total_slots.with_label_values(&[scope_name]).set(0);
        requests_total.with_label_values(&[scope_name]);
        requests_failed.with_label_values(&[scope_name]);
        request_duration_seconds.with_label_values(&[scope_name]);

        Ok(Self {
            registry,
            total_contexts,
            active_contexts,
            claimed_contexts,
            available_slots,
            total_slots,
            requests_total,
            requests_failed,
            request_duration_seconds,
            browser_processes,
            browser_process_pss_bytes,
            browser_process_max_pss_bytes,
            browser_targets,
            browser_contexts,
            process_resident_memory_bytes,
            process_threads,
            target_probe: Arc::new(BrowserTargetProbe::default()),
            scope_name: scope_name.to_string(),
        })
    }

    /// Refresh browser resource gauges from `/proc` and from the browser's CDP endpoint.
    ///
    /// Every type of the closed label sets is written, zero included, so a type does not vanish
    /// from a graph when its last process exits. A source that could not be read leaves its gauges
    /// **absent** instead: a stale or zero value would read as a fact about the browser.
    async fn refresh_browser_gauges(&self, browser_pool: &Arc<RwLock<BrowserPool>>) {
        // Only the endpoint leaves the lock: holding the `Arc<Browser>` across the probe would keep
        // a replaced browser process alive until the probe finished.
        let ws_url = browser_pool.read().await.get_browser().get_ws_url();

        let probe = self.target_probe.clone();
        let (processes, targets) = tokio::join!(
            within_budget(tokio::task::spawn_blocking(
                browser_resources::collect_processes
            )),
            within_budget(tokio::task::spawn_blocking(move || probe.snapshot(&ws_url))),
        );
        let processes = processes.unwrap_or_default();
        let targets = targets.and_then(|result| {
            result
                .map_err(|e| tracing::debug!("Browser target probe failed: {e:#}"))
                .ok()
        });

        let scope = self.scope_name.as_str();
        let by_type = [
            &self.browser_processes,
            &self.browser_process_pss_bytes,
            &self.browser_process_max_pss_bytes,
        ];
        match processes.browser {
            Some(groups) => {
                for process_type in PROCESS_TYPES {
                    let group = groups.get(process_type).copied().unwrap_or_default();
                    let labels = [scope, process_type];
                    let values = [group.count, group.pss_bytes, group.max_pss_bytes];
                    for (gauge, value) in by_type.iter().zip(values) {
                        gauge.with_label_values(&labels).set(value as i64);
                    }
                }
            }
            None => by_type.iter().for_each(|gauge| gauge.reset()),
        }

        match targets {
            Some(snapshot) => {
                for target_type in TARGET_TYPES {
                    let count = snapshot.targets.get(target_type).copied().unwrap_or(0);
                    self.browser_targets
                        .with_label_values(&[scope, target_type])
                        .set(count as i64);
                }
                self.browser_contexts
                    .with_label_values(&[scope])
                    .set(snapshot.contexts as i64);
            }
            None => {
                self.browser_targets.reset();
                self.browser_contexts.reset();
            }
        }

        match processes.worker {
            Some(worker) => {
                self.process_resident_memory_bytes
                    .with_label_values(&[scope])
                    .set(worker.resident_bytes as i64);
                self.process_threads
                    .with_label_values(&[scope])
                    .set(worker.threads as i64);
            }
            None => {
                self.process_resident_memory_bytes.reset();
                self.process_threads.reset();
            }
        }
    }

    /// Refresh pool gauges from the current browser pool state.
    ///
    /// Called on every Prometheus scrape so gauges always reflect live pool
    /// state regardless of which code path changed it (requests, lifecycle
    /// recycling, pool recreation).
    async fn refresh_pool_gauges(&self, browser_pool: &Arc<RwLock<BrowserPool>>) {
        let stats = {
            let pool = browser_pool.read().await;
            pool.get_stats().await
        };

        let scope = [self.scope_name.as_str()];
        self.total_contexts
            .with_label_values(&scope)
            .set(stats.total_contexts as i64);
        self.active_contexts
            .with_label_values(&scope)
            .set(stats.active_requests as i64);
        self.claimed_contexts
            .with_label_values(&scope)
            .set(stats.claimed_contexts as i64);
        self.available_slots
            .with_label_values(&scope)
            .set(stats.available_slots as i64);
        self.total_slots
            .with_label_values(&scope)
            .set(stats.total_slots as i64);
    }

    /// Start HTTP server for Prometheus metrics on specified port
    pub async fn start_server(
        self,
        port: u16,
        browser_pool: Arc<RwLock<BrowserPool>>,
    ) -> anyhow::Result<()> {
        let state = MetricsState {
            metrics: self,
            browser_pool,
        };

        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;

        tracing::info!("Metrics server listening on {}", addr);

        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Result of a blocking collector, or `None` when it panicked or ran past
/// [`BROWSER_SNAPSHOT_BUDGET`].
async fn within_budget<T>(handle: tokio::task::JoinHandle<T>) -> Option<T> {
    match tokio::time::timeout(BROWSER_SNAPSHOT_BUDGET, handle).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            tracing::warn!("Browser resource collector failed: {e}");
            None
        }
        Err(_) => {
            tracing::debug!("Browser resource collector exceeded its budget");
            None
        }
    }
}

#[derive(Clone)]
struct MetricsState {
    metrics: Metrics,
    browser_pool: Arc<RwLock<BrowserPool>>,
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<MetricsState>,
) -> impl IntoResponse {
    tokio::join!(
        state.metrics.refresh_pool_gauges(&state.browser_pool),
        state.metrics.refresh_browser_gauges(&state.browser_pool),
    );

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
