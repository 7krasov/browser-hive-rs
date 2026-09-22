use anyhow::Result;
use headless_chrome::protocol::cdp::Network::ResourceType;
use std::ffi::OsStr;
use std::fmt::Debug;

/// Middleware for modifying Chrome binary launch parameters
///
/// Implement this trait to customize Chrome's command-line arguments before launch.
/// This is useful for:
/// - Adding stealth/anti-bot arguments
/// - Configuring performance parameters
/// - Setting up caching strategies
/// - Customizing browser behavior per scope
///
/// # Example
///
/// ```rust
/// use browser_hive_common::browser_middleware::BrowserBinaryParamsMiddleware;
/// use std::ffi::OsStr;
///
/// #[derive(Debug, Clone)]
/// struct CustomStealth {
///     disable_webrtc: bool,
/// }
///
/// impl BrowserBinaryParamsMiddleware for CustomStealth {
///     fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
///         if self.disable_webrtc {
///             args.push(OsStr::new("--disable-webrtc"));
///         }
///     }
///
///     fn name(&self) -> &str {
///         "custom_stealth"
///     }
///
///     fn clone_box(&self) -> Box<dyn BrowserBinaryParamsMiddleware> {
///         Box::new(self.clone())
///     }
/// }
/// ```
pub trait BrowserBinaryParamsMiddleware: Debug + Send + Sync {
    /// Apply Chrome launch arguments
    ///
    /// This method receives a mutable vector of arguments and can:
    /// - Add new arguments
    /// - Conditionally add arguments based on headless mode
    /// - Add performance or stealth configurations
    ///
    /// # Parameters
    /// * `args` - Mutable vector of Chrome arguments (as OsStr for cross-platform support)
    /// * `headless` - Whether browser is running in headless mode
    fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool);

    /// Get unique identifier for this middleware (used in logging)
    fn name(&self) -> &str;

    /// Clone this middleware into a Box
    ///
    /// Required for trait objects to be cloneable.
    /// Standard implementation: `Box::new(self.clone())`
    fn clone_box(&self) -> Box<dyn BrowserBinaryParamsMiddleware>;
}

/// Make Box<dyn BrowserBinaryParamsMiddleware> cloneable
impl Clone for Box<dyn BrowserBinaryParamsMiddleware> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Middleware for tab initialization after creation
///
/// Implement this trait to execute CDP commands immediately after tab creation,
/// before any navigation or page loading occurs. This is useful for:
/// - Overriding navigator properties (user agent, platform, etc.)
/// - Injecting JavaScript stealth scripts
/// - Setting up tracking prevention
/// - Customizing browser fingerprint
///
/// # Example
///
/// ```rust
/// use browser_hive_common::browser_middleware::TabInitMiddleware;
/// use anyhow::Result;
///
/// #[derive(Debug, Clone)]
/// struct TimezoneOverride {
///     timezone: String,
/// }
///
/// impl TabInitMiddleware for TimezoneOverride {
///     fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()> {
///         let script = format!(
///             "Object.defineProperty(Intl.DateTimeFormat.prototype, 'resolvedOptions', {{ \
///                 value: function() {{ return {{ timeZone: '{}' }}; }} \
///             }});",
///             self.timezone
///         );
///         tab.evaluate(&script, false)?;
///         Ok(())
///     }
///
///     fn name(&self) -> &str {
///         "timezone_override"
///     }
///
///     fn clone_box(&self) -> Box<dyn TabInitMiddleware> {
///         Box::new(self.clone())
///     }
/// }
/// ```
pub trait TabInitMiddleware: Debug + Send + Sync {
    /// Apply CDP operations to a newly created tab
    ///
    /// This method is called immediately after `browser.new_tab()` and can:
    /// - Execute CDP protocol commands
    /// - Override navigator properties via CDP
    /// - Inject JavaScript for stealth
    ///
    /// **Important**: This is called for EVERY new tab (initial contexts + recycled contexts),
    /// so keep operations lightweight and fast.
    ///
    /// # Parameters
    /// * `tab` - Reference to the newly created tab (not yet navigated anywhere)
    ///
    /// # Returns
    /// * `Ok(())` if middleware applied successfully
    /// * `Err` if critical failure (tab will still be usable, error is logged)
    fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()>;

    /// Get unique identifier for this middleware (used in logging)
    fn name(&self) -> &str;

