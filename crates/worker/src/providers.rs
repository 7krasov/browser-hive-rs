// Built-in proxy provider implementations
//
// These providers are included out-of-the-box for common use cases.
// Users can also implement custom providers in their own code.

use anyhow::{bail, Context, Result};
use browser_hive_common::proxy::{ProxyConfig, ProxyProvider, ProxyScheme};
use std::env;

/// No-proxy provider for direct connections
///
/// Use this when you don't need a proxy. The browser will connect directly.
///
/// # Example
///
/// ```rust
/// use browser_hive_worker::providers::NoProxyProvider;
///
/// let provider = NoProxyProvider;
/// ```
#[derive(Debug, Clone)]
pub struct NoProxyProvider;

impl ProxyProvider for NoProxyProvider {
    fn build_config(&self) -> Result<ProxyConfig> {
        Ok(ProxyConfig {
            proxy_url: None,
            scheme: ProxyScheme::default(),
            address: None,
            port: None,
            username: None,
            password: None,
        })
    }

    fn name(&self) -> &str {
        "no_proxy"
    }

    fn clone_box(&self) -> Box<dyn ProxyProvider> {
        Box::new(self.clone())
    }
}

/// Generic HTTP proxy provider with environment variable configuration
///
/// This provider reads proxy settings from environment variables, making it
/// easy to configure for local development and testing.
///
/// # Environment Variables
///
/// - `PROXY_URL` - Complete proxy URL (e.g., "http://user:pass@proxy.com:8080")
///   OR use individual components:
/// - `PROXY_ADDRESS` - Proxy server address (e.g., "proxy.example.com")
/// - `PROXY_PORT` - Proxy server port (e.g., "8080")
/// - `PROXY_USERNAME` - Optional username for authentication
/// - `PROXY_PASSWORD` - Optional password for authentication
/// - `PROXY_SCHEME` - Optional scheme ("http", "https", "socks5"). Default: "http"
///
/// # Example
///
/// ```rust,ignore
/// use browser_hive_worker::providers::GenericProxyProvider;
/// use browser_hive_common::ProxyScheme;
///
/// // From environment variables
/// let provider = GenericProxyProvider::from_env()?;
///
/// // Or construct manually
/// let provider = GenericProxyProvider {
///     proxy_url: None,
///     scheme: ProxyScheme::Http,
///     address: Some("proxy.example.com".to_string()),
///     port: Some(8080),
///     username: Some("user".to_string()),
///     password: Some("pass".to_string()),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct GenericProxyProvider {
    pub proxy_url: Option<String>,
    pub scheme: ProxyScheme,
    pub address: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl GenericProxyProvider {
    /// Create provider from environment variables
    pub fn from_env() -> Result<Self> {
        // Check if complete URL is provided
        if let Ok(url) = env::var("PROXY_URL") {
            return Ok(Self {
                proxy_url: Some(url),
                scheme: ProxyScheme::default(),
                address: None,
                port: None,
                username: None,
                password: None,
            });
        }

        // Otherwise, build from components
        let address = env::var("PROXY_ADDRESS").ok();
        let port = env::var("PROXY_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok());

        let username = env::var("PROXY_USERNAME").ok();
        let password = env::var("PROXY_PASSWORD").ok();

        let scheme = env::var("PROXY_SCHEME")
            .ok()
            .and_then(|s| match s.to_lowercase().as_str() {
                "http" => Some(ProxyScheme::Http),
                "https" => Some(ProxyScheme::Https),
                "socks5" => Some(ProxyScheme::Socks5),
                _ => None,
            })
            .unwrap_or_default();

        Ok(Self {
            proxy_url: None,
            scheme,
            address,
            port,
            username,
            password,
        })
    }
}

impl ProxyProvider for GenericProxyProvider {
    fn build_config(&self) -> Result<ProxyConfig> {
        Ok(ProxyConfig {
            proxy_url: self.proxy_url.clone(),
            scheme: self.scheme.clone(),
            address: self.address.clone(),
            port: self.port,
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }

    fn name(&self) -> &str {
        "generic_proxy"
    }

    fn clone_box(&self) -> Box<dyn ProxyProvider> {
        Box::new(self.clone())
    }
}

/// Create a proxy provider from environment variables
///
/// This is a helper function for docker-compose and local development.
/// Production deployments should build providers programmatically.
///
/// # Environment Variables
///
/// - `PROXY_TYPE` - Type of proxy to use:
///   - `"none"` or `"no_proxy"` - Direct connection without proxy
///   - `"generic"` or `"default"` - Generic HTTP proxy (requires PROXY_* vars)
///
/// # Example
///
/// ```bash
/// # No proxy
/// PROXY_TYPE=none
///
/// # Generic proxy with URL
/// PROXY_TYPE=generic
/// PROXY_URL=http://user:pass@proxy.example.com:8080
///
/// # Generic proxy with components
/// PROXY_TYPE=generic
/// PROXY_ADDRESS=proxy.example.com
/// PROXY_PORT=8080
/// PROXY_USERNAME=user
/// PROXY_PASSWORD=pass
/// PROXY_SCHEME=http
/// ```
pub fn create_from_env() -> Result<Box<dyn ProxyProvider>> {
    let proxy_type = env::var("PROXY_TYPE").unwrap_or_else(|_| "none".to_string());

    match proxy_type.to_lowercase().as_str() {
        "none" | "no_proxy" => Ok(Box::new(NoProxyProvider)),
        "generic" | "default" => {
            let provider = GenericProxyProvider::from_env()
                .context("Failed to create GenericProxyProvider from environment")?;
            Ok(Box::new(provider))
        }
        "" => {
            bail!("PROXY_TYPE not set. Set to 'none' for no proxy or 'generic' for HTTP proxy.")
        }
        _ => {
            bail!(
                "Unknown PROXY_TYPE: '{}'. Supported types: none, generic",
                proxy_type
            )
        }
    }
}
