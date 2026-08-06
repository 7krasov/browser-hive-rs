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
    /// Contexts to pre-create on startup. `0` (the default) means none.
    ///
    /// Pre-initialization is an option, not a mode: any value above 0 makes the pool fill itself
    /// at startup. It only pays off in [`SessionMode::Reusable`], where a pre-created context can
    /// serve any request; in the other modes a context has no client, so it is destroyed unused —
    /// [`ScopeConfig::validate`] warns about that rather than silently pre-creating.
    pub min_contexts: u16,
    pub max_contexts: u16,         // Maximum number of browser contexts allowed
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

    /// Destroy a [`SessionMode::Dedicated`] context as soon as its page comes back 403 or 429.
    ///
    /// This is what replaces an explicit release RPC. A client drops its session on those
    /// statuses anyway, so without this the worker would keep the slot — and the burnt exit IP —
    /// claimed until the idle timeout expires. The statuses are the ones the wait strategy already
    /// exits early on, so the signal costs nothing extra.
    ///
    /// An **option** rather than a rule: a 403 on one page does not universally mean the session
    /// is dead. Has no effect outside `Dedicated`, where a context is either destroyed with the
    /// request (`AlwaysNew`) or shared by everyone (`Reusable`); [`ScopeConfig::validate`] warns
    /// when it is set there.
    pub destroy_session_on_block: bool,
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
/// The mode decides three things at once: whether a context outlives the request that created
/// it, who may be handed that context next, and what `max_contexts` counts. See SESSION_MODES.md.
///
/// | Mode | Context outlives the request | Who gets it next | `max_contexts` counts |
/// |------|---|---|---|
/// | `AlwaysNew` | no | nobody | concurrent **requests** |
/// | `Reusable` | yes, until recycled | any later request | concurrent **requests** |
/// | `Dedicated` | yes, until idle or blocked | only the session that owns it | concurrent **sessions** |
///
/// **Note on contexts that outlive their request**: they are NOT kept forever. The lifecycle
/// monitor acts on them based on `ContextLifecycleConfig` — `Reusable` contexts are *recycled*
/// (replaced by a fresh one in the same slot), `Dedicated` contexts are *removed* (the slot goes
/// back to the pool, because nothing else can free it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    /// Create a new context for each request, destroy after completion.
    /// - Contexts are created on-demand
    /// - Each request gets a fresh, isolated browser state
    /// - Context is destroyed immediately after the request completes
    /// - Best for: one-shot scraping, avoiding session/cookie contamination
    /// - Session IDs are NOT returned to clients (cannot be reused)
    AlwaysNew,

    /// An anonymous pool of reusable contexts.
    /// - Contexts are created on-demand up to `max_contexts` and reused by **any** later request
    /// - Requests are spread evenly across contexts, so every exit IP carries a similar share
    /// - Contexts are recycled based on `ContextLifecycleConfig` (idle time, age, requests, cache)
    /// - Best for: high-throughput scraping where each request stands on its own
    /// - Session IDs are NOT returned: the pool guarantees no client anything, and pretending
    ///   otherwise is what made two unrelated clients believe they shared a session
    #[default]
    Reusable,

    /// One context belongs to one session, for as long as the client keeps using it.
    /// - A request without a session id always gets a **fresh** context; a claimed context is
    ///   never handed to a stranger, so `Context is already busy` between unrelated clients
    ///   cannot happen
    /// - Cookies, storage and (with a sticky provider) the exit IP stay stable across the
    ///   client's requests
    /// - `max_contexts` bounds **concurrent sessions**, not request rate: a client that fetches
    ///   one page and disappears holds its slot until `max_idle_time`
    /// - The idle timeout **removes** the context — it is the only thing that frees a slot
    ///   without client cooperation, which is why it should be short (a minute, not five)
    /// - Optionally destroyed on 403/429, see [`ScopeConfig::destroy_session_on_block`]
    /// - Best for: multi-page workflows, login flows, anything that must not be interleaved
    ///   with another client's traffic
    /// - Session IDs ARE returned to clients
    Dedicated,
}

