use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

/// Runtime scope configuration with proxy provider and browser middlewares
///
/// Note: This struct cannot derive Serialize/Deserialize because
/// Box<dyn ProxyProvider> and middleware trait objects are not serializable.
/// Use environment variables or build this config programmatically in your application code.
#[derive(Clone)]
pub struct ScopeConfig {
    pub name: String,
    pub proxy_provider: Box<dyn crate::proxy::ProxyProvider>,
    pub min_contexts: u16, // Minimum contexts to pre-initialize on startup (if session_mode=ReusablePreinit)
    pub max_contexts: u16, // Maximum number of browser contexts allowed
    pub session_mode: SessionMode, // Controls context creation, reuse, and destruction behavior
    pub headless: bool, // true = headless (faster, detectable), false = headfull (slower, better stealth)
    // Note: We use 1 tab per context (hardcoded) for simplicity and reliability
    pub lifecycle: ContextLifecycleConfig,
    /// Path to browser binary. If None, uses default Chrome/Chromium auto-detection.
    /// Use this to specify alternative browsers like Brave: `/usr/bin/brave-browser`
    pub browser_path: Option<PathBuf>,
    /// Enable browser diagnostics (console logs, page diagnostics) after scraping
    /// When enabled, diagnostics are only collected if there's enough time remaining (>2 sec)
    /// Default: false (faster response, recommended for production)
    pub enable_browser_diagnostics: bool,

    // Browser customization middlewares
    /// Middlewares for Chrome binary parameters (executed BEFORE browser launch)
    /// Order matters: middlewares are applied in sequence, each can add/modify args
    pub binary_params_middlewares:
        Vec<Box<dyn crate::browser_middleware::BrowserBinaryParamsMiddleware>>,

    /// Middlewares for tab initialization (executed AFTER each tab creation, before any navigation)
    /// Order matters: middlewares are applied in sequence to each new tab
    pub tab_init_middlewares: Vec<Box<dyn crate::browser_middleware::TabInitMiddleware>>,

    /// Context isolation mode - controls how browser contexts share state
    ///
    /// - `Shared`: All contexts share cookies/storage within the same Chrome process (fastest)
    /// - `Isolated`: Each context gets its own isolated cookies/storage via CDP BrowserContext (default)
    ///
    /// Use `Isolated` when you need true separation between requests (e.g., different user sessions,
    /// avoiding cookie contamination between scraping tasks).
    ///
    /// Note: Even with `Shared` mode, different Chrome processes (workers) are always isolated.
    /// This setting only affects isolation WITHIN a single worker's Chrome process.
    pub context_isolation: ContextIsolation,
}

/// Controls how browser contexts share state within a Chrome process
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextIsolation {
    /// All contexts share cookies, localStorage, sessionStorage, and cache.
    /// Fastest option - contexts are just tabs in the default browser context.
    /// Use when cookie sharing is acceptable or desired (e.g., Reusable session mode).
    Shared,

    /// Each context gets isolated cookies, localStorage, sessionStorage, and cache.
    /// Uses CDP BrowserContext (similar to incognito mode) for true isolation.
    /// Slightly slower to create (~50-200ms vs ~10-50ms for Shared).
    /// Use when you need guaranteed separation between requests.
    #[default]
    Isolated,
}

impl FromStr for ContextIsolation {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "shared" => Ok(Self::Shared),
            "isolated" => Ok(Self::Isolated),
            _ => Err(()),
        }
    }
}

