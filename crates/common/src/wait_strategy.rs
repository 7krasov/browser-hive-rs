//! Wait strategy system for controlling page load behavior
//!
//! This module provides a trait-based system for defining how the browser
//! should wait for page loading to complete. Users can implement custom
//! strategies for their specific needs.

use anyhow::Result;
use headless_chrome::Tab;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tokio_cancellation_ext::{check_cancellation, CancellationToken};

/// Maximum allowed timeout for any wait strategy (40 seconds = 40000 milliseconds)
/// Client-specified timeout will be capped at this value
pub const MAX_WAIT_TIMEOUT_MS: u32 = 40_000;

/// Result of a wait strategy execution
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResult {
    /// Wait completed successfully (page loaded, selector found, or normal completion)
    Success,
    /// Skip selector was found - content should be returned with ERROR_CODE_SKIP_SELECTOR_FOUND
    SkipSelectorFound,
    /// Wait selector was specified but not found within timeout - content should be returned with error
    WaitSelectorNotFound,
}

/// Default timeout if not specified for navigation (40 seconds = 40000 milliseconds)
/// This is also the total timeout cap - client cannot exceed this value
pub const DEFAULT_WAIT_TIMEOUT_MS: u32 = 40_000;

/// HTTP status codes that trigger early exit (don't wait for network idle)
/// These are typically error pages that may have continuous network activity
const EARLY_EXIT_STATUS_CODES: &[u32] = &[403];

/// Result of checking if a selector exists in the page
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelectorCheckResult {
    /// Whether the selector was found in the DOM
    is_selector_found: bool,
    /// Error that occurred during selector search (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    selector_search_error: Option<String>,
}

/// Helper function to check if a CSS selector exists in the page
///
/// Returns true if at least one element matching the selector is found.
/// Returns an error if the JavaScript execution failed or returned unexpected format.
fn check_selector_exists(tab: &Arc<Tab>, selector: &str, ray_id: &str) -> Result<bool> {
    // Use JavaScript to check if selector exists with proper error handling
    // This is more reliable than CDP's querySelector which can have timing issues
    let script = format!(
        r#"(function() {{
            try {{
                return JSON.stringify({{
                    is_selector_found: document.querySelector({}) !== null
                }});
            }} catch (e) {{
                return JSON.stringify({{
                    is_selector_found: false,
                    selector_search_error: e.toString()
                }});
            }}
        }})()"#,
        serde_json::to_string(selector).unwrap_or_else(|_| "''".to_string())
    );

    let eval_result = tab.evaluate(&script, false)?;

    if let Some(value) = eval_result.value {
        if let Some(json_str) = value.as_str() {
            let check_result: SelectorCheckResult = serde_json::from_str(json_str)
                .map_err(|e| anyhow::anyhow!("Failed to parse selector check result: {}", e))?;

            if let Some(error) = &check_result.selector_search_error {
                tracing::warn!(ray_id = %ray_id, "Selector '{}' search error: {}", selector, error);
            } else if !check_result.is_selector_found {
                tracing::debug!(ray_id = %ray_id, "Selector '{}' not found", selector);
            }

            return Ok(check_result.is_selector_found);
        }
    }

    anyhow::bail!(
        "Failed to check selector '{}': unexpected response format",
        selector
    );
}

