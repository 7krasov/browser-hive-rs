mod browser_pool;
mod metrics;
mod service;

pub mod providers;

pub use browser_pool::BrowserPool;
pub use metrics::Metrics;
pub use service::WorkerService;

use anyhow::Result;
use browser_hive_common::WorkerConfig;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio_cancellation_ext::CancellationToken;
use tonic::transport::Server;
use tracing::{info, warn};

const GRPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(320);

/// Run the worker service with given configuration
///
/// This is the main entry point for running a Browser Hive worker.
/// Users provide a WorkerConfig with their custom ProxyProvider implementation.
///
/// # Example
///
/// ```rust,ignore
/// use browser_hive_worker::run_worker;
/// use browser_hive_common::{WorkerConfig, ScopeConfig, ContextLifecycleConfig, SessionMode, ProxyProvider};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let provider = MyProxyProvider::new(/* config */);
///
///     let config = WorkerConfig {
///         scope: ScopeConfig {
///             name: "my_scope".to_string(),
///             proxy_provider: Box::new(provider),
///             min_contexts: 2,
///             max_contexts: 10,
///             session_mode: SessionMode::ReusablePreinit, // Pre-initialize contexts on startup
///             headless: true,
///             lifecycle: ContextLifecycleConfig::default(),
///             enable_browser_diagnostics: false, // Disable for faster response
///             binary_params_middlewares: vec![],
///             tab_init_middlewares: vec![],
///             context_isolation: ContextIsolation::Isolated,
///         },
///         grpc_port: 50052,
///         pod_name: "worker-1".to_string(),
///         pod_ip: "0.0.0.0".to_string(),
///     };
///
///     run_worker(config).await
/// }
/// ```
pub async fn run_worker(config: WorkerConfig) -> Result<()> {
    info!(
        "Starting worker for scope: {} on {}:{} (session_mode: {:?})",
        config.scope.name, config.pod_ip, config.grpc_port, config.scope.session_mode
    );

    // Create cancellation token for graceful shutdown
    let cancellation_token = CancellationToken::new();

    let metrics = Metrics::new(&config.scope.name)?;
    let worker_service =
        WorkerService::new(config.clone(), metrics.clone(), cancellation_token.clone()).await?;
    let active_requests = worker_service.active_requests();
    let is_ready = worker_service.is_ready_flag();

    // Start metrics HTTP server in background
    let metrics_port = 9090;
    let metrics_handle = tokio::spawn(async move {
        if let Err(e) = metrics.start_server(metrics_port).await {
            tracing::error!("Metrics server error: {}", e);
        }
    });

    // Start gRPC server
    let addr: SocketAddr = format!("{}:{}", config.pod_ip, config.grpc_port)
        .parse()
        .expect("Invalid address");

    info!("Worker gRPC server listening on {}", addr);

    // Run both servers with graceful shutdown
    tokio::select! {
        result = Server::builder()
            .timeout(GRPC_REQUEST_TIMEOUT)
            .add_service(
                browser_hive_proto::worker::worker_service_server::WorkerServiceServer::new(
                    worker_service,
                ),
            )
            .serve_with_shutdown(addr, shutdown_signal(active_requests.clone(), is_ready.clone(), cancellation_token.clone())) => {
            result?;
            info!("gRPC server shutdown complete");
        }
        _ = metrics_handle => {
            info!("Metrics server stopped");
        }
    }

    let remaining = active_requests.load(Ordering::SeqCst);
    if remaining == 0 {
        info!("All active requests completed, terminating gracefully");
    } else {
        warn!(
            "Terminating with {} active request(s) still running",
            remaining
        );
    }

    info!("Worker shutdown complete");
    Ok(())
}

async fn shutdown_signal(
    active_requests: Arc<AtomicUsize>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
    cancellation_token: CancellationToken,
) {
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

    // Mark as not ready for K8s readiness probe
    is_ready.store(false, Ordering::SeqCst);
    info!("Worker marked as not ready (readiness probe will fail)");

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
