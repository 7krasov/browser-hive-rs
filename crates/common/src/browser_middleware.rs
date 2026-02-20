use anyhow::Result;
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
