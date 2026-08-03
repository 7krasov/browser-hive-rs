use crate::request_context::ProxyParams;
use anyhow::Result;
use std::fmt::Debug;

/// Proxy protocol/scheme
#[derive(Clone, Debug, Default)]
pub enum ProxyScheme {
    /// HTTP proxy (default)
    #[default]
    Http,
    /// HTTPS proxy
    Https,
    /// SOCKS5 proxy
    Socks5,
}

impl ProxyScheme {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
        }
    }
}

/// Proxy configuration with flexible format support
///
/// This is the standardized config format that all ProxyProvider implementations
/// must return. The worker uses this to configure Chrome's proxy settings.
///
/// # Configuration Options
///
/// **Option 1: Complete URL** (recommended for complex setups)
/// ```rust,ignore
/// ProxyConfig {
///     proxy_url: Some("http://user:pass@proxy.example.com:8080".to_string()),
///     scheme: ProxyScheme::Http,
///     address: None,
///     port: None,
///     username: None,
///     password: None,
/// }
/// ```
///
/// **Option 2: Individual components** (easier for programmatic generation)
/// ```rust,ignore
/// ProxyConfig {
///     proxy_url: None,
///     scheme: ProxyScheme::Http,
///     address: Some("proxy.example.com".to_string()),
///     port: Some(8080),
///     username: Some("user".to_string()),
///     password: Some("pass".to_string()),
/// }
/// ```
///
/// **Option 3: No proxy** (direct connection)
/// ```rust,ignore
/// ProxyConfig {
///     proxy_url: None,
///     scheme: ProxyScheme::Http,
///     address: None,
///     port: None,
///     username: None,
///     password: None,
/// }
/// ```
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// Complete proxy URL with everything embedded (takes precedence if set)
    /// Format: "http://user:pass@host:port" or "http://host:port"
    pub proxy_url: Option<String>,

    /// Proxy protocol (used only when building from components)
    pub scheme: ProxyScheme,

    /// Proxy server address/hostname (used if proxy_url is None)
    pub address: Option<String>,

    /// Proxy server port (used if proxy_url is None)
    pub port: Option<u16>,

    /// Username for proxy authentication
    pub username: Option<String>,

    /// Password for proxy authentication
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Build the final proxy server string for Chrome
    ///
    /// Returns None if no proxy is configured (direct connection)
    pub fn build_proxy_server(&self) -> Option<String> {
        if let Some(url) = &self.proxy_url {
            // Use complete URL, but strip embedded credentials
            // Chrome needs credentials via Fetch API, not in URL
            Some(self.strip_credentials_from_url(url))
        } else if let (Some(address), Some(port)) = (&self.address, &self.port) {
            // Build from components
            Some(format!("{}://{}:{}", self.scheme.as_str(), address, port))
        } else {
            // No proxy configured
            None
        }
    }

    /// Extract credentials from config
    ///
    /// Returns (username, password) if available from either proxy_url or separate fields
    pub fn get_credentials(&self) -> Option<(String, String)> {
        if let Some(url) = &self.proxy_url {
            // Try to extract from URL (format: scheme://user:pass@host:port)
            if let Some((user, pass)) = self.extract_credentials_from_url(url) {
                return Some((user, pass));
            }
        }

        // Use separate credential fields
        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            Some((user.clone(), pass.clone()))
        } else {
            None
        }
    }

    /// Check if proxy is enabled
    pub fn is_proxy_enabled(&self) -> bool {
        self.proxy_url.is_some() || (self.address.is_some() && self.port.is_some())
    }

    /// Strip credentials from URL for Chrome --proxy-server flag
    ///
    /// Chrome doesn't support embedded credentials in proxy URL for HTTPS sites,
    /// so we must remove "user:pass@" and handle auth via Fetch API instead.
    fn strip_credentials_from_url(&self, url: &str) -> String {
        if let Some(at_pos) = url.find('@') {
            if let Some(scheme_end) = url.find("://") {
                // Rebuild URL without credentials: scheme://host:port
                return format!("{}{}", &url[..scheme_end + 3], &url[at_pos + 1..]);
            }
        }
        // No credentials embedded or malformed URL - return as-is
        url.to_string()
    }

    /// Extract credentials from proxy URL
    ///
    /// Parses "scheme://user:pass@host:port" format
    fn extract_credentials_from_url(&self, url: &str) -> Option<(String, String)> {
        if let Some(at_pos) = url.find('@') {
            if let Some(scheme_end) = url.find("://") {
                let creds = &url[scheme_end + 3..at_pos];
                if let Some(colon_pos) = creds.find(':') {
                    let user = creds[..colon_pos].to_string();
                    let pass = creds[colon_pos + 1..].to_string();
                    return Some((user, pass));
                }
            }
        }
        None
    }
}