/// Trait for implementing custom wait strategies
///
/// Wait strategies control how the browser waits for page loading to complete
/// after navigation. Different strategies are useful for different types of pages:
/// - Static pages: `network_idle` is usually sufficient
/// - SPAs with heavy AJAX: `timeout` with fixed delay might be better
/// - Custom conditions: implement your own strategy
pub trait WaitStrategy: Send + Sync {
    /// Execute the wait strategy on the given tab
    ///
    /// This method is called after `tab.navigate_to()` and should wait
    /// until the page is considered "loaded" according to this strategy.
    ///
    /// # Arguments
    /// * `tab` - The Chrome tab to wait on
    /// * `timeout_ms` - Timeout value in milliseconds (0 means use default)
    /// * `wait_selector` - Optional CSS selector to wait for after page load
    /// * `skip_selector` - Optional CSS selector that indicates content should be skipped
    /// * `cancellation_token` - Token to check for graceful shutdown requests
    ///
    /// # Timeout Behavior (NetworkIdleStrategy)
    ///
    /// **Variant 1**: `timeout_ms=0` && `wait_selector` empty
    /// - Wait for network idle, maximum `DEFAULT_WAIT_TIMEOUT_MS`
    ///
    /// **Variant 2**: `timeout_ms=X` && `wait_selector` empty
    /// - Wait for network idle, maximum X ms (capped at `MAX_WAIT_TIMEOUT_MS`)
    ///
    /// **Variant 3**: `timeout_ms=0` && `wait_selector` present
    /// - Total timeout: `DEFAULT_WAIT_TIMEOUT_MS`
    /// - Wait for network idle → search for selector until total timeout
    ///
    /// **Variant 4**: `timeout_ms=X` && `wait_selector` present
    /// - Total timeout: `DEFAULT_WAIT_TIMEOUT_MS`
    /// - Wait for network idle → search for selector min(X ms, remaining time)
    ///
    /// # Selector Priority
    /// If both selectors are provided:
    /// 1. `skip_selector` is always checked first (highest priority)
    /// 2. Then `wait_selector` is checked
    /// 3. Final check performed even after timeout
    ///
    /// # Returns
    /// * `Ok(WaitResult::Success)` - Page loaded successfully and wait_selector found (if specified)
    /// * `Ok(WaitResult::SkipSelectorFound)` - skip_selector was found
    /// * `Ok(WaitResult::WaitSelectorNotFound)` - wait_selector was specified but not found
    /// * `Err(...)` - Fatal error occurred (network error, browser crash, cancellation, etc.)
    fn wait(
        &self,
        tab: &Arc<Tab>,
        timeout_ms: u32,
        wait_selector: Option<&str>,
        skip_selector: Option<&str>,
        cancellation_token: &CancellationToken,
        ray_id: &str,
    ) -> Result<WaitResult>;

    /// Get the name of this strategy (for logging and identification)
    fn name(&self) -> &str;
}

/// Network Idle strategy - waits until network is completely quiet
///
/// This is the default strategy. It waits for the `networkIdle` lifecycle event,
/// which fires when there have been no network requests for ~500ms.
///
/// # Behavior
///
/// **Without wait_selector**:
/// - Waits for network idle with timeout = effective_timeout(timeout_ms)
/// - Returns immediately after network idle
///
/// **With wait_selector**:
/// - Phase 1: Wait for network idle (within `DEFAULT_WAIT_TIMEOUT_MS` total)
/// - Phase 2: Search for selector after idle:
///   - If timeout_ms=0: search until total timeout
///   - If timeout_ms=X: search for min(X, remaining time until total timeout)
/// - Always performs final selector check before returning
///
/// **Early exit**: On HTTP 403 and other error codes, exits early to avoid
/// waiting for network idle (which may never occur on CAPTCHA/error pages).
///
/// Best for: Most web pages, SPAs, pages requiring dynamic content to load
/// Not ideal for: Pages with continuous polling (use timeout strategy instead)
#[derive(Debug, Clone, Default)]
pub struct NetworkIdleStrategy;

impl WaitStrategy for NetworkIdleStrategy {
    fn wait(
        &self,
        tab: &Arc<Tab>,
        timeout_ms: u32,
        wait_selector: Option<&str>,
        skip_selector: Option<&str>,
        cancellation_token: &CancellationToken,
        ray_id: &str,
    ) -> Result<WaitResult> {
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_millis(500);

        // Determine total timeout based on whether wait_selector is present
        // Variant 1 & 2: no selector → use effective_timeout(timeout_ms)
        // Variant 3 & 4: selector present → total timeout is always DEFAULT_WAIT_TIMEOUT_MS
        let total_timeout_ms = if wait_selector.is_some() {
            DEFAULT_WAIT_TIMEOUT_MS
        } else {
            effective_timeout(timeout_ms)
        };

        let total_timeout = std::time::Duration::from_millis(total_timeout_ms as u64);
        tab.set_default_timeout(total_timeout);

        // Shared state for network idle detection: (is_finished, result)
        let idle_finished = StdArc::new(StdMutex::new((false, None::<Result<()>>)));

        // Spawn background thread for network idle detection
        let tab_clone = tab.clone();
        let idle_finished_clone = idle_finished.clone();
        std::thread::spawn(move || {
            let result = tab_clone
                .wait_until_navigated()
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("Navigation failed: {}", e));

            let mut guard = idle_finished_clone.lock().unwrap();
            guard.0 = true;
            guard.1 = Some(result);
        });