    /// Clone this middleware into a Box
    ///
    /// Required for trait objects to be cloneable.
    /// Standard implementation: `Box::new(self.clone())`
    fn clone_box(&self) -> Box<dyn TabInitMiddleware>;

    /// Resource types this middleware drops, so the worker can attribute the blocked loads it
    /// observes to them (`browser_hive_worker_requests_blocked_by_type_total`). Only
    /// [`BlockedResourceTypesMiddleware`] overrides it.
    fn blocked_resource_types(&self) -> &[BlockedResourceType] {
        &[]
    }
}

/// Make Box<dyn TabInitMiddleware> cloneable
impl Clone for Box<dyn TabInitMiddleware> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Default implementation of BrowserBinaryParamsMiddleware for Chrome/Chromium
///
/// This middleware adds the standard Chrome arguments for:
/// - Container compatibility (--no-sandbox, --disable-dev-shm-usage)
/// - Anti-bot stealth (disable AutomationControlled, exclude automation switches)
/// - Window size (1920x1080 for realistic desktop)
/// - Performance optimization (disable throttling, first-run checks)
/// - Cache configuration (500MB disk cache, 100MB media cache)
/// - Memory management (tab discarding, JS heap limits)
///
/// For Brave browser, use `BraveBinaryParamsMiddleware` instead.
#[derive(Debug, Clone)]
pub struct DefaultBinaryParamsMiddleware;

impl BrowserBinaryParamsMiddleware for DefaultBinaryParamsMiddleware {
    fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
        // Required for containers (Docker, K8s) — Chrome's namespace sandbox
        // requires SYS_ADMIN capability which is a security risk. Container
        // isolation (namespaces, cgroups) provides equivalent protection.
        args.push(OsStr::new("--no-sandbox"));
        args.push(OsStr::new("--disable-dev-shm-usage"));

        // Anti-bot stealth (critical!)
        args.push(OsStr::new("--disable-blink-features=AutomationControlled"));
        args.push(OsStr::new("--exclude-switches=enable-automation"));

        // Window size (realistic Full HD desktop resolution)
        args.push(OsStr::new("--window-size=1920,1080"));

        // Startup optimization
        args.push(OsStr::new("--no-first-run"));
        args.push(OsStr::new("--no-default-browser-check"));

        // Prevent throttling (keeps JS running normally)
        args.push(OsStr::new("--disable-background-timer-throttling"));
        args.push(OsStr::new("--disable-backgrounding-occluded-windows"));
        args.push(OsStr::new("--disable-renderer-backgrounding"));

        // Cache configuration
        args.push(OsStr::new("--disk-cache-dir=/chrome-data/cache"));
        args.push(OsStr::new("--disk-cache-size=524288000")); // 500MB
        args.push(OsStr::new("--media-cache-size=104857600")); // 100MB

        // Memory management
        args.push(OsStr::new("--enable-features=TabDiscarding"));
        args.push(OsStr::new("--js-flags=--max-old-space-size=512"));

        // Headful-specific args (better stealth for visible browser)
        if !headless {
            args.push(OsStr::new("--disable-infobars"));
            args.push(OsStr::new(
                "--disable-features=IsolateOrigins,site-per-process",
            ));
        }
    }

    fn name(&self) -> &str {
        "default_binary_params"
    }

    fn clone_box(&self) -> Box<dyn BrowserBinaryParamsMiddleware> {
        Box::new(self.clone())
    }
}

/// Browser middleware optimized for Brave browser
///
/// Brave has built-in stealth features (Shields, fingerprint protection), so this middleware:
/// - Includes all standard Chrome args for Docker compatibility
/// - Disables Brave Shields to prevent interference with scraping
/// - Disables Brave-specific extensions that might interfere
/// - Uses the same cache and performance optimizations as Chrome
///
/// **Note**: Requires setting `ScopeConfig::browser_path` to Brave binary
/// (typically `/usr/bin/brave-browser` on Linux).
#[derive(Debug, Clone)]
pub struct BraveBinaryParamsMiddleware;

impl BrowserBinaryParamsMiddleware for BraveBinaryParamsMiddleware {
    fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
        // Required for containers (Docker, K8s) — Chrome's namespace sandbox
        // requires SYS_ADMIN capability which is a security risk. Container
        // isolation (namespaces, cgroups) provides equivalent protection.
        args.push(OsStr::new("--no-sandbox"));
        args.push(OsStr::new("--disable-dev-shm-usage"));