impl SessionMode {
    /// The value accepted by `WORKER_SESSION_MODE`, and what logs call this mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AlwaysNew => "always_new",
            Self::Reusable => "reusable",
            Self::Dedicated => "dedicated",
        }
    }

    /// Whether clients of this mode receive a `session_id` they can send back.
    pub fn returns_session_id(&self) -> bool {
        matches!(self, Self::Dedicated)
    }
}

impl FromStr for SessionMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "always_new" => Ok(Self::AlwaysNew),
            "reusable" => Ok(Self::Reusable),
            "dedicated" => Ok(Self::Dedicated),
            _ => Err(()),
        }
    }
}

/// Accepted values of `WORKER_SESSION_MODE`, for error messages.
pub const SESSION_MODE_VALUES: &str = "always_new, reusable, dedicated";

/// Default idle timeout for modes where an idle context is still useful to somebody.
pub const DEFAULT_MAX_IDLE_TIME_SECS: u64 = 5 * 60;

/// Default idle timeout for [`SessionMode::Dedicated`].
///
/// Much shorter than the others because it is the **only** way a claimed slot comes back without
/// the client cooperating: there is no release RPC, so every abandoned session costs capacity for
/// exactly this long. A client that is genuinely working a site comes back far sooner than a
/// minute; one that does not was finished anyway.
pub const DEFAULT_DEDICATED_MAX_IDLE_TIME_SECS: u64 = 60;

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
            .field("destroy_session_on_block", &self.destroy_session_on_block)
            .finish()
    }
}