/// Controls how browser contexts are created and managed.
///
/// | Mode | Reusable? | Behavior |
/// |------|-----------|----------|
/// | `AlwaysNew` | No | Fresh context per request, destroyed after |
/// | `Reusable` | Yes | Contexts created on-demand, reused until recycled |
/// | `ReusablePreinit` | Yes | Same as Reusable, but pre-created on startup |
///
/// **Note on Reusable modes**: Contexts are NOT kept forever. They are automatically
/// recycled by the lifecycle monitor based on `ContextLifecycleConfig` settings:
/// - `max_idle_time` (default: 5 min) - recycled after being idle
/// - `max_lifetime` (default: 6 hours) - recycled after this age
/// - `max_requests` (default: 10,000) - recycled after this many requests
/// - `max_cache_size_mb` (default: 500 MB) - recycled when cache grows too large
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    /// Create a new context for each request, destroy after completion.
    /// - Contexts are created on-demand
    /// - Each request gets a fresh, isolated browser state
    /// - Context is destroyed immediately after the request completes
    /// - Best for: one-shot scraping, avoiding session/cookie contamination
    /// - Session IDs are NOT returned to clients (cannot be reused)
    AlwaysNew,

    /// Reusable contexts with automatic lifecycle management.
    /// - Contexts are created on-demand up to `max_contexts`
    /// - Idle contexts are reused for subsequent requests
    /// - Contexts persist cookies/storage between requests (session affinity)
    /// - Contexts are recycled based on `ContextLifecycleConfig` (idle time, age, requests, cache size)
    /// - Best for: session-based scraping, login flows, multi-page workflows
    /// - Session IDs ARE returned to clients for reuse
    #[default]
    Reusable,

    /// Reusable contexts with pre-initialization on startup.
    /// - Creates `min_contexts` contexts during worker initialization
    /// - Contexts are reused and recycled like `Reusable` mode
    /// - Faster first-request latency (no context creation overhead)
    /// - Best for: high-throughput scenarios where startup time is acceptable
    /// - Session IDs ARE returned to clients for reuse
    ReusablePreinit,
}

impl FromStr for SessionMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "always_new" => Ok(Self::AlwaysNew),
            "reusable" => Ok(Self::Reusable),
            "reusable_preinit" => Ok(Self::ReusablePreinit),
            _ => Err(()),
        }
    }
}

impl std::fmt::Debug for ScopeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let binary_middleware_names: Vec<&str> = self
            .binary_params_middlewares
            .iter()
            .map(|m| m.name())
            .collect();

        let tab_init_middleware_names: Vec<&str> =
            self.tab_init_middlewares.iter().map(|m| m.name()).collect();

        f.debug_struct("ScopeConfig")
            .field("name", &self.name)
            .field("proxy_provider", &self.proxy_provider.name())
            .field("min_contexts", &self.min_contexts)
            .field("max_contexts", &self.max_contexts)
            .field("session_mode", &self.session_mode)
            .field("headless", &self.headless)
            .field("lifecycle", &self.lifecycle)
            .field("browser_path", &self.browser_path)
            .field(
                "enable_browser_diagnostics",
                &self.enable_browser_diagnostics,
            )
            .field("binary_params_middlewares", &binary_middleware_names)
            .field("tab_init_middlewares", &tab_init_middleware_names)
            .field("context_isolation", &self.context_isolation)
            .finish()
    }
}

// ProxyProvider trait and ProxyConfig are now in crate::proxy module

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLifecycleConfig {
    #[serde(with = "humantime_serde")]
    pub max_lifetime: Duration,
    pub max_requests: u64,
    pub max_cache_size_mb: u64,
    #[serde(with = "humantime_serde")]
    pub max_idle_time: Duration,
    pub rotation_strategy: RotationStrategy,
}

impl Default for ContextLifecycleConfig {
    fn default() -> Self {
        Self {
            max_lifetime: Duration::from_secs(6 * 3600), // 6 hours
            max_requests: 10_000,
            max_cache_size_mb: 500,
            max_idle_time: Duration::from_secs(5 * 60), // 5 minutes (for on-demand session model)
            rotation_strategy: RotationStrategy::Hybrid,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RotationStrategy {
    TimeBasedOnly,
    RequestBasedOnly,
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub scope: ScopeConfig,
    pub grpc_port: u16,
    pub pod_name: String,
    pub pod_ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    pub grpc_port: u16,
    #[serde(with = "humantime_serde")]
    pub worker_cache_ttl: Duration,
    pub enable_metrics: bool,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            grpc_port: 50051,
            worker_cache_ttl: Duration::from_secs(5),
            enable_metrics: true,
        }
    }
}