        // Anti-bot stealth (critical!)
        args.push(OsStr::new("--disable-blink-features=AutomationControlled"));
        args.push(OsStr::new("--exclude-switches=enable-automation"));

        // Brave-specific: disable Shields to prevent blocking of trackers/ads during scraping
        // This prevents Brave from interfering with page loading
        args.push(OsStr::new("--disable-brave-extension"));

        // Window size (realistic Full HD desktop resolution)
        args.push(OsStr::new("--window-size=1920,1080"));

        // Startup optimization
        args.push(OsStr::new("--no-first-run"));
        args.push(OsStr::new("--no-default-browser-check"));

        // Prevent throttling (keeps JS running normally)
        args.push(OsStr::new("--disable-background-timer-throttling"));
        args.push(OsStr::new("--disable-backgrounding-occluded-windows"));
        args.push(OsStr::new("--disable-renderer-backgrounding"));

        // Cache configuration
        args.push(OsStr::new("--disk-cache-dir=/chrome-data/cache"));
        args.push(OsStr::new("--disk-cache-size=524288000")); // 500MB
        args.push(OsStr::new("--media-cache-size=104857600")); // 100MB

        // Memory management
        args.push(OsStr::new("--enable-features=TabDiscarding"));
        args.push(OsStr::new("--js-flags=--max-old-space-size=512"));

        // Headful-specific args (better stealth for visible browser)
        if !headless {
            args.push(OsStr::new("--disable-infobars"));
            args.push(OsStr::new(
                "--disable-features=IsolateOrigins,site-per-process",
            ));
        }
    }

    fn name(&self) -> &str {
        "brave_binary_params"
    }

    fn clone_box(&self) -> Box<dyn BrowserBinaryParamsMiddleware> {
        Box::new(self.clone())
    }
}

/// Default implementation of TabInitMiddleware
///
/// This middleware:
/// - Detects the browser's User-Agent string on first use (lazy initialization)
/// - Replaces "HeadlessChrome" with "Chrome" to hide automation markers
/// - Uses CDP SetUserAgentOverride for clean, undetectable override
/// - Caches the corrected UA for subsequent calls
///
/// **Note**: Only applies to headless mode. Headful Chrome already has correct UA.
#[derive(Debug)]
pub struct DefaultTabInitMiddleware {
    /// Whether browser is running in headless mode
    pub headless: bool,
    /// Cached corrected User-Agent (lazy initialized on first apply)
    corrected_user_agent: std::sync::Mutex<Option<String>>,
}

impl DefaultTabInitMiddleware {
    /// Create new middleware instance
    ///
    /// # Parameters
    /// * `headless` - Whether browser is in headless mode
    ///
    /// **Note**: UA detection happens lazily on first `apply()` call
    pub fn new(headless: bool) -> Self {
        Self {
            headless,
            corrected_user_agent: std::sync::Mutex::new(None),
        }
    }
}

impl Clone for DefaultTabInitMiddleware {
    fn clone(&self) -> Self {
        let cached_ua = self.corrected_user_agent.lock().unwrap().clone();

        Self {
            headless: self.headless,
            corrected_user_agent: std::sync::Mutex::new(cached_ua),
        }
    }
}

impl TabInitMiddleware for DefaultTabInitMiddleware {
    fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()> {
        // Only apply in headless mode
        if !self.headless {
            return Ok(());
        }

        // Check if UA already detected
        let mut ua_guard = self.corrected_user_agent.lock().unwrap();

        let corrected_ua = if let Some(ref ua) = *ua_guard {
            // Already detected, use cached value
            ua.clone()
        } else {
            // First call, detect UA
            tracing::debug!("Detecting User-Agent to replace 'HeadlessChrome' with 'Chrome'...");

            // Detect UA from current tab
            let result = tab
                .evaluate("navigator.userAgent", false)
                .map_err(|e| anyhow::anyhow!("Failed to evaluate navigator.userAgent: {}", e))?;

            let corrected = if let Some(ua_value) = result.value {
                if let Some(original_ua) = ua_value.as_str() {
                    let corrected = original_ua.replace("HeadlessChrome", "Chrome");
                    tracing::debug!("Original UA: {}", original_ua);
                    tracing::debug!("Corrected UA: {}", corrected);
                    corrected
                } else {
                    anyhow::bail!(
                        "Could not extract UA string from navigator.userAgent (not a string)"
                    )
                }
            } else {
                anyhow::bail!("Could not extract UA string from navigator.userAgent (no value)")
            };

            // Cache for next calls
            *ua_guard = Some(corrected.clone());
            corrected
        };

        // Release lock before calling CDP
        drop(ua_guard);

        // Apply User-Agent override via CDP
        use headless_chrome::protocol::cdp::Network;

        tab.call_method(Network::SetUserAgentOverride {
            user_agent: corrected_ua.clone(),
            accept_language: None,
            platform: None,
            user_agent_metadata: None,
        })?;

        tracing::debug!("Applied User-Agent override via CDP: {}", corrected_ua);

        Ok(())
    }