/// Trait for custom proxy provider implementations
///
/// Implement this trait to add your own proxy providers to Browser Hive
/// without modifying the core codebase.
///
/// # Example
///
/// ```rust
/// use browser_hive_common::proxy::{ProxyConfig, ProxyProvider, ProxyScheme};
/// use browser_hive_common::ProxyParams;
/// use anyhow::Result;
///
/// #[derive(Debug, Clone)]
/// struct MyProxyProvider {
///     endpoint: String,
///     api_key: String,
/// }
///
/// impl ProxyProvider for MyProxyProvider {
///     fn build_config(&self) -> Result<ProxyConfig> {
///         Ok(ProxyConfig {
///             proxy_url: None,
///             scheme: ProxyScheme::Http,
///             address: Some(self.endpoint.clone()),
///             port: Some(8080),
///             username: Some("user".to_string()),
///             password: Some(self.api_key.clone()),
///         })
///     }
///
///     fn name(&self) -> &str {
///         "my_provider"
///     }
///
///     fn clone_box(&self) -> Box<dyn ProxyProvider> {
///         Box::new(self.clone())
///     }
/// }
/// ```
pub trait ProxyProvider: Debug + Send + Sync {
    /// Build proxy configuration from this provider
    ///
    /// This method is called when creating a browser context to get
    /// the proxy settings. It can perform any necessary transformations
    /// (e.g., template substitution, country selection, etc.)
    fn build_config(&self) -> Result<ProxyConfig>;

    /// Build proxy configuration with request-specific parameters
    ///
    /// This method allows the proxy provider to customize the proxy based on
    /// request parameters like country_code. Providers should override this
    /// if they support geo-targeting.
    ///
    /// Default implementation ignores params and calls build_config().
    ///
    /// # Parameters
    /// * `params` - Request-specific proxy parameters (country_code, etc.)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// fn build_config_with_params(&self, params: &ProxyParams) -> Result<ProxyConfig> {
    ///     let mut config = self.build_config()?;
    ///
    ///     // Add country to proxy URL if specified
    ///     if let Some(country) = &params.country_code {
    ///         config.username = config.username.map(|u| format!("{}-country-{}", u, country));
    ///     }
    ///
    ///     Ok(config)
    /// }
    /// ```
    fn build_config_with_params(&self, params: &ProxyParams) -> Result<ProxyConfig> {
        let _ = params; // Unused in default implementation
        self.build_config()
    }

    /// Get unique identifier for this provider (used in logging)
    fn name(&self) -> &str;

    /// Clone this provider into a Box
    ///
    /// Required for trait objects to be cloneable.
    /// Standard implementation: `Box::new(self.clone())`
    fn clone_box(&self) -> Box<dyn ProxyProvider>;

    /// Check if this provider supports per-context proxy assignment
    ///
    /// Default: false (all contexts use the same proxy from build_config)
    /// Override to return true for providers with proxy pools
    ///
    /// **This means per-context *credentials*, not a per-context proxy host.** The config
    /// returned by [`get_context_proxy`](Self::get_context_proxy) is read for its credentials
    /// only; its host is discarded. That is the right model for gateway providers, where every
    /// exit lives behind one host and the identity of the exit is encoded in the username
    /// (session ID, country, …). A provider whose pool encodes identity in the *host* must also
    /// override [`assigns_proxy_host_per_context`](Self::assigns_proxy_host_per_context),
    /// otherwise its pool is silently unused.
    fn supports_per_context_proxy(&self) -> bool {
        false
    }

