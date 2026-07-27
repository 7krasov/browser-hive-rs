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
    /// Browser diagnostics: capture of JS errors, failed resource loads, console output and
    /// page state, plus the gates that keep their cost and log volume bounded.
    /// Default: disabled (faster response, recommended for production).
    /// See [`DiagnosticsConfig`].
    pub diagnostics: DiagnosticsConfig,

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
            .field("diagnostics", &self.diagnostics)
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

// ---------------------------------------------------------------------------
// Browser diagnostics
//
// Diagnostics answer "why did this page not render what the client asked for" by capturing
// browser-side signals the returned HTML cannot show: uncaught JavaScript exceptions, failed
// resource loads, console output and the final page state.
//
// Two independent gates keep this affordable in production:
//
// - **Capture** (per request, decided *before* navigation) is gated by `enabled`, `mode` and
//   `domains`. It costs CDP traffic, so it must be decided up front - the request outcome does
//   not exist yet.
// - **Logging** (decided *after* the request) is gated by the outcome (`DiagnosticsMode::OnError`),
//   by per-category entry caps and by a per-minute rate limit. This is what actually controls log
//   volume, because the overwhelming majority of requests succeed.
//
// Only the worker captures diagnostics; the configuration lives here because `ScopeConfig` does,
// and downstream workers build a `ScopeConfig`. The capture implementation is in the worker
// crate (`worker/src/diagnostics.rs`), which is the only crate that uses it.
// ---------------------------------------------------------------------------

/// Default cap on captured entries per category.
pub const DEFAULT_DIAGNOSTICS_MAX_ENTRIES: usize = 20;

/// Default cap on how many requests per minute may emit diagnostics.
pub const DEFAULT_DIAGNOSTICS_MAX_PER_MINUTE: u32 = 10;

/// When captured diagnostics are written to the log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiagnosticsMode {
    /// Never. Capture is skipped entirely, so this is equivalent to disabling diagnostics.
    Off,

    /// Only for requests that failed in a way the client did not ask for: navigation errors,
    /// `wait_selector` not found, hard timeouts.
    ///
    /// A found `skip_selector` is **not** treated as a failure — it is an expected outcome that
    /// would otherwise produce diagnostics on every skipped page.
    #[default]
    OnError,

    /// For every request, successful or not. Useful when comparing a healthy page against a
    /// broken one; expensive to leave on for a busy scope.
    Always,
}

impl FromStr for DiagnosticsMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" | "never" => Ok(Self::Off),
            "on_error" | "onerror" | "error" => Ok(Self::OnError),
            "always" => Ok(Self::Always),
            _ => Err(()),
        }
    }
}

/// Configuration for browser diagnostics capture and logging.
#[derive(Debug, Clone)]
pub struct DiagnosticsConfig {
    /// Master switch. When false nothing is captured and nothing is logged.
    pub enabled: bool,

    /// When captured diagnostics are logged.
    pub mode: DiagnosticsMode,

    /// Hosts to capture for. **Empty means every host.** A non-empty list restricts capture to
    /// the listed registrable hosts and their subdomains, which limits both log volume and the
    /// extra CDP surface (see [`DiagnosticsConfig::capture_console`]).
    pub domains: Vec<String>,

    /// Maximum entries kept per category (JS errors, failed requests, console messages).
    /// Additional entries are counted and dropped, so one page stuck in an error loop cannot
    /// produce an unbounded log line.
    pub max_entries: usize,

    /// Maximum number of requests per minute that may emit diagnostics. `0` means unlimited.
    /// Suppressed requests are counted and reported with the next emitted line.
    pub max_per_minute: u32,

    /// Capture page `console.*` output and uncaught exceptions via the CDP `Runtime` domain.
    ///
    /// **Off by default on purpose.** Enabling the `Runtime` domain is a known anti-bot
    /// fingerprinting vector — some protection vendors detect it by observing that the browser
    /// serialises exception objects for a listening debugger. Without it we still capture JS
    /// errors and failed loads through the `Log` and `Network` domains, which carry no such
    /// signal. Turn this on together with [`DiagnosticsConfig::domains`] for targeted debugging
    /// rather than fleet-wide.
    pub capture_console: bool,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: DiagnosticsMode::default(),
            domains: Vec::new(),
            max_entries: DEFAULT_DIAGNOSTICS_MAX_ENTRIES,
            max_per_minute: DEFAULT_DIAGNOSTICS_MAX_PER_MINUTE,
            capture_console: false,
        }
    }
}