    fn name(&self) -> &str {
        "default_user_agent_override"
    }

    fn clone_box(&self) -> Box<dyn TabInitMiddleware> {
        Box::new(self.clone())
    }
}

/// Blocks matching URLs from loading, on every tab of the scope.
///
/// Third-party analytics, ad and session-recording scripts cost a request three ways: bandwidth
/// through the proxy, a separate CONNECT tunnel per host (which for gateway providers can draw a
/// *different* exit IP — see `PROXY_NETWORKING.md`), and wall-clock time, since the
/// `network_idle` strategy waits for `networkAlmostIdle` and their beacons are exactly what keeps
/// requests in flight. Dropping them at the browser is the cheapest place to do it.
///
/// # This type holds no list of its own
///
/// The patterns are supplied by the caller. Nothing is blocked by default and no domain is named
/// in this crate: which third parties are safe to cut is a property of the sites a deployment
/// scrapes, not of the library. An empty list makes the middleware a no-op.
///
/// # Pattern syntax
///
/// Patterns go to CDP `Network.setBlockedURLs` **unchanged**. Measured (2026-09-22, Chrome, macOS,
/// headless): the pattern is split at every `*`, and a URL is blocked when it contains those pieces
/// **in order, anywhere** — there is no anchoring to the start or the end of the URL, `*` is the
/// only special character, `?` is a literal, and there is no escape syntax. The matcher knows
/// nothing about hosts or paths; the patterns below work as host rules only because `://` and `/`
/// frame the host:
///
/// | Intent | Pattern |
/// |---|---|
/// | one host, no subdomains | `*://example.com/*` |
/// | subdomains only | `*://*.example.com/*` |
/// | both | the two patterns above |
/// | one path on a host | `*://example.com/tracker/*` |
/// | one file, any subdomain | `*://*.example.com/beacon.js*` |
///
/// Consequences worth knowing before writing a list:
///
/// - A bare host (`example.com`) matches **too much**, not nothing: with no anchoring it behaves
///   like `*example.com*` and blocks every URL containing the string — `notexample.com`, and any
///   URL carrying it in a query, such as `https://other.test/?ref=example.com`. [`Self::new`] warns
///   about every pattern without a `*` for that reason.
/// - Even `*://example.com/*` also matches a URL that carries that address in its query string
///   (a redirect parameter), and it does not match the host with an explicit port.
/// - There is no way to say "the URL **ends** with X": `*.m3u8` also blocks `x.m3u8.js` and
///   `a.m3u8x/b.js`. Narrow it with what follows instead — `*.m3u8?*` blocks only the manifest
///   with a query string.
/// - `?` is a plain character, not a single-character wildcard (that is the `Fetch` domain's
///   `urlPattern` syntax, a different matcher — do not assume the two agree).
///
/// # What must never be blocked
///
/// Anti-bot and consent scripts. Blocking an analytics endpoint costs a page nothing, but a
/// blocked challenge script (bot detection, CAPTCHA) turns a page that would have loaded into a
/// hard block, and a blocked consent manager can leave the content gated forever. Note that these
/// often share a host with blockable content, which is why path-level patterns exist above.
///
/// # Scope of the effect
///
/// The list is installed per tab, in the same call that every other tab-init middleware runs
/// (initial contexts, recycled contexts, tabs recreated after a dead CDP session), so it covers
/// every request the scope serves. `Network` is enabled here as `setBlockedURLs` acts on that
/// domain; this adds no CDP surface, since the worker's response observer enables `Network` on
/// every request anyway.
///
/// Blocked loads are counted per request and recorded on the `scrape_page` span as
/// `blocked_requests` (worker), which is the way to confirm from production logs that a list is
/// actually in force — a mistyped pattern otherwise looks exactly like a page with no trackers.
#[derive(Debug, Clone)]
pub struct BlockedUrlsMiddleware {
    patterns: Vec<String>,
}