    /// Whether the host of the per-context config must actually route the context's traffic.
    ///
    /// Default: false — the browser-wide `--proxy-server` carries every context, and only the
    /// credentials from [`get_context_proxy`](Self::get_context_proxy) vary per context.
    ///
    /// Override to `true` only for providers whose pool is a list of distinct proxy *hosts*
    /// (dedicated IP pools). The worker then creates each CDP BrowserContext with its own
    /// `proxyServer`, so the pool is genuinely rotated instead of one entry being picked at
    /// browser launch and used by every request until the process restarts.
    ///
    /// # Constraints when this returns true
    ///
    /// * **Requires [`supports_per_context_proxy`](Self::supports_per_context_proxy) to be
    ///   `true` as well.** The host is read off the config
    ///   [`get_context_proxy`](Self::get_context_proxy) returns, and that call only happens when
    ///   the other flag is set. The worker refuses to start on this flag alone.
    /// * **Requires `ContextIsolation::Isolated`.** Shared isolation runs every request in the
    ///   browser's default context, which no CDP call can re-point at another proxy. The worker
    ///   refuses to start on that combination rather than silently serving one host.
    /// * **`build_config()` must still return a usable proxy.** It launches the browser and
    ///   therefore carries the default context. Returning nothing here would send any traffic
    ///   outside a per-context context — including a `shared`-mode request — straight out of the
    ///   pod's own IP.
    /// * **Credentials still travel over CDP** (`Fetch.authRequired`), unchanged by this flag.
    fn assigns_proxy_host_per_context(&self) -> bool {
        false
    }

    /// Whether this provider is allowed to leave the browser without a proxy at all.
    ///
    /// Default: `false` — a provider whose [`build_config`](Self::build_config) yields no usable
    /// proxy server aborts worker startup. One browser-wide `--proxy-server` carrying every
    /// context is a legitimate mode (it is what every gateway provider uses, and what every
    /// provider that does not override
    /// [`assigns_proxy_host_per_context`](Self::assigns_proxy_host_per_context) gets); *no*
    /// proxy is a legitimate mode too, but only when it is asked for. The two are one typo apart:
    /// an unparseable port, an empty `PROXY_URL` or a pool that came back empty all reduce to the
    /// same "no proxy server" value, and the browser then scrapes every URL from the pod's own
    /// public IP with nothing but an INFO line to say so.
    ///
    /// Fail-closed is the default because the failure is invisible in the right direction: pages
    /// load, selectors match, requests succeed. It surfaces only as the origin's blocklist
    /// learning the egress IP of the cluster.
    ///
    /// Override to `true` only for a provider whose *purpose* is a direct connection (the base
    /// worker's `NoProxyProvider`, used for local development).
    ///
    /// Note this is about the **absence** of a proxy, not about how many there are. A provider
    /// that returns one host for the whole browser passes this check with nothing to override.
    fn allows_direct_connection(&self) -> bool {
        false
    }

    /// Get a context-specific proxy configuration
    ///
    /// Only called if supports_per_context_proxy() returns true.
    /// Used by providers with proxy pools to assign different proxies to each context.
    ///
    /// # Parameters
    /// * `context_id` - The unique identifier for the browser context (can be used for session stickiness)
    fn get_context_proxy(&self, context_id: &str) -> Option<ProxyConfig> {
        let _ = context_id; // Unused in default implementation
        None
    }

    /// Get a context-specific proxy configuration with request parameters
    ///
    /// Only called if supports_per_context_proxy() returns true.
    /// This variant accepts ProxyParams for geo-targeting support.
    ///
    /// # Parameters
    /// * `context_id` - The unique identifier for the browser context
    /// * `params` - Request-specific proxy parameters (country_code, etc.)
    fn get_context_proxy_with_params(
        &self,
        context_id: &str,
        params: &ProxyParams,
    ) -> Option<ProxyConfig> {
        let _ = params; // Unused in default implementation
        self.get_context_proxy(context_id)
    }
}

/// Make Box<dyn ProxyProvider> cloneable
impl Clone for Box<dyn ProxyProvider> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
