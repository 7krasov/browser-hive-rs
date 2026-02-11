mod local_worker_discovery;
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
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

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
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Load configuration
    let config = CoordinatorConfig::default();

    info!("Starting Coordinator service on port {}", config.grpc_port);

    // Create cancellation token for graceful shutdown
    let cancellation_token = CancellationToken::new();

    // Create coordinator service
    let coordinator_service =
        CoordinatorService::new(config.clone(), cancellation_token.clone()).await?;

    // Get active requests counter for shutdown monitoring
    let active_requests = coordinator_service.active_requests();

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