impl ScopeConfig {
    /// Check the configuration for combinations that cannot do what they say.
    ///
    /// Called fail-fast at worker startup, in the same spirit as the "a worker never starts
    /// without a proxy" guard in `BrowserPool::new`: a scope that is configured to do something
    /// it cannot do must not serve traffic while its logs claim otherwise.
    ///
    /// Two categories, deliberately distinguished:
    ///
    /// - **Error** — the combination silently does the wrong thing. The worker refuses to start.
    /// - **Warning** — the setting is valid but has no effect in this mode. Returned to the
    ///   caller to log, never dropped: a value nobody chose to ignore was still typed by
    ///   somebody, and the whole point of validating is that no configuration disappears in
    ///   silence.
    ///
    /// A third category has nothing to check: `max_lifetime` in `Dedicated`, whose useful value
    /// is the proxy provider's sticky TTL and is measured rather than guessed (see
    /// SESSION_MODES.md).
    pub fn validate(&self) -> anyhow::Result<Vec<String>> {
        let mode = self.session_mode.as_str();

        if self.max_contexts == 0 {
            anyhow::bail!(
                "scope '{}': max_contexts is 0, so the scope can serve nothing",
                self.name
            );
        }

        if self.min_contexts > self.max_contexts {
            anyhow::bail!(
                "scope '{}': min_contexts ({}) exceeds max_contexts ({})",
                self.name,
                self.min_contexts,
                self.max_contexts
            );
        }

        if self.session_mode == SessionMode::Dedicated {
            // Every request in shared isolation runs in the browser's default context, which is
            // one context for the whole process — exclusivity cannot exist there, and the mode
            // would degrade into `reusable` with a session id nobody honours.
            if self.context_isolation == ContextIsolation::Shared {
                anyhow::bail!(
                    "scope '{}': session_mode=dedicated requires context_isolation=isolated. \
                     In shared isolation every request runs in the browser's default context, so \
                     a context cannot belong to one session.",
                    self.name
                );
            }

            // The idle timeout is the only thing that frees a claimed slot without the client's
            // help, and a non-Hybrid rotation strategy does not consult it — the pool would fill
            // up with abandoned sessions and never recover.
            if !matches!(self.lifecycle.rotation_strategy, RotationStrategy::Hybrid) {
                anyhow::bail!(
                    "scope '{}': session_mode=dedicated requires rotation_strategy=Hybrid, but \
                     it is {:?}. Only Hybrid consults max_idle_time, which is the only way a \
                     claimed slot is ever released.",
                    self.name,
                    self.lifecycle.rotation_strategy
                );
            }
        }

        // A context that dies of old age before it can ever be idle long enough is a lifecycle
        // configured backwards. Checked only where these thresholds do something — in AlwaysNew
        // they are ignored (and warned about below), so failing on their relationship would
        // reject a harmless shared lifecycle config.
        if self.session_mode != SessionMode::AlwaysNew
            && self.lifecycle.max_lifetime < self.lifecycle.max_idle_time
        {
            anyhow::bail!(
                "scope '{}': max_lifetime ({:?}) is shorter than max_idle_time ({:?}), so a \
                 context is always recycled by age before it can be idle long enough",
                self.name,
                self.lifecycle.max_lifetime,
                self.lifecycle.max_idle_time
            );
        }

        let mut warnings = Vec::new();

        if self.min_contexts > 0 && self.session_mode != SessionMode::Reusable {
            warnings.push(format!(
                "scope '{}': min_contexts={} pre-creates contexts that {} mode cannot hand to \
                 anybody - they will be destroyed unused. Set WORKER_MIN_CONTEXTS=0.",
                self.name, self.min_contexts, mode
            ));
        }

        if self.session_mode == SessionMode::AlwaysNew {
            warnings.push(format!(
                "scope '{}': the lifecycle settings (max_idle_time, max_lifetime, max_requests, \
                 max_cache_size_mb) have no effect in always_new mode - a context never outlives \
                 its request.",
                self.name
            ));
        }

        // The general case of the gate that is a hard error for `dedicated`: under the
        // request/time-only strategies two thresholds are read from the config and then ignored.
        if self.session_mode == SessionMode::Reusable
            && !matches!(self.lifecycle.rotation_strategy, RotationStrategy::Hybrid)
        {
            warnings.push(format!(
                "scope '{}': rotation_strategy={:?} ignores max_idle_time and max_cache_size_mb; \
                 only Hybrid consults all four thresholds.",
                self.name, self.lifecycle.rotation_strategy
            ));
        }

        if self.destroy_session_on_block && self.session_mode != SessionMode::Dedicated {
            warnings.push(format!(
                "scope '{}': destroy_session_on_block has no effect in {} mode - a context is \
                 either destroyed with its request or shared by every client.",
                self.name, mode
            ));
        }

        Ok(warnings)
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
            max_idle_time: Duration::from_secs(DEFAULT_MAX_IDLE_TIME_SECS),
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
    pub metrics_port: u16,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            grpc_port: 50051,
            worker_cache_ttl: Duration::from_secs(5),
            enable_metrics: true,
            metrics_port: 9090,
        }
    }
}

