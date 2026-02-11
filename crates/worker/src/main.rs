// Example worker binary for docker-compose
//
// This is a minimal example showing how to use browser-hive-worker library.
// Real users should create their own binary in their private repository
// with their custom proxy provider implementations.

mod providers;

use anyhow::Result;
use browser_hive_worker::run_worker;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Create proxy provider from environment variables
    let provider = providers::create_from_env()?;

    // Load configuration from environment
    let config = load_config_from_env(provider)?;

    // Run worker
    run_worker(config).await
}

fn load_config_from_env(
    proxy_provider: Box<dyn browser_hive_common::ProxyProvider>,
) -> Result<browser_hive_common::WorkerConfig> {
    use browser_hive_common::{
        ContextIsolation, ContextLifecycleConfig, DefaultBinaryParamsMiddleware, ScopeConfig,
        SessionMode, WorkerConfig,
    };
    use std::env;
    use std::path::PathBuf;
    use std::time::Duration;

    let scope_name = env::var("WORKER_SCOPE_NAME").unwrap_or_else(|_| "local_dev".to_string());
    let grpc_port = env::var("WORKER_GRPC_PORT")
        .unwrap_or_else(|_| "50052".to_string())
        .parse::<u16>()?;
    let pod_name = env::var("POD_NAME").unwrap_or_else(|_| "worker-local".to_string());
    let pod_ip = env::var("POD_IP").unwrap_or_else(|_| "0.0.0.0".to_string());

    // Maximum concurrent browser contexts per worker
    let max_contexts = env::var("WORKER_MAX_CONTEXTS")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(3);

    let min_contexts = env::var("WORKER_MIN_CONTEXTS")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(max_contexts); // Default: min = max (for ReusablePreinit)

    // Session mode: "always_new", "reusable", or "reusable_preinit"
    // Default is defined by SessionMode's #[default] attribute (Reusable)
    let session_mode: SessionMode = env::var("WORKER_SESSION_MODE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let headless = env::var("WORKER_HEADLESS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(true);

    let enable_browser_diagnostics = env::var("WORKER_ENABLE_BROWSER_DIAGNOSTICS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(false); // Default: disabled for faster response

    // Context isolation mode: "shared" or "isolated"
    // Default is defined by ContextIsolation's #[default] attribute (Isolated)
    let context_isolation: ContextIsolation = env::var("WORKER_CONTEXT_ISOLATION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    // Custom browser path (e.g., /usr/bin/brave-browser for Brave)
    // If not set, uses default Chrome/Chromium auto-detection
    let browser_path: Option<PathBuf> = env::var("WORKER_BROWSER_PATH").ok().map(PathBuf::from);

    // Lifecycle configuration
    let max_idle_time_secs = env::var("WORKER_MAX_IDLE_TIME_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5 * 60); // Default: 5 minutes

    // Create default binary params middleware
    // Users in production can replace this with custom implementations
    let binary_params_middlewares: Vec<
        Box<dyn browser_hive_common::BrowserBinaryParamsMiddleware>,
    > = vec![Box::new(DefaultBinaryParamsMiddleware)];

    // Create default tab init middlewares
    // Users can add custom middleware here (timezone override, WebGL spoofing, etc.)
    let tab_init_middlewares: Vec<Box<dyn browser_hive_common::TabInitMiddleware>> =
        vec![Box::new(
            browser_hive_common::DefaultTabInitMiddleware::new(headless),
        )];

    let scope_config = ScopeConfig {
        name: scope_name,
        proxy_provider,
        min_contexts,
        max_contexts,
        session_mode,
        headless,
        lifecycle: ContextLifecycleConfig {
            max_lifetime: Duration::from_secs(6 * 3600), // 6 hours
            max_requests: 10_000,
            max_cache_size_mb: 500,
            max_idle_time: Duration::from_secs(max_idle_time_secs),
            rotation_strategy: browser_hive_common::RotationStrategy::Hybrid,
        },
        browser_path,
        enable_browser_diagnostics,
        binary_params_middlewares,
        tab_init_middlewares,
        context_isolation,
    };

    Ok(WorkerConfig {
        scope: scope_config,
        grpc_port,
        pod_name,
        pod_ip,
    })
}