        // Phase 1: Wait for network idle
        loop {
            // Check for cancellation at the start of each iteration
            check_cancellation(cancellation_token, "wait_strategy:phase1")?;

            // Always check skip_selector first (highest priority)
            if let Some(selector) = skip_selector {
                if check_selector_exists(tab, selector, ray_id)? {
                    tracing::info!(
                        ray_id = %ray_id,
                        "Skip selector '{}' found after {:?}",
                        selector,
                        start.elapsed()
                    );
                    return Ok(WaitResult::SkipSelectorFound);
                }
            }

            // Check if network idle reached
            {
                let guard = idle_finished.lock().unwrap();
                if guard.0 {
                    match guard.1.as_ref().unwrap() {
                        Ok(_) => {
                            tracing::debug!(ray_id = %ray_id, "Network idle reached after {:?}", start.elapsed());
                            break; // Exit Phase 1, proceed to Phase 2
                        }
                        Err(e) => return Err(anyhow::anyhow!("{}", e)),
                    }
                }
            }

            // Check early exit status codes (403, etc.)
            if let Ok(result) = tab.evaluate(
                "performance.getEntriesByType('navigation')[0]?.responseStatus || 0",
                false,
            ) {
                if let Some(status) = result.value.and_then(|v| v.as_u64()) {
                    let status_u32 = status as u32;

                    if status_u32 > 0 && EARLY_EXIT_STATUS_CODES.contains(&status_u32) {
                        tracing::info!(
                            ray_id = %ray_id,
                            "Early exit: HTTP {} detected after {:?}",
                            status_u32,
                            start.elapsed()
                        );

                        // Return immediately - no point waiting for selectors on error pages
                        // The HTTP status code already indicates the page is blocked/error
                        return Ok(WaitResult::Success);
                    }
                }
            }

            // Check total timeout
            if start.elapsed() >= total_timeout {
                tracing::warn!(ray_id = %ray_id, "Total timeout reached while waiting for network idle");

                // If wait_selector specified, perform final check
                if let Some(selector) = wait_selector {
                    if check_selector_exists(tab, selector, ray_id)? {
                        tracing::info!(ray_id = %ray_id, "Wait selector '{}' found on final check", selector);
                        return Ok(WaitResult::Success);
                    }
                    return Ok(WaitResult::WaitSelectorNotFound);
                }

                return Err(anyhow::anyhow!("Timeout waiting for network idle"));
            }

            // Before sleeping, ensure we won't exceed total timeout
            let remaining = total_timeout.saturating_sub(start.elapsed());
            if remaining < poll_interval {
                // Not enough time left for another sleep - check one more time and exit
                tracing::debug!(
                    ray_id = %ray_id,
                    "Skipping sleep - only {:?} remaining (poll_interval: {:?})",
                    remaining,
                    poll_interval
                );
                // Loop will check timeout on next iteration and exit
                continue;
            }

            sleep(poll_interval);
        }

        // Phase 1.5: Ensure document is fully loaded (readyState = "complete")
        // Network idle doesn't guarantee that all async scripts have executed

        // Calculate remaining time for ready check (respect total_timeout)
        let elapsed_so_far = start.elapsed();
        let remaining_total = total_timeout.saturating_sub(elapsed_so_far);
        let max_ready_wait = std::cmp::min(
            std::time::Duration::from_secs(30), // Increased from 10 to 30 sec
            remaining_total,                    // But don't exceed total timeout
        );