impl BlockedUrlsMiddleware {
    /// Build the middleware from a list of URL patterns. See the type docs for the syntax.
    ///
    /// Patterns are stored and sent verbatim. A pattern containing no `*` is almost always a bare
    /// host that blocks far more than that host (see the type docs), so it is reported as a
    /// warning here — at construction, i.e. during worker startup.
    pub fn new(patterns: Vec<String>) -> Self {
        for pattern in patterns.iter().filter(|p| !p.contains('*')) {
            tracing::warn!(
                "Blocked-URL pattern '{}' contains no '*': it blocks every URL containing that \
                 string anywhere, other hosts and query strings included; a host is written as \
                 '*://{}/*'",
                pattern,
                pattern
            );
        }
        Self { patterns }
    }

    /// The patterns this middleware installs, in the order they were given.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

impl TabInitMiddleware for BlockedUrlsMiddleware {
    fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()> {
        if self.patterns.is_empty() {
            return Ok(());
        }

        use headless_chrome::protocol::cdp::Network;

        // `setBlockedURLs` acts on the Network domain, so it has to be enabled first. Enabling it
        // again later (the response observer does, once per request) does not clear the list.
        tab.call_method(Network::Enable {
            max_total_buffer_size: None,
            max_resource_buffer_size: None,
            max_post_data_size: None,
            report_direct_socket_traffic: None,
            enable_durable_messages: None,
        })?;

        tab.call_method(Network::SetBlockedURLs {
            urls: self.patterns.clone(),
        })?;

        tracing::debug!(
            "Installed blocked-URL list on tab: {} pattern(s)",
            self.patterns.len()
        );

        Ok(())
    }

    fn name(&self) -> &str {
        "blocked_urls"
    }

    fn clone_box(&self) -> Box<dyn TabInitMiddleware> {
        Box::new(self.clone())
    }
}

/// Environment variable read by [`BlockedResourceTypesMiddleware::from_env`]: a comma-separated
/// list of CDP resource type names, case-insensitive (`media`, `media,font`).
pub const BLOCKED_RESOURCE_TYPES_ENV: &str = "WORKER_BLOCKED_RESOURCE_TYPES";

/// How a resource type behaves when blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeSupport {
    /// Nothing on a page depends on it for content.
    Safe,
    /// Accepted, but blocking it can break pages or trip anti-bot checks. Allowed — the choice is
    /// the deployment's — and warned about at startup.
    Risky,
    /// Chrome's `Fetch.enable` rejects it (`Unknown resource type in fetch filter`), and one such
    /// type fails the whole call, i.e. disables every other type too. Dropped at startup.
    Unsupported,
}

/// Every `Network.ResourceType`, by its lowercase CDP name.
///
/// `Unsupported` was measured against Chrome (2026-09-22), not taken from the protocol docs.
/// `prefetch` is also useless as a type: a `<link rel=prefetch>` load arrives in
/// `Fetch.requestPaused` as `Fetch`.
const RESOURCE_TYPES: &[(&str, ResourceType, TypeSupport)] = {
    use ResourceType as R;
    use TypeSupport::*;
    &[
        ("document", R::Document, Risky),
        ("stylesheet", R::Stylesheet, Risky),
        ("image", R::Image, Risky),
        ("media", R::Media, Safe),
        ("font", R::Font, Safe),
        ("script", R::Script, Risky),
        ("texttrack", R::TextTrack, Unsupported),
        ("xhr", R::Xhr, Risky),
        ("fetch", R::Fetch, Risky),
        ("prefetch", R::Prefetch, Unsupported),
        ("eventsource", R::EventSource, Risky),
        ("websocket", R::WebSocket, Unsupported),
        ("manifest", R::Manifest, Unsupported),
        ("signedexchange", R::SignedExchange, Unsupported),
        ("ping", R::Ping, Risky),
        ("cspviolationreport", R::CspViolationReport, Safe),
        ("preflight", R::Preflight, Unsupported),
        ("fedcm", R::FedCm, Unsupported),
        ("other", R::Other, Risky),
    ]
};