impl DiagnosticsConfig {
    /// Build the configuration from environment variables.
    ///
    /// Provided here rather than in the worker binary so that downstream workers, which have
    /// their own `main.rs`, get the full set of knobs from a single call instead of
    /// re-implementing six parsers.
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `WORKER_ENABLE_BROWSER_DIAGNOSTICS` | `false` |
    /// | `WORKER_DIAGNOSTICS_MODE` (`off`/`on_error`/`always`) | `on_error` |
    /// | `WORKER_DIAGNOSTICS_DOMAINS` (comma-separated, empty = all) | empty |
    /// | `WORKER_DIAGNOSTICS_MAX_ENTRIES` | `20` |
    /// | `WORKER_DIAGNOSTICS_MAX_PER_MINUTE` (`0` = unlimited) | `10` |
    /// | `WORKER_DIAGNOSTICS_CONSOLE` | `false` |
    pub fn from_env() -> Self {
        let defaults = Self::default();

        let enabled = std::env::var("WORKER_ENABLE_BROWSER_DIAGNOSTICS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults.enabled);

        let mode = std::env::var("WORKER_DIAGNOSTICS_MODE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults.mode);

        let domains = std::env::var("WORKER_DIAGNOSTICS_DOMAINS")
            .map(|s| parse_domain_list(&s))
            .unwrap_or(defaults.domains);

        let max_entries = std::env::var("WORKER_DIAGNOSTICS_MAX_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults.max_entries);

        let max_per_minute = std::env::var("WORKER_DIAGNOSTICS_MAX_PER_MINUTE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults.max_per_minute);

        let capture_console = std::env::var("WORKER_DIAGNOSTICS_CONSOLE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults.capture_console);

        Self {
            enabled,
            mode,
            domains,
            max_entries,
            max_per_minute,
            capture_console,
        }
    }

    /// Whether diagnostics should be captured for a request to `url`.
    ///
    /// Called before navigation, so it can only consider configuration and the target URL.
    /// A URL that cannot be parsed matches only when no domain filter is configured — a filter
    /// is an explicit narrowing and must not be widened by a parse failure.
    pub fn is_active_for(&self, url: &str) -> bool {
        if !self.enabled || self.mode == DiagnosticsMode::Off {
            return false;
        }
        if self.domains.is_empty() {
            return true;
        }
        match url::Url::parse(url).ok().and_then(|u| {
            u.host_str()
                .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        }) {
            Some(host) => self.domains.iter().any(|d| host_matches(&host, d)),
            None => false,
        }
    }
}

/// Parse a comma-separated domain list, normalising case and stripping a leading dot so that
/// both `example.com` and `.example.com` behave the same.
fn parse_domain_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|d| d.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect()
}

/// Match a host against a configured domain on a **label boundary**.
///
/// `ends_with` alone would make `egorealestate.com` match `notegorealestate.com`, so a
/// non-exact match additionally requires a dot right before the suffix.
fn host_matches(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    host.len() > domain.len()
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_matches_on_label_boundary() {
        assert!(host_matches("egorealestate.com", "egorealestate.com"));
        assert!(host_matches(
            "11828-3.ep.egorealestate.com",
            "egorealestate.com"
        ));
        // The reason this is not a plain `ends_with`.
        assert!(!host_matches("notegorealestate.com", "egorealestate.com"));
        assert!(!host_matches(
            "egorealestate.com.evil.net",
            "egorealestate.com"
        ));
    }

    #[test]
    fn test_parse_domain_list() {
        assert_eq!(
            parse_domain_list(" Example.COM , .foo.org ,, "),
            vec!["example.com".to_string(), "foo.org".to_string()]
        );
        assert!(parse_domain_list("").is_empty());
    }

    #[test]
    fn test_is_active_for() {
        let base = DiagnosticsConfig {
            enabled: true,
            ..Default::default()
        };

        // No domain filter → every host.
        assert!(base.is_active_for("https://anything.example/x"));

        let filtered = DiagnosticsConfig {
            domains: vec!["egorealestate.com".to_string()],
            ..base.clone()
        };
        assert!(filtered.is_active_for("https://11828-3.ep.egorealestate.com/imoveis?a=1"));
        assert!(!filtered.is_active_for("https://example.com/"));
        // A filter must not be widened by an unparseable URL.
        assert!(!filtered.is_active_for("not a url"));

        // Disabled and Off both suppress capture regardless of domain.
        assert!(!DiagnosticsConfig {
            enabled: false,
            ..filtered.clone()
        }
        .is_active_for("https://egorealestate.com/"));
        assert!(!DiagnosticsConfig {
            mode: DiagnosticsMode::Off,
            ..filtered
        }
        .is_active_for("https://egorealestate.com/"));
    }

    #[test]
    fn test_mode_from_str() {
        assert_eq!("off".parse(), Ok(DiagnosticsMode::Off));
        assert_eq!("ON_ERROR".parse(), Ok(DiagnosticsMode::OnError));
        assert_eq!("always".parse(), Ok(DiagnosticsMode::Always));
        assert_eq!("nonsense".parse::<DiagnosticsMode>(), Err(()));
    }
}
