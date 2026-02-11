use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::{Encoder, IntGaugeVec, Opts, Registry, TextEncoder};
use std::sync::Arc;

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,
    pub total_contexts: IntGaugeVec,
    pub active_contexts: IntGaugeVec,
    pub available_slots: IntGaugeVec,
    pub requests_total: IntGaugeVec,
    pub requests_failed: IntGaugeVec,
}

impl Metrics {
    pub fn new(scope_name: &str) -> anyhow::Result<Self> {
        let registry = Arc::new(Registry::new());

        // Total browser contexts
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

        // Available slots (tabs)
        let available_slots = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_available_slots",
                "Number of available browser tab slots",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(available_slots.clone()))?;

        // Total requests processed
        let requests_total = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_requests_total",
                "Total number of scraping requests processed",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        // Failed requests
        let requests_failed = IntGaugeVec::new(
            Opts::new(
                "browser_hive_worker_requests_failed",
                "Total number of failed scraping requests",
            ),
            &["scope"],
        )?;
        registry.register(Box::new(requests_failed.clone()))?;

        // Initialize all metrics with scope label
        total_contexts.with_label_values(&[scope_name]).set(0);
        active_contexts.with_label_values(&[scope_name]).set(0);
        available_slots.with_label_values(&[scope_name]).set(0);
        requests_total.with_label_values(&[scope_name]).set(0);
        requests_failed.with_label_values(&[scope_name]).set(0);

        Ok(Self {
            registry,
            total_contexts,
            active_contexts,
            available_slots,
            requests_total,
            requests_failed,
        })
    }

    /// Start HTTP server for Prometheus metrics on specified port
    pub async fn start_server(self, port: u16) -> anyhow::Result<()> {
        let app = Router::new().route("/metrics", get(metrics_handler));

        let app = app.with_state(self);

        let addr = format!("0.0.0.0:{}", port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;

        tracing::info!("Metrics server listening on {}", addr);

        axum::serve(listener, app).await?;

        Ok(())
    }
}

async fn metrics_handler(
    axum::extract::State(metrics): axum::extract::State<Metrics>,
) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = metrics.registry.gather();

    let mut buffer = vec![];
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => (StatusCode::OK, buffer),
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Vec::new())
        }
    }
}