/// One resource type blocked by [`BlockedResourceTypesMiddleware`].
#[derive(Debug, Clone, PartialEq)]
pub struct BlockedResourceType {
    pub resource_type: ResourceType,
    /// Lowercase CDP name, used as the metric label.
    pub label: &'static str,
}

impl BlockedResourceType {
    /// The `Fetch.enable` pattern that pauses loads of this type.
    pub fn fetch_pattern(&self) -> headless_chrome::protocol::cdp::Fetch::RequestPattern {
        headless_chrome::protocol::cdp::Fetch::RequestPattern {
            url_pattern: None,
            resource_Type: Some(self.resource_type.clone()),
            request_stage: None,
        }
    }
}

/// Drops every load of the configured resource types (`<video>`/`<audio>` for `media`) before it
/// leaves the browser.
///
/// # Why Fetch and not `Network.setBlockedURLs`
///
/// `setBlockedURLs` matches URLs only; the resource type is known to Fetch interception.
///
/// # This middleware does not enable Fetch — the worker does
///
/// It only installs the tab's request interceptor, a local setting with no CDP call. `Fetch.enable`
/// has exactly one caller, the worker's request path (`enable_fetch` in `worker/src/service.rs`),
/// which learns the types from [`TabInitMiddleware::blocked_resource_types`]: with proxy
/// credentials it enables Fetch for every load with auth handling (as it always did), otherwise
/// with one pattern per blocked type, so nothing else is paused.
///
/// ⚠️ Two callers would not be safe in either order. Each `Fetch.enable` replaces the previous
/// configuration, and Chrome answers the proxy's 407 only for requests matching the current
/// patterns — measured 2026-09-22 with an authenticating proxy: enabling this type's patterns after
/// the worker's auth call failed every navigation with `ERR_INVALID_AUTH_CREDENTIALS`, with
/// `handleAuthRequests: true` passed along or not. The request fails; it does not bypass the proxy.
///
/// A blocked load is reported by Chrome exactly like one dropped by [`BlockedUrlsMiddleware`]
/// (`blockedReason: inspector`, `net::ERR_BLOCKED_BY_CLIENT.Inspector`), so it is counted in
/// `blocked_requests` and the third-party blocked metric, and kept out of diagnostics, with no
/// extra code.
///
/// # Limits
///
/// - **Streaming video is not caught**: HLS/DASH players fetch segments as `XHR`/`Fetch`.
/// - **Cross-site iframes are not reached**, the same limit as [`BlockedUrlsMiddleware`] — an
///   embedded player runs in its own renderer.
/// - **A tab has one request interceptor.** Another middleware calling
///   `enable_request_interception` replaces this one's, silently.
/// - **Blocks nothing outside the browser-hive worker**, since Fetch is enabled there.
///
/// # Configuration
///
/// Types are given by CDP name, case-insensitive. Unknown names and types Chrome's Fetch filter
/// rejects are dropped with a WARN; types that can break pages (`image`, `script`, `stylesheet`,
/// `document`, `xhr`, `fetch`, …) are **kept** with a WARN. An empty list makes it a no-op.
#[derive(Debug, Clone)]
pub struct BlockedResourceTypesMiddleware {
    types: Vec<BlockedResourceType>,
}

impl BlockedResourceTypesMiddleware {
    /// Build the middleware from resource type names. Problems are logged here, at worker startup.
    pub fn new(names: Vec<String>) -> Self {
        let mut types: Vec<BlockedResourceType> = Vec::new();
        for name in names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
            let lowercase = name.to_ascii_lowercase();
            let Some((label, resource_type, support)) =
                RESOURCE_TYPES.iter().find(|(l, _, _)| *l == lowercase)
            else {
                tracing::warn!("Blocked resource type '{}' is unknown and is ignored", name);
                continue;
            };
            match support {
                TypeSupport::Unsupported => {
                    tracing::warn!(
                        "Blocked resource type '{}' is not accepted by Chrome's Fetch filter and \
                         is ignored",
                        name
                    );
                    continue;
                }
                TypeSupport::Risky => tracing::warn!(
                    "Blocked resource type '{}' can break pages or trip anti-bot checks",
                    name
                ),
                TypeSupport::Safe => {}
            }
            if types.iter().all(|t| t.label != *label) {
                types.push(BlockedResourceType {
                    resource_type: resource_type.clone(),
                    label,
                });
            }
        }
        Self { types }
    }

    /// Build from [`BLOCKED_RESOURCE_TYPES_ENV`]; unset or empty blocks nothing.
    pub fn from_env() -> Self {
        let raw = std::env::var(BLOCKED_RESOURCE_TYPES_ENV).unwrap_or_default();
        Self::new(raw.split(',').map(str::to_string).collect())
    }
}

