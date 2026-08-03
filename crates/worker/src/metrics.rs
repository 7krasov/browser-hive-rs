use crate::browser_pool::BrowserPool;
use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::sync::Arc;
use tokio::sync::RwLock;

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
            scope_name: scope_name.to_string(),
        })
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

#[derive(Clone)]
struct MetricsState {
    metrics: Metrics,
    browser_pool: Arc<RwLock<BrowserPool>>,
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<MetricsState>,
) -> impl IntoResponse {
    state.metrics.refresh_pool_gauges(&state.browser_pool).await;

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
