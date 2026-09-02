// Example worker binary for docker-compose
//
// This is a minimal example showing how to use browser-hive-worker library.
// Real users should create their own binary in their private repository
// with their custom proxy provider implementations.

mod providers;

use anyhow::{anyhow, Result};
use browser_hive_worker::run_worker;
use std::env;
use std::fmt::Display;
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing (format via LOG_FORMAT, level via RUST_LOG)
    browser_hive_common::init_logging()?;

    // Create proxy provider from environment variables
    let provider = providers::create_from_env()?;

    // Load configuration from environment
    let config = load_config_from_env(provider)?;

    // Run worker
    run_worker(config).await
}

/// Read an optional environment variable, failing loudly on a malformed value.
///
/// An unset variable yields the default; a variable that is set but cannot be parsed aborts
/// startup. The alternative — `.ok().and_then(parse).unwrap_or(default)` — turns a typo into a
/// silent default: the worker comes up with settings nobody chose and nothing in the logs says
/// so. A manifest that sets `WORKER_MAX_CONTEXTS: "ten"` should not run at 3.
fn env_parsed<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    match env::var(key) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e| anyhow!("{key}: invalid value '{}' ({e})", raw.trim())),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(anyhow!("{key}: {e}")),
    }
}

/// Same as [`env_parsed`], for the library's config enums.
///
/// Their `FromStr::Err` is `()`, which carries no message, so the accepted values have to be
/// spelled out by the caller.
fn env_enum<T: FromStr>(key: &str, default: T, allowed: &str) -> Result<T> {
    match env::var(key) {
        Ok(raw) => raw.trim().parse().map_err(|_| {
            anyhow!(
                "{key}: invalid value '{}' (expected one of: {allowed})",
                raw.trim()
            )
        }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(anyhow!("{key}: {e}")),
    }
}

fn load_config_from_env(
    proxy_provider: Box<dyn browser_hive_common::ProxyProvider>,
) -> Result<browser_hive_common::WorkerConfig> {
    use browser_hive_common::{
        ContextIsolation, ContextLifecycleConfig, DefaultBinaryParamsMiddleware, ScopeConfig,
        SessionMode, WorkerConfig,
    };
    use std::path::PathBuf;
    use std::time::Duration;

    let scope_name = env::var("WORKER_SCOPE_NAME").unwrap_or_else(|_| "local_dev".to_string());
    let grpc_port = env_parsed::<u16>("WORKER_GRPC_PORT", 50052)?;
    let pod_name = env::var("POD_NAME").unwrap_or_else(|_| "worker-local".to_string());
    let pod_ip = env::var("POD_IP").unwrap_or_else(|_| "0.0.0.0".to_string());

    // Maximum concurrent browser contexts per worker
    let max_contexts = env_parsed::<u16>("WORKER_MAX_CONTEXTS", 3)?;

    // Session mode: "always_new", "reusable" or "dedicated"
    // Default is defined by SessionMode's #[default] attribute (Reusable)
    let session_mode = env_enum::<SessionMode>(
        "WORKER_SESSION_MODE",
        SessionMode::default(),
        browser_hive_common::SESSION_MODE_VALUES,
    )?;

    // Contexts pre-created on startup. Default 0: pre-initialization is worth its startup cost
    // only in `reusable`, where any request can use a pre-created context, so it is opted into
    // rather than inherited from max_contexts.
    let min_contexts = env_parsed::<u16>("WORKER_MIN_CONTEXTS", 0)?;

    let headless = env_parsed::<bool>("WORKER_HEADLESS", true)?;

    // Browser diagnostics: all knobs in one call so downstream binaries, which have their own
    // main.rs, do not have to re-implement the parsing. Disabled unless
    // WORKER_ENABLE_BROWSER_DIAGNOSTICS is set.
    let diagnostics = browser_hive_common::DiagnosticsConfig::from_env();

    // Context isolation mode: "shared" or "isolated"
    // Default is defined by ContextIsolation's #[default] attribute (Isolated)
    let context_isolation = env_enum::<ContextIsolation>(
        "WORKER_CONTEXT_ISOLATION",
        ContextIsolation::default(),
        "shared, isolated",
    )?;

    // Custom browser path (e.g., /usr/bin/brave-browser for Brave)
    // If not set, uses default Chrome/Chromium auto-detection
    let browser_path: Option<PathBuf> = env::var("WORKER_BROWSER_PATH").ok().map(PathBuf::from);

    // Lifecycle configuration.
    //
    // The default depends on the mode because the timeout means different things. In `reusable`
    // an idle context costs only memory, so five minutes of warm cache is a win. In `dedicated`
    // it is the only thing that ever frees a claimed slot — there is no release RPC — so every
    // abandoned session costs capacity for exactly this long, and the default drops to a minute.
    let default_max_idle_time_secs = if session_mode == SessionMode::Dedicated {
        browser_hive_common::DEFAULT_DEDICATED_MAX_IDLE_TIME_SECS
    } else {
        browser_hive_common::DEFAULT_MAX_IDLE_TIME_SECS
    };
    let max_idle_time_secs =
        env_parsed::<u64>("WORKER_MAX_IDLE_TIME_SECS", default_max_idle_time_secs)?;

    // Release a dedicated session's context as soon as its page comes back 403/429, instead of
    // holding the slot (and the refused exit IP) until the idle timeout.
    //
    // Like max_idle_time above, the default follows the mode: in `dedicated` a blocked context is
    // a slot nobody will ever come back for (the client drops its session id on those statuses and
    // there is no release RPC), so releasing it is the right default; in the other modes the
    // setting does nothing, and defaulting to `true` there would only make validate() warn on
    // every start. An explicit value always wins.
    let destroy_session_on_block = env_parsed::<bool>(
        "WORKER_DESTROY_SESSION_ON_BLOCK",
        session_mode == SessionMode::Dedicated,
    )?;

    // Keep a `reusable` context out of one origin's rotation after that origin refused it with
    // 403/429, instead of handing the same (refused) exit IP to the next request for that site.
    //
    // Like the two settings above, the default follows the mode: only `reusable` picks between
    // contexts, so only there can a quarantine reroute anything — in `dedicated` the context is
    // addressed by its session id (and `destroy_session_on_block` above is the right tool), and in
    // `always_new` it dies with the request. Defaulting to non-zero elsewhere would only make
    // validate() warn on every start. `WORKER_BLOCK_QUARANTINE_SECS=0` disables it.
    let default_block_quarantine_secs = if session_mode == SessionMode::Reusable {
        browser_hive_common::DEFAULT_BLOCK_QUARANTINE_SECS
    } else {
        0
    };
    let block_quarantine = Duration::from_secs(env_parsed::<u64>(
        "WORKER_BLOCK_QUARANTINE_SECS",
        default_block_quarantine_secs,
    )?);

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
        diagnostics,
        binary_params_middlewares,
        tab_init_middlewares,
        context_isolation,
        destroy_session_on_block,
        block_quarantine,
    };

    Ok(WorkerConfig {
        scope: scope_config,
        grpc_port,
        pod_name,
        pod_ip,
    })
}