impl TabInitMiddleware for BlockedResourceTypesMiddleware {
    fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()> {
        if self.types.is_empty() {
            return Ok(());
        }

        use headless_chrome::browser::tab::RequestPausedDecision;
        use headless_chrome::protocol::cdp::Fetch;
        use headless_chrome::protocol::cdp::Network::ErrorReason;

        // No `Fetch.enable` here: the worker owns it (see the type docs). A load that is paused
        // for any other reason (proxy auth pauses every load) and is not of a blocked type goes on.
        let blocked: Vec<ResourceType> =
            self.types.iter().map(|t| t.resource_type.clone()).collect();
        tab.enable_request_interception(std::sync::Arc::new(
            move |_transport, _session_id, event: Fetch::events::RequestPausedEvent| {
                if blocked.contains(&event.params.resource_Type) {
                    RequestPausedDecision::Fail(Fetch::FailRequest {
                        request_id: event.params.request_id,
                        error_reason: ErrorReason::BlockedByClient,
                    })
                } else {
                    RequestPausedDecision::Continue(None)
                }
            },
        ))?;

        tracing::debug!(
            "Installed blocked resource type interceptor on tab: {:?}",
            self.types.iter().map(|t| t.label).collect::<Vec<_>>()
        );
        Ok(())
    }

    fn name(&self) -> &str {
        "blocked_resource_types"
    }

    fn clone_box(&self) -> Box<dyn TabInitMiddleware> {
        Box::new(self.clone())
    }

    fn blocked_resource_types(&self) -> &[BlockedResourceType] {
        &self.types
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Patterns reach CDP exactly as written. This is the decision the type documents: the base
    /// never reinterprets a pattern into the rule it guesses was meant. A bare host, which blocks
    /// every URL containing it, is reported as a warning instead, so the mistake is loud rather
    /// than corrected into a different rule.
    #[test]
    fn patterns_are_stored_verbatim() {
        let given = vec![
            "*://example.com/*".to_string(),
            "example.com".to_string(),
            "*://*.example.test/beacon.js*".to_string(),
        ];
        let middleware = BlockedUrlsMiddleware::new(given.clone());
        assert_eq!(middleware.patterns(), given.as_slice());
    }

    #[test]
    fn empty_list_is_accepted() {
        assert!(BlockedUrlsMiddleware::new(Vec::new()).patterns().is_empty());
    }

    fn labels(names: &[&str]) -> Vec<&'static str> {
        BlockedResourceTypesMiddleware::new(names.iter().map(|n| n.to_string()).collect())
            .blocked_resource_types()
            .iter()
            .map(|t| t.label)
            .collect()
    }

    /// Unknown names and types Chrome's Fetch filter rejects are dropped: passing one to
    /// `Fetch.enable` would fail the call and disable every other type with it. Risky types are
    /// the deployment's choice and stay. Names are case-insensitive, blanks and repeats collapse.
    #[test]
    fn resource_types_are_filtered_and_normalised() {
        assert_eq!(
            labels(&[
                " Media",
                "FONT",
                "",
                "media",
                "video",
                "prefetch",
                "textTrack",
                "image"
            ]),
            vec!["media", "font", "image"]
        );
        assert!(labels(&[]).is_empty());
    }

    /// Every entry of the table carries the enum value its name stands for, so the label on the
    /// metric and the type sent to CDP cannot drift apart.
    #[test]
    fn resource_type_table_matches_cdp_names() {
        for (label, resource_type, _) in RESOURCE_TYPES {
            let cdp_name = serde_json::to_value(resource_type).unwrap();
            assert_eq!(cdp_name.as_str().unwrap().to_ascii_lowercase(), *label);
        }
    }
}
