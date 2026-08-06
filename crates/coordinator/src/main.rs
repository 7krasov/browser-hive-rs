mod local_worker_discovery;
mod metrics;
mod service;
mod worker_discovery;

use anyhow::Result;
use browser_hive_common::CoordinatorConfig;
use service::CoordinatorService;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio_cancellation_ext::CancellationToken;
use tonic::transport::Server;
use tracing::{info, warn, Instrument};

// gRPC server timeout - maximum time for a single request
const GRPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(320);

async fn shutdown_signal(active_requests: Arc<AtomicUsize>, cancellation_token: CancellationToken) {
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
        _ = ctrl_c => {
            warn!("Received Ctrl+C signal");
        },
        _ = terminate => {
            warn!("Received SIGTERM signal");
        },
    }

    // Cancel all ongoing operations
    info!("Cancelling all active operations...");
    cancellation_token.cancel();

    let active_count = active_requests.load(Ordering::SeqCst);
    if active_count > 0 {
        info!(
            "Starting graceful shutdown, waiting for {} active request(s) to complete...",
            active_count
        );

        // Wait for all requests to complete
        loop {
            let remaining = active_requests.load(Ordering::SeqCst);
            if remaining == 0 {
                break;
            }
            info!("Waiting for {} request(s) to complete...", remaining);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        info!("All active requests completed");
    } else {
        info!("Starting graceful shutdown, no active requests");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing (format via LOG_FORMAT, level via RUST_LOG)
    browser_hive_common::init_logging()?;

    run_coordinator().await
}

// Process-lifetime span for everything that is not request-scoped: startup, the serving future,
// signal handling and shutdown all run inside it. There is no `scope` field — one coordinator
// serves every scope — but `ray_id` carries a sentinel so every line stays filterable by
// `span_ray_id`, and `span_ray_id!~"^ray_"` isolates the non-request lines.
//
// The body lives here rather than in `main` because a span must be applied to a future
// (`.instrument`), never entered with a guard held across `.await`: the guard is thread-local,
// so a request task polled on the same thread meanwhile would inherit this span.
#[tracing::instrument(
    name = "coordinator_lifecycle",
    fields(ray_id = "coordinator-lifecycle")
)]
async fn run_coordinator() -> Result<()> {
    // Logged before anything can fail, so the running revision is always identifiable.
    // The coordinator image is built from base `main` unpinned, so this is the only
    // in-process record of which build is actually serving.
    // Single literal, so the same pattern finds the version in the logs and in the binary:
    //   grep -a -o 'Browser Hive library version=[0-9.]*' /usr/local/bin/coordinator
    info!(
        "{}",
        concat!("Browser Hive library version=", env!("CARGO_PKG_VERSION"))
    );

    // Load configuration. Parse failures fall back to defaults but are never silent.
    let (config, config_warnings) = CoordinatorConfig::from_env();
    for warning in &config_warnings {
        warn!("Coordinator config: {}", warning);
    }

    info!("Starting Coordinator service on port {}", config.grpc_port);

    // Create cancellation token for graceful shutdown
    let cancellation_token = CancellationToken::new();

    // Create coordinator service
    let coordinator_service =
        CoordinatorService::new(config.clone(), cancellation_token.clone()).await?;

    // Get active requests counter for shutdown monitoring
    let active_requests = coordinator_service.active_requests();

    // Prometheus metrics server. Long-lived background task, so it opens its own span with a
    // sentinel ray_id (a spawned task does not inherit the lifetime span).
    if let Some(metrics) = coordinator_service.metrics() {
        let (workers, healthy) = coordinator_service.fleet_view();
        let metrics_port = config.metrics_port;
        let span = tracing::info_span!("metrics_server", ray_id = "metrics-server");
        tokio::spawn(
            async move {
                if let Err(e) = metrics.start_server(metrics_port, workers, healthy).await {
                    warn!("Metrics server error: {}", e);
                }
            }
            .instrument(span),
        );
    }

    // Start gRPC server
    let addr: SocketAddr = format!("0.0.0.0:{}", config.grpc_port)
        .parse()
        .expect("Invalid address");

    info!("Coordinator gRPC server listening on {}", addr);

    Server::builder()
        .timeout(GRPC_REQUEST_TIMEOUT)
        .add_service(
            browser_hive_proto::coordinator::scraper_coordinator_server::ScraperCoordinatorServer::new(
                coordinator_service,
            ),
        )
        .serve_with_shutdown(addr, shutdown_signal(active_requests.clone(), cancellation_token.clone()))
        .await?;

    let remaining = active_requests.load(Ordering::SeqCst);
    if remaining == 0 {
        info!("All active requests completed, terminating gracefully");
    } else {
        warn!(
            "Terminating with {} active request(s) still running",
            remaining
        );
    }

    info!("Coordinator shutdown complete");
    Ok(())
}