impl CoordinatorConfig {
    /// Build the configuration from environment variables.
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `COORDINATOR_GRPC_PORT` | `50051` |
    /// | `COORDINATOR_WORKER_CACHE_TTL` (humantime, e.g. `5s`) | `5s` |
    /// | `COORDINATOR_ENABLE_METRICS` | `true` |
    /// | `COORDINATOR_METRICS_PORT` | `9090` |
    ///
    /// An unparseable value falls back to the default rather than failing: the coordinator has
    /// no scope validation phase, and refusing to start over a malformed metrics port would take
    /// down routing for the whole cluster. Every fallback is logged by the caller-visible
    /// `warnings()` list so a typo is never silent.
    pub fn from_env() -> (Self, Vec<String>) {
        let defaults = Self::default();
        let mut warnings = Vec::new();

        fn parse_var<T: std::str::FromStr>(
            name: &str,
            default: T,
            warnings: &mut Vec<String>,
        ) -> T {
            match std::env::var(name) {
                Err(_) => default,
                Ok(raw) => match raw.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        warnings.push(format!(
                            "{}='{}' could not be parsed, using the default",
                            name, raw
                        ));
                        default
                    }
                },
            }
        }

        let grpc_port = parse_var("COORDINATOR_GRPC_PORT", defaults.grpc_port, &mut warnings);
        let enable_metrics = parse_var(
            "COORDINATOR_ENABLE_METRICS",
            defaults.enable_metrics,
            &mut warnings,
        );
        let metrics_port = parse_var(
            "COORDINATOR_METRICS_PORT",
            defaults.metrics_port,
            &mut warnings,
        );

        let worker_cache_ttl = match std::env::var("COORDINATOR_WORKER_CACHE_TTL") {
            Err(_) => defaults.worker_cache_ttl,
            Ok(raw) => match humantime::parse_duration(&raw) {
                Ok(d) => d,
                Err(_) => {
                    warnings.push(format!(
                        "COORDINATOR_WORKER_CACHE_TTL='{}' could not be parsed, using the default",
                        raw
                    ));
                    defaults.worker_cache_ttl
                }
            },
        };

        (
            Self {
                grpc_port,
                worker_cache_ttl,
                enable_metrics,
                metrics_port,
            },
            warnings,
        )
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
/// `ends_with` alone would make `example.com` match `notexample.com`, so a
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
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("a1.sub.example.com", "example.com"));
        // The reason this is not a plain `ends_with`.
        assert!(!host_matches("notexample.com", "example.com"));
        assert!(!host_matches("example.com.evil.net", "example.com"));
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
            domains: vec!["example.com".to_string()],
            ..base.clone()
        };
        assert!(filtered.is_active_for("https://a1.sub.example.com/items?a=1"));
        assert!(!filtered.is_active_for("https://other.org/"));
        // A filter must not be widened by an unparseable URL.
        assert!(!filtered.is_active_for("not a url"));

        // Disabled and Off both suppress capture regardless of domain.
        assert!(!DiagnosticsConfig {
            enabled: false,
            ..filtered.clone()
        }
        .is_active_for("https://example.com/"));
        assert!(!DiagnosticsConfig {
            mode: DiagnosticsMode::Off,
            ..filtered
        }
        .is_active_for("https://example.com/"));
    }

    /// Minimal provider: `validate` never touches the proxy, but `ScopeConfig` needs one.
    #[derive(Debug, Clone)]
    struct TestProvider;

    impl crate::proxy::ProxyProvider for TestProvider {
        fn build_config(&self) -> anyhow::Result<crate::proxy::ProxyConfig> {
            Ok(crate::proxy::ProxyConfig {
                proxy_url: None,
                scheme: crate::proxy::ProxyScheme::Http,
                address: None,
                port: None,
                username: None,
                password: None,
            })
        }

        fn name(&self) -> &str {
            "test"
        }

        fn clone_box(&self) -> Box<dyn crate::proxy::ProxyProvider> {
            Box::new(self.clone())
        }
    }

    fn scope(session_mode: SessionMode) -> ScopeConfig {
        ScopeConfig {
            name: "test_scope".to_string(),
            proxy_provider: Box::new(TestProvider),
            min_contexts: 0,
            max_contexts: 3,
            session_mode,
            headless: true,
            lifecycle: ContextLifecycleConfig::default(),
            browser_path: None,
            diagnostics: DiagnosticsConfig::default(),
            binary_params_middlewares: vec![],
            tab_init_middlewares: vec![],
            context_isolation: ContextIsolation::Isolated,
            destroy_session_on_block: false,
        }
    }

    #[test]
    fn test_session_mode_from_str() {
        assert_eq!("always_new".parse(), Ok(SessionMode::AlwaysNew));
        assert_eq!("Reusable".parse(), Ok(SessionMode::Reusable));
        assert_eq!("dedicated".parse(), Ok(SessionMode::Dedicated));
        // Removed outright rather than aliased: a manifest still asking for it must fail loudly,
        // not silently get a mode with different capacity semantics.
        assert_eq!("reusable_preinit".parse::<SessionMode>(), Err(()));
    }

    /// A `dedicated` scope in shared isolation would serve every session from the browser's one
    /// default context — exclusivity, cookies and exit IP all silently absent.
    #[test]
    fn test_dedicated_requires_isolated_contexts() {
        let mut config = scope(SessionMode::Dedicated);
        config.context_isolation = ContextIsolation::Shared;

        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("dedicated") && error.contains("isolated"),
            "the error must name the mode and the required isolation: {error}"
        );
    }

    /// Without Hybrid nothing consults `max_idle_time`, and in `dedicated` that is the only way a
    /// claimed slot ever comes back: the pool would fill with abandoned sessions permanently.
    #[test]
    fn test_dedicated_requires_hybrid_rotation() {
        let mut config = scope(SessionMode::Dedicated);
        config.lifecycle.rotation_strategy = RotationStrategy::TimeBasedOnly;

        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("max_idle_time"),
            "the error must say which threshold stops being consulted: {error}"
        );
    }

    #[test]
    fn test_rejects_impossible_capacities() {
        let mut zero = scope(SessionMode::Reusable);
        zero.max_contexts = 0;
        assert!(zero.validate().is_err());

        let mut inverted = scope(SessionMode::Reusable);
        inverted.min_contexts = 5;
        inverted.max_contexts = 3;
        assert!(inverted.validate().is_err());
    }

    /// The lifecycle relationship is only checked where the thresholds do something. In
    /// `always_new` they are ignored, so a shared lifecycle config must not block startup.
    #[test]
    fn test_inverted_lifetime_is_an_error_only_where_it_applies() {
        let mut reusable = scope(SessionMode::Reusable);
        reusable.lifecycle.max_lifetime = Duration::from_secs(60);
        reusable.lifecycle.max_idle_time = Duration::from_secs(300);
        assert!(reusable.validate().is_err());

        let mut always_new = scope(SessionMode::AlwaysNew);
        always_new.lifecycle.max_lifetime = Duration::from_secs(60);
        always_new.lifecycle.max_idle_time = Duration::from_secs(300);
        assert!(always_new.validate().is_ok());
    }

    /// Settings that do nothing must be reported, not dropped — that is the whole point of
    /// validating a configuration nobody re-reads after startup.
    #[test]
    fn test_warns_about_settings_with_no_effect() {
        let mut config = scope(SessionMode::Dedicated);
        config.min_contexts = 2;
        let warnings = config.validate().expect("valid, just pointless");
        assert!(warnings.iter().any(|w| w.contains("min_contexts")));

        let mut blocked = scope(SessionMode::Reusable);
        blocked.destroy_session_on_block = true;
        let warnings = blocked.validate().expect("valid, just pointless");
        assert!(warnings
            .iter()
            .any(|w| w.contains("destroy_session_on_block")));

        // The default configuration of every mode must be warning-free, otherwise the warnings
        // become noise nobody reads.
        for mode in [
            SessionMode::AlwaysNew,
            SessionMode::Reusable,
            SessionMode::Dedicated,
        ] {
            let warnings = scope(mode).validate().expect("defaults must be valid");
            if mode == SessionMode::AlwaysNew {
                // Except always_new, which always carries lifecycle settings it ignores.
                continue;
            }
            assert!(
                warnings.is_empty(),
                "{mode:?} warned about defaults: {warnings:?}"
            );
        }
    }

    #[test]
    fn test_mode_from_str() {
        assert_eq!("off".parse(), Ok(DiagnosticsMode::Off));
        assert_eq!("ON_ERROR".parse(), Ok(DiagnosticsMode::OnError));
        assert_eq!("always".parse(), Ok(DiagnosticsMode::Always));
        assert_eq!("nonsense".parse::<DiagnosticsMode>(), Err(()));
    }
}
