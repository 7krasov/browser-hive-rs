//! Request Context DTOs
//!
//! This module provides structured data transfer objects for passing request
//! parameters through the system. Using separate DTOs instead of adding
//! individual parameters makes the API more extensible and maintainable.
//!
//! # Architecture
//!
//! ```text
//! Client Request
//!       │
//!       ▼
//! ┌─────────────────┐
//! │ RequestContext  │ ← Combined container for all request parameters
//! │  ├─ ray_id      │
//! │  ├─ ProxyParams │ ← Parameters affecting proxy selection
//! │  └─ ScrapeParams│ ← Parameters affecting scraping behavior
//! └─────────────────┘
//!       │
//!       ▼
//! Coordinator → Worker → BrowserPool
//! ```

use serde::{Deserialize, Serialize};

/// Parameters affecting proxy selection
///
/// This struct groups all parameters that influence how proxies are selected
/// or configured for a request. New proxy-related parameters should be added here.
///
/// # Examples
///
/// ```rust
/// use browser_hive_common::ProxyParams;
///
/// // Request specific country
/// let params = ProxyParams {
///     country_code: Some("US".to_string()),
/// };
///
/// // No country preference (use default)
/// let default_params = ProxyParams::default();
/// assert!(default_params.country_code.is_none());
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyParams {
    /// ISO 3166-1 alpha-2 country code for proxy geo-targeting
    ///
    /// Examples: "US", "DE", "GB", "UA"
    /// If None, the proxy provider uses its default behavior.
    pub country_code: Option<String>,
    // Future extensions:
    // pub city: Option<String>,
    // pub session_type: Option<SessionType>, // sticky vs rotating
    // pub asn: Option<u32>,
}

impl ProxyParams {
    /// Create new ProxyParams with specified country code
    pub fn with_country(country_code: impl Into<String>) -> Self {
        Self {
            country_code: Some(country_code.into()),
        }
    }

    /// Check if any proxy parameters are specified
    pub fn has_overrides(&self) -> bool {
        self.country_code.is_some()
    }
}

/// Parameters affecting scraping behavior
///
/// This struct groups all parameters that influence how a page is scraped,
/// including wait strategies, timeouts, and selectors.
///
/// # Examples
///
/// ```rust
/// use browser_hive_common::ScrapeParams;
///
/// let params = ScrapeParams {
///     wait_strategy: "network_idle".to_string(),
///     wait_timeout_ms: 5000,
///     wait_selector: Some(".content-loaded".to_string()),
///     skip_selector: Some(".captcha".to_string()),
/// };
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScrapeParams {
    /// Wait strategy for page loading (e.g., "network_idle", "timeout")
    pub wait_strategy: String,

    /// Timeout in milliseconds for wait strategy
    pub wait_timeout_ms: u32,

    /// CSS selector to wait for after initial load
    pub wait_selector: Option<String>,

    /// CSS selector that indicates content should be skipped
    pub skip_selector: Option<String>,
}

impl ScrapeParams {
    /// Create ScrapeParams with default network_idle strategy
    pub fn network_idle() -> Self {
        Self {
            wait_strategy: "network_idle".to_string(),
            ..Default::default()
        }
    }

    /// Set wait selector
    pub fn with_wait_selector(mut self, selector: impl Into<String>) -> Self {
        self.wait_selector = Some(selector.into());
        self
    }

    /// Set skip selector
    pub fn with_skip_selector(mut self, selector: impl Into<String>) -> Self {
        self.skip_selector = Some(selector.into());
        self
    }

    /// Set timeout
    pub fn with_timeout(mut self, timeout_ms: u32) -> Self {
        self.wait_timeout_ms = timeout_ms;
        self
    }
}

/// Combined request context containing all parameters
///
/// This is the main container passed through the system, combining
/// the request identifier with all parameter groups.
///
/// # Examples
///
/// ```rust
/// use browser_hive_common::{RequestContext, ProxyParams, ScrapeParams};
///
/// let ctx = RequestContext {
///     ray_id: "ray_abc123".to_string(),
///     proxy: ProxyParams::with_country("DE"),
///     scrape: ScrapeParams::network_idle()
///         .with_wait_selector(".loaded")
///         .with_timeout(5000),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Request tracing ID for log correlation
    pub ray_id: String,

    /// Proxy-related parameters
    pub proxy: ProxyParams,

    /// Scraping-related parameters
    pub scrape: ScrapeParams,
}

impl RequestContext {
    /// Create a new RequestContext with the given ray_id
    pub fn new(ray_id: impl Into<String>) -> Self {
        Self {
            ray_id: ray_id.into(),
            proxy: ProxyParams::default(),
            scrape: ScrapeParams::default(),
        }
    }

    /// Set proxy parameters
    pub fn with_proxy(mut self, proxy: ProxyParams) -> Self {
        self.proxy = proxy;
        self
    }

    /// Set scrape parameters
    pub fn with_scrape(mut self, scrape: ScrapeParams) -> Self {
        self.scrape = scrape;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_params_default() {
        let params = ProxyParams::default();
        assert!(params.country_code.is_none());
        assert!(!params.has_overrides());
    }

    #[test]
    fn test_proxy_params_with_country() {
        let params = ProxyParams::with_country("US");
        assert_eq!(params.country_code, Some("US".to_string()));
        assert!(params.has_overrides());
    }

    #[test]
    fn test_scrape_params_builder() {
        let params = ScrapeParams::network_idle()
            .with_wait_selector(".content")
            .with_skip_selector(".captcha")
            .with_timeout(5000);

        assert_eq!(params.wait_strategy, "network_idle");
        assert_eq!(params.wait_selector, Some(".content".to_string()));
        assert_eq!(params.skip_selector, Some(".captcha".to_string()));
        assert_eq!(params.wait_timeout_ms, 5000);
    }

    #[test]
    fn test_request_context_builder() {
        let ctx = RequestContext::new("ray_123")
            .with_proxy(ProxyParams::with_country("DE"))
            .with_scrape(ScrapeParams::network_idle().with_timeout(3000));

        assert_eq!(ctx.ray_id, "ray_123");
        assert_eq!(ctx.proxy.country_code, Some("DE".to_string()));
        assert_eq!(ctx.scrape.wait_timeout_ms, 3000);
    }
}