        if max_ready_wait.as_millis() < 100 {
            // Less than 100ms remaining - skip ready check
            tracing::warn!(
                ray_id = %ray_id,
                "Skipping document ready check - only {:?} remaining until total timeout",
                remaining_total
            );
        } else {
            // Use event-based approach instead of polling
            let wait_script = format!(
                r#"
                (async function() {{
                    const result = await new Promise((resolve) => {{
                        if (document.readyState === 'complete') {{
                            resolve({{ state: 'already-complete', timeMs: 0 }});
                        }} else {{
                            const startTime = Date.now();
                            const timeout = setTimeout(() => {{
                                const elapsed = Date.now() - startTime;
                                resolve({{
                                    state: 'timeout-interactive',
                                    timeMs: elapsed
                                }});
                            }}, {});

                            window.addEventListener('load', () => {{
                                clearTimeout(timeout);
                                const elapsed = Date.now() - startTime;
                                resolve({{
                                    state: 'load-complete',
                                    timeMs: elapsed
                                }});
                            }}, {{ once: true }});
                        }}
                    }});

                    return JSON.stringify(result);
                }})()
                "#,
                max_ready_wait.as_millis()
            );

            match tab.evaluate(&wait_script, true) {
                // await=true to wait for Promise resolution
                Ok(result) => {
                    if let Some(value) = result.value {
                        if let Some(json_str) = value.as_str() {
                            #[derive(serde::Deserialize)]
                            struct LoadResult {
                                state: String,
                                #[serde(rename = "timeMs")]
                                time_ms: u64,
                            }

                            match serde_json::from_str::<LoadResult>(json_str) {
                                Ok(load_result) => {
                                    match load_result.state.as_str() {
                                        "already-complete" => {
                                            tracing::debug!(
                                                ray_id = %ray_id,
                                                "Document was already complete when Phase 1.5 started"
                                            );
                                        }
                                        "load-complete" => {
                                            tracing::debug!(
                                                ray_id = %ray_id,
                                                "Document 'load' event fired after {}ms in Phase 1.5",
                                                load_result.time_ms
                                            );
                                        }
                                        "timeout-interactive" => {
                                            tracing::warn!(
                                                ray_id = %ray_id,
                                                "Document still 'interactive' after {}ms - 'load' event never fired! Proceeding anyway",
                                                load_result.time_ms
                                            );

                                            // Diagnose why 'load' event didn't fire
                                            let diagnose_blocked = || -> Result<()> {
                                                let script = r#"
                                                    JSON.stringify({
                                                        readyState: document.readyState,
                                                        pendingScripts: Array.from(document.getElementsByTagName('script'))
                                                            .filter(s => !s.hasAttribute('async') && !s.hasAttribute('defer'))
                                                            .length,
                                                        pendingImages: Array.from(document.images)
                                                            .filter(img => !img.complete)
                                                            .length,
                                                        totalImages: document.images.length
                                                    })
                                                "#;

                                                if let Ok(result) = tab.evaluate(script, false) {
                                                    if let Some(value) = result.value {
                                                        if let Some(json) = value.as_str() {
                                                            tracing::warn!(ray_id = %ray_id, "Blocked resources diagnostics: {}", json);
                                                        }
                                                    }
                                                }
                                                Ok(())
                                            };

                                            let _ = diagnose_blocked();
                                        }
                                        _ => {
                                            tracing::warn!(
                                                ray_id = %ray_id,
                                                "Unknown load result state: '{}' - proceeding",
                                                load_result.state
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        ray_id = %ray_id,
                                        "Failed to parse load result JSON '{}': {} - proceeding anyway",
                                        json_str,
                                        e
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        ray_id = %ray_id,
                        "Failed to execute load event listener: {} - proceeding anyway",
                        e
                    );
                }
            }
        }

        // After Phase 1.5, check if we exceeded total timeout
        if start.elapsed() >= total_timeout {
            tracing::warn!(ray_id = %ray_id, "Total timeout exceeded after Phase 1.5 - skipping selector search");
            // If wait_selector was specified, this is a failure; otherwise success
            return if wait_selector.is_some() {
                Ok(WaitResult::WaitSelectorNotFound)
            } else {
                Ok(WaitResult::Success)
            };
        }

        // Phase 2: If wait_selector specified, search for it after network idle
        if let Some(selector) = wait_selector {
            let idle_elapsed = start.elapsed();
            let remaining = total_timeout.saturating_sub(idle_elapsed);

            // Calculate selector search timeout
            // Variant 3: timeout_ms=0 → use remaining time until total timeout
            // Variant 4: timeout_ms=X → use min(X, remaining time)
            let selector_timeout = if timeout_ms == 0 {
                remaining
            } else {
                let requested = std::time::Duration::from_millis(timeout_ms as u64);
                std::cmp::min(requested, remaining)
            };

            tracing::info!(
                ray_id = %ray_id,
                "Searching for selector '{}' for {:?} (total elapsed: {:?})",
                selector,
                selector_timeout,
                idle_elapsed
            );

            let selector_start = std::time::Instant::now();

            loop {
                // Check for cancellation at the start of each iteration
                check_cancellation(cancellation_token, "wait_strategy:phase2")?;

                // Check skip_selector
                if let Some(skip_sel) = skip_selector {
                    if check_selector_exists(tab, skip_sel, ray_id)? {
                        tracing::info!(ray_id = %ray_id, "Skip selector '{}' found", skip_sel);
                        return Ok(WaitResult::SkipSelectorFound);
                    }
                }

                // Check wait_selector
                if check_selector_exists(tab, selector, ray_id)? {
                    tracing::info!(
                        ray_id = %ray_id,
                        "Wait selector '{}' found after {:?} (total: {:?})",
                        selector,
                        selector_start.elapsed(),
                        start.elapsed()
                    );
                    return Ok(WaitResult::Success);
                }

                // Check selector timeout
                if selector_start.elapsed() >= selector_timeout {
                    tracing::warn!(
                        ray_id = %ray_id,
                        "Selector search timeout reached after {:?}",
                        selector_start.elapsed()
                    );

                    // Final check before returning
                    if check_selector_exists(tab, selector, ray_id)? {
                        tracing::info!(ray_id = %ray_id, "Wait selector '{}' found on final check", selector);
                        return Ok(WaitResult::Success);
                    }

                    return Ok(WaitResult::WaitSelectorNotFound);
                }

                // Check total timeout (safety net)
                if start.elapsed() >= total_timeout {
                    // Final check
                    if check_selector_exists(tab, selector, ray_id)? {
                        tracing::info!(ray_id = %ray_id, "Wait selector '{}' found on final timeout check", selector);
                        return Ok(WaitResult::Success);
                    }
                    return Ok(WaitResult::WaitSelectorNotFound);
                }

                // Before sleeping, ensure we won't exceed total timeout
                let remaining = total_timeout.saturating_sub(start.elapsed());
                if remaining < poll_interval {
                    // Not enough time left for another sleep - check one more time and exit
                    tracing::debug!(
                        ray_id = %ray_id,
                        "Skipping sleep in selector search - only {:?} remaining (poll_interval: {:?})",
                        remaining,
                        poll_interval
                    );
                    // Loop will check timeout on next iteration and exit
                    continue;
                }

                sleep(poll_interval);
            }
        }

        // No wait_selector - return success after network idle
        Ok(WaitResult::Success)
    }

    fn name(&self) -> &str {
        "network_idle"
    }
}

/// Timeout strategy - waits for a fixed duration
///
/// Unlike network_idle, this strategy simply waits for the specified timeout,
/// optionally polling for selectors during that period.
///
/// # Behavior
///
/// **Without wait_selector**:
/// - Waits for effective_timeout(timeout_ms) milliseconds
/// - Returns after timeout expires
///
/// **With wait_selector**:
/// - Polls for selector every 500ms for effective_timeout(timeout_ms) duration
/// - Returns immediately when selector is found
/// - Returns WaitSelectorNotFound if timeout expires without finding selector
///
/// **All cases**: Performs final selector check before returning
///
/// Best for: SPAs with predictable load times, pages where network_idle doesn't work
/// Not ideal for: Pages where you don't know load time (wastes time or times out early)
#[derive(Debug, Clone, Default)]
pub struct TimeoutStrategy;

impl WaitStrategy for TimeoutStrategy {
    fn wait(
        &self,
        tab: &Arc<Tab>,
        timeout_ms: u32,
        wait_selector: Option<&str>,
        skip_selector: Option<&str>,
        cancellation_token: &CancellationToken,
        ray_id: &str,
    ) -> Result<WaitResult> {
        // First wait for initial navigation to complete
        tab.wait_until_navigated()?;

        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(500);

        // Use effective_timeout: 0 → DEFAULT_WAIT_TIMEOUT_MS, X → min(X, MAX_WAIT_TIMEOUT_MS)
        let timeout_duration = Duration::from_millis(effective_timeout(timeout_ms) as u64);

        tracing::debug!(
            ray_id = %ray_id,
            "Timeout strategy: waiting {:?}, wait_selector={:?}, skip_selector={:?}",
            timeout_duration,
            wait_selector,
            skip_selector
        );

        loop {
            // Check for cancellation at the start of each iteration
            check_cancellation(cancellation_token, "wait_strategy:timeout")?;

            // Always check skip_selector first (highest priority)
            if let Some(selector) = skip_selector {
                if check_selector_exists(tab, selector, ray_id)? {
                    tracing::info!(
                        ray_id = %ray_id,
                        "Skip selector '{}' found after {:?}",
                        selector,
                        start.elapsed()
                    );
                    return Ok(WaitResult::SkipSelectorFound);
                }
            }

            // Check wait_selector
            if let Some(selector) = wait_selector {
                if check_selector_exists(tab, selector, ray_id)? {
                    tracing::info!(
                        ray_id = %ray_id,
                        "Wait selector '{}' found after {:?}",
                        selector,
                        start.elapsed()
                    );
                    return Ok(WaitResult::Success);
                }
            }

            // Check if timeout period has elapsed
            if start.elapsed() >= timeout_duration {
                tracing::info!(ray_id = %ray_id, "Timeout reached after {:?}", start.elapsed());

                // Final check before returning
                if let Some(selector) = wait_selector {
                    if check_selector_exists(tab, selector, ray_id)? {
                        tracing::info!(ray_id = %ray_id, "Wait selector '{}' found on final check", selector);
                        return Ok(WaitResult::Success);
                    }
                    return Ok(WaitResult::WaitSelectorNotFound);
                }

                return Ok(WaitResult::Success);
            }

            // Sleep before next poll
            sleep(poll_interval);
        }
    }

    fn name(&self) -> &str {
        "timeout"
    }
}

/// Registry for wait strategies
///
/// This struct manages available wait strategies and provides lookup by name.
/// Users can register custom strategies for use in their scraping operations.
#[derive(Default)]
pub struct WaitStrategyRegistry {
    strategies: std::collections::HashMap<String, Arc<dyn WaitStrategy>>,
}

impl WaitStrategyRegistry {
    /// Create a new registry with default strategies
    pub fn new() -> Self {
        let mut registry = Self {
            strategies: std::collections::HashMap::new(),
        };

        // Register built-in strategies
        registry.register(Arc::new(NetworkIdleStrategy));
        registry.register(Arc::new(TimeoutStrategy));

        registry
    }

    /// Register a custom wait strategy
    pub fn register(&mut self, strategy: Arc<dyn WaitStrategy>) {
        self.strategies
            .insert(strategy.name().to_string(), strategy);
    }

    /// Get a strategy by name
    ///
    /// Returns None if strategy is not found
    pub fn get(&self, name: &str) -> Option<Arc<dyn WaitStrategy>> {
        self.strategies.get(name).cloned()
    }

    /// Get default strategy (network_idle)
    pub fn default_strategy(&self) -> Arc<dyn WaitStrategy> {
        self.strategies
            .get("network_idle")
            .cloned()
            .unwrap_or_else(|| Arc::new(NetworkIdleStrategy))
    }

    /// List all registered strategy names
    pub fn list_strategies(&self) -> Vec<&str> {
        self.strategies.keys().map(|s| s.as_str()).collect()
    }
}

/// Validate wait timeout value
///
/// Returns error if timeout exceeds maximum allowed value
pub fn validate_timeout(timeout_ms: u32) -> Result<u32> {
    if timeout_ms > MAX_WAIT_TIMEOUT_MS {
        anyhow::bail!(
            "Wait timeout {} ms exceeds maximum allowed {} ms",
            timeout_ms,
            MAX_WAIT_TIMEOUT_MS
        );
    }
    Ok(timeout_ms)
}

/// Get effective timeout value
///
/// Returns provided timeout if valid, otherwise returns default
pub fn effective_timeout(timeout_ms: u32) -> u32 {
    if timeout_ms == 0 {
        DEFAULT_WAIT_TIMEOUT_MS
    } else if timeout_ms > MAX_WAIT_TIMEOUT_MS {
        MAX_WAIT_TIMEOUT_MS
    } else {
        timeout_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_default_strategies() {
        let registry = WaitStrategyRegistry::new();

        assert!(registry.get("network_idle").is_some());
        assert!(registry.get("timeout").is_some());
        assert!(registry.get("unknown").is_none());
    }

    #[test]
    fn test_validate_timeout() {
        assert!(validate_timeout(30_000).is_ok());
        assert!(validate_timeout(MAX_WAIT_TIMEOUT_MS).is_ok());
        assert!(validate_timeout(MAX_WAIT_TIMEOUT_MS + 1).is_err());
    }

    #[test]
    fn test_effective_timeout() {
        assert_eq!(effective_timeout(0), DEFAULT_WAIT_TIMEOUT_MS);
        assert_eq!(effective_timeout(30_000), 30_000);
        assert_eq!(
            effective_timeout(MAX_WAIT_TIMEOUT_MS + 10_000),
            MAX_WAIT_TIMEOUT_MS
        );
    }
}
