use crate::browser_pool::{BrowserContext, BrowserPool};
use crate::metrics::Metrics;
use anyhow::Result;
use browser_hive_common::{
    effective_timeout, utils, validate_timeout, ProxyParams, SessionMode, WaitResult,
    WaitStrategyRegistry, WorkerConfig, DEFAULT_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS,
};
use std::time::Duration;

/// Minimum time (in seconds) required to run browser diagnostics
/// If remaining time is less than this, diagnostics are skipped to ensure timely response
const MIN_TIME_FOR_DIAGNOSTICS_SECS: u64 = 2;

/// Hard timeout for get_content operation (seconds)
/// If Chrome hangs during content retrieval, we bail out after this time
const GET_CONTENT_TIMEOUT_SECS: u64 = 20;

/// Hard timeout for navigation operation (seconds)
/// If Chrome hangs during navigation, we bail out after this time
const NAVIGATION_TIMEOUT_SECS: u64 = 20;

/// Safety margin (seconds) added to wait strategy timeout for the outer hard timeout
/// This ensures we catch stuck CDP calls even if internal timeout doesn't fire
const WAIT_STRATEGY_TIMEOUT_MARGIN_SECS: u64 = 10;
use browser_hive_proto::worker::{
    worker_service_server::WorkerService as WorkerServiceTrait, ErrorCode, HealthCheckResponse,
    ScrapePageRequest, ScrapePageResponse, WorkerStatsResponse,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

/// Calculate SHA256 hash of content for compact logging
fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// Check proxy exit IP to detect geo-blocking patterns
/// Adds ~1 second overhead per request
#[allow(dead_code)]
fn check_proxy_exit_ip(tab: &std::sync::Arc<headless_chrome::Tab>) {
    debug!("Checking proxy exit IP...");
    let check_result = (|| -> Result<(), anyhow::Error> {
        let result = tab.evaluate(
            r#"
            (async function() {
                try {
                    const response = await fetch('https://api.ipify.org?format=json', {
                        method: 'GET',
                        headers: { 'Accept': 'application/json' }
                    });
                    const data = await response.json();
                    return JSON.stringify({
                        ip: data.ip,
                        success: true
                    });
                } catch (e) {
                    return JSON.stringify({
                        ip: 'unknown',
                        success: false,
                        error: e.toString()
                    });
                }
            })()
            "#,
            true, // await Promise
        )?;

        if let Some(value) = result.value {
            if let Some(json_str) = value.as_str() {
                debug!("Proxy exit IP info: {}", json_str);
            }
        }
        Ok(())
    })();

    if let Err(e) = check_result {
        warn!("Failed to check proxy IP: {}", e);
    }
}

/// RAII guard that automatically decrements active request counter on drop
struct ActiveRequestGuard {
    counter: Arc<AtomicUsize>,
}

impl ActiveRequestGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// RAII guard that automatically sets context is_busy flag
struct ContextBusyGuard {
    is_busy: Arc<std::sync::atomic::AtomicBool>,
}

impl ContextBusyGuard {
    fn new(is_busy: Arc<std::sync::atomic::AtomicBool>) -> Result<Self, ()> {
        // Try to set busy flag (compare-and-swap from false to true)
        match is_busy.compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        ) {
            Ok(_) => Ok(Self { is_busy }),
            Err(_) => Err(()), // Context is already busy
        }
    }
}

impl Drop for ContextBusyGuard {
    fn drop(&mut self) {
        self.is_busy
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

pub struct WorkerService {
    browser_pool: Arc<RwLock<BrowserPool>>,
    config: WorkerConfig,
    active_requests: Arc<AtomicUsize>,
    total_requests: Arc<AtomicUsize>,
    failed_requests: Arc<AtomicUsize>,
    metrics: Metrics,
    wait_strategy_registry: WaitStrategyRegistry,
    cancellation_token: tokio_cancellation_ext::CancellationToken,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerService {
    pub async fn new(
        config: WorkerConfig,
        metrics: Metrics,
        cancellation_token: tokio_cancellation_ext::CancellationToken,
    ) -> Result<Self> {
        info!(
            "Initializing WorkerService for scope: {}",
            config.scope.name
        );

        let browser_pool = BrowserPool::new(config.scope.clone()).await?;

        Ok(Self {
            browser_pool: Arc::new(RwLock::new(browser_pool)),
            config,
            active_requests: Arc::new(AtomicUsize::new(0)),
            total_requests: Arc::new(AtomicUsize::new(0)),
            failed_requests: Arc::new(AtomicUsize::new(0)),
            metrics,
            wait_strategy_registry: WaitStrategyRegistry::new(),
            cancellation_token,
            is_ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    /// Get access to the wait strategy registry for registering custom strategies
    pub fn wait_strategy_registry_mut(&mut self) -> &mut WaitStrategyRegistry {
        &mut self.wait_strategy_registry
    }

    pub fn active_requests(&self) -> Arc<AtomicUsize> {
        self.active_requests.clone()
    }

    pub fn is_ready_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.is_ready.clone()
    }

    /// Recreate the browser pool when the browser process has died.
    ///
    /// This can happen when:
    /// 1. `idle_browser_timeout` is reached (default: 1 hour) - headless_chrome kills the browser
    ///    process if it receives no CDP commands for this duration
    /// 2. Browser crashes or is killed externally
    ///
    /// When this happens, all existing tabs become unusable and `browser.new_tab()` will fail
    /// with "connection is closed" error. This method:
    /// - Creates a completely new BrowserPool with a fresh Chrome process
    /// - Replaces the old pool (old browser process is automatically cleaned up)
    /// - All existing contexts are lost (clients will need to get new session IDs)
    ///
    /// This is called automatically when we detect a dead browser during request processing.
    async fn recreate_browser_pool(&self) -> anyhow::Result<()> {
        warn!("Recreating browser pool due to dead browser process");

        let new_pool = BrowserPool::new(self.config.scope.clone()).await?;

        let mut pool_guard = self.browser_pool.write().await;
        *pool_guard = new_pool;

        info!("Browser pool successfully recreated");
        Ok(())
    }

    /// Check if an error indicates the browser process is dead.
    fn is_dead_browser_error(error_msg: &str) -> bool {
        error_msg.contains("connection is closed") || error_msg.contains("No such process")
    }

    /// Check if an error indicates only the tab's CDP session is dead (but browser is alive).
    /// This happens after hard timeout closes the tab - the CDP session is gone but the
    /// browser process and CDP context (with cookies/storage) are still valid.
    fn is_dead_tab_error(error_msg: &str) -> bool {
        error_msg.contains("No session with given id")
    }

    /// Acquire a context for a request, using the appropriate strategy based on config.
    ///
    /// In AlwaysNew mode: always creates a new context (never reuses).
    /// In Reusable/ReusablePreinit mode: reuses idle context or creates new one up to max.
    ///
    /// # Parameters
    /// * `pool` - The browser pool to acquire context from
    /// * `ray_id` - Request tracing ID for logging
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    async fn acquire_context(
        &self,
        pool: &BrowserPool,
        ray_id: &str,
        proxy_params: &ProxyParams,
    ) -> anyhow::Result<Option<Arc<BrowserContext>>> {
        match self.config.scope.session_mode {
            SessionMode::AlwaysNew => pool.create_always_new_context(ray_id, proxy_params).await,
            SessionMode::Reusable | SessionMode::ReusablePreinit => {
                pool.get_or_create_context(ray_id, proxy_params).await
            }
        }
    }

    /// Acquire a context with automatic browser pool recovery on dead browser.
    ///
    /// If context creation fails due to dead browser (connection closed), this method
    /// will recreate the browser pool and retry once.
    ///
    /// # Parameters
    /// * `start_time` - Request start time for execution_time_ms calculation
    /// * `ray_id` - Request tracing ID for logging
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    async fn acquire_context_with_recovery(
        &self,
        start_time: Instant,
        ray_id: &str,
        proxy_params: &ProxyParams,
    ) -> Result<Arc<BrowserContext>, Response<ScrapePageResponse>> {
        let mode_name = match self.config.scope.session_mode {
            SessionMode::AlwaysNew => "always_new",
            SessionMode::Reusable => "reusable",
            SessionMode::ReusablePreinit => "reusable_preinit",
        };

        debug!(ray_id = %ray_id, "Acquiring context ({} mode) with country_code={:?}", mode_name, proxy_params.country_code);

        let browser_pool_guard = self.browser_pool.read().await;

        match self
            .acquire_context(&browser_pool_guard, ray_id, proxy_params)
            .await
        {
            Ok(Some(ctx)) => Ok(ctx),
            Ok(None) => {
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                let error_message = if self.config.scope.session_mode == SessionMode::AlwaysNew {
                    format!(
                        "No available slots - max contexts limit ({}) reached",
                        self.config.scope.max_contexts
                    )
                } else {
                    format!(
                        "No available contexts - all {} contexts are busy",
                        self.config.scope.max_contexts
                    )
                };
                Err(Response::new(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message,
                    error_code: ErrorCode::ContextCreationFailed as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: String::new(),
                    ray_id: ray_id.to_string(),
                }))
            }
            Err(e) => {
                let error_msg = e.to_string();

                // Check if browser process is dead - attempt recovery
                if Self::is_dead_browser_error(&error_msg) {
                    warn!(
                        ray_id = %ray_id,
                        "Browser process appears dead during context creation ({} mode) - attempting recovery: {}",
                        mode_name, error_msg
                    );

                    drop(browser_pool_guard); // Release read lock before recreation

                    // Recreate browser pool
                    if let Err(recreate_err) = self.recreate_browser_pool().await {
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Err(Response::new(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!(
                                "Failed to recreate browser pool for scope '{}': {} (original error: {})",
                                self.config.scope.name, recreate_err, error_msg
                            ),
                            error_code: ErrorCode::BrowserError as i32,
                            response_headers: std::collections::HashMap::new(),
                            execution_time_ms,
                            context_id: String::new(),
                            ray_id: ray_id.to_string(),
                        }));
                    }

                    // Retry with new pool
                    let new_pool_guard = self.browser_pool.read().await;
                    match self
                        .acquire_context(&new_pool_guard, ray_id, proxy_params)
                        .await
                    {
                        Ok(Some(ctx)) => {
                            debug!(ray_id = %ray_id, "Successfully acquired context after browser pool recovery");
                            Ok(ctx)
                        }
                        Ok(None) => {
                            let execution_time_ms = start_time.elapsed().as_millis() as u64;
                            let error_message = if self.config.scope.session_mode
                                == SessionMode::AlwaysNew
                            {
                                format!(
                                    "No available slots after pool recreation - max contexts limit ({}) reached",
                                    self.config.scope.max_contexts
                                )
                            } else {
                                format!(
                                    "No available contexts after pool recreation - all {} contexts are busy",
                                    self.config.scope.max_contexts
                                )
                            };
                            Err(Response::new(ScrapePageResponse {
                                success: false,
                                status_code: 0,
                                content: String::new(),
                                error_message,
                                error_code: ErrorCode::ContextCreationFailed as i32,
                                response_headers: std::collections::HashMap::new(),
                                execution_time_ms,
                                context_id: String::new(),
                                ray_id: ray_id.to_string(),
                            }))
                        }
                        Err(retry_err) => {
                            let execution_time_ms = start_time.elapsed().as_millis() as u64;
                            Err(Response::new(ScrapePageResponse {
                                success: false,
                                status_code: 0,
                                content: String::new(),
                                error_message: format!(
                                    "Failed to create context after pool recreation: {}",
                                    retry_err
                                ),
                                error_code: ErrorCode::ContextCreationFailed as i32,
                                response_headers: std::collections::HashMap::new(),
                                execution_time_ms,
                                context_id: String::new(),
                                ray_id: ray_id.to_string(),
                            }))
                        }
                    }
                } else {
                    // Some other error - not a dead browser
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    Err(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!("Failed to create context: {}", e),
                        error_code: ErrorCode::ContextCreationFailed as i32,
                        response_headers: std::collections::HashMap::new(),
                        execution_time_ms,
                        context_id: String::new(),
                        ray_id: ray_id.to_string(),
                    }))
                }
            }
        }
    }

    /// Collect browser diagnostics (console logs, page info) with timeout protection.
    ///
    /// This method runs diagnostics in a separate blocking task with a timeout to prevent
    /// blocking the response. If diagnostics take too long, they are abandoned (but may
    /// continue running in the background).
    async fn collect_browser_diagnostics(
        tab: Arc<headless_chrome::Tab>,
        timeout: Duration,
        ray_id: &str,
    ) {
        let ray_id_owned = ray_id.to_string();

        let diagnostics_result = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                Self::run_diagnostics_sync(&tab, &ray_id_owned);
            }),
        )
        .await;

        match diagnostics_result {
            Ok(Ok(())) => {
                info!(ray_id = %ray_id, "Browser diagnostics completed successfully");
            }
            Ok(Err(e)) => {
                warn!(ray_id = %ray_id, "Browser diagnostics task failed: {}", e);
            }
            Err(_) => {
                warn!(ray_id = %ray_id, "Browser diagnostics timed out after {:?} - skipping to return response faster", timeout);
            }
        }
    }

    /// Synchronous diagnostics collection (runs in blocking task).
    fn run_diagnostics_sync(tab: &headless_chrome::Tab, ray_id: &str) {
        // Collect console logs
        if let Err(e) = Self::collect_console_logs_sync(tab) {
            warn!(ray_id = %ray_id, "Failed to collect console logs: {}", e);
        }

        // Collect page diagnostics
        if let Err(e) = Self::collect_page_diagnostics_sync(tab) {
            warn!(ray_id = %ray_id, "Failed to collect page diagnostics: {}", e);
        }
    }

    /// Collect console logs from the injected logger.
    fn collect_console_logs_sync(tab: &headless_chrome::Tab) -> Result<()> {
        let result = tab.evaluate(
            r#"JSON.stringify(window.__browserHiveConsoleLogger || { errors: [], warnings: [], logs: [] })"#,
            false,
        )?;

        if let Some(value) = result.value {
            if let Some(json_str) = value.as_str() {
                #[derive(serde::Deserialize)]
                struct ConsoleLog {
                    errors: Vec<String>,
                    warnings: Vec<String>,
                    logs: Vec<String>,
                }

                if let Ok(console_log) = serde_json::from_str::<ConsoleLog>(json_str) {
                    debug!(
                        "Console logs collected: {} errors, {} warnings, {} logs",
                        console_log.errors.len(),
                        console_log.warnings.len(),
                        console_log.logs.len()
                    );

                    if !console_log.errors.is_empty() {
                        warn!("Browser console errors: {:?}", console_log.errors);
                    }
                    if !console_log.warnings.is_empty() {
                        debug!("Browser console warnings: {:?}", console_log.warnings);
                    }
                    if !console_log.logs.is_empty() {
                        let preview: Vec<_> = console_log.logs.iter().take(10).collect();
                        debug!(
                            "Browser console.log messages (first 10 of {}): {:?}",
                            console_log.logs.len(),
                            preview
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Collect page diagnostics (document state, element counts, etc.).
    fn collect_page_diagnostics_sync(tab: &headless_chrome::Tab) -> Result<()> {
        let result = tab.evaluate(
            r#"JSON.stringify({
                documentReady: document.readyState,
                scriptsCount: document.getElementsByTagName('script').length,
                hasBody: !!document.body,
                bodyChildrenCount: document.body ? document.body.children.length : 0,
                htmlLength: document.documentElement.outerHTML.length,
                title: document.title || "(no title)",
                url: document.URL
            })"#,
            false,
        )?;

        if let Some(value) = result.value {
            if let Some(json_str) = value.as_str() {
                debug!("Page diagnostics: {}", json_str);
            }
        }
        Ok(())
    }

    async fn scrape_page_internal(
        &self,
        req: &ScrapePageRequest,
        context: Arc<BrowserContext>,
        ray_id: &str,
    ) -> Result<ScrapePageResponse, Status> {
        let start_time = Instant::now();

        // Set context as busy (RAII guard will clear on drop)
        let _busy_guard = match ContextBusyGuard::new(context.metadata.is_busy.clone()) {
            Ok(guard) => guard,
            Err(_) => {
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                let last_used = context.metadata.last_used_at.lock().await;
                let total_reqs = context.metadata.total_requests.load(Ordering::SeqCst);
                let cache_size = context.metadata.cache_size_mb.load(Ordering::SeqCst);
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!(
                        "Context {} is already busy (created: {:?}, last_used: {:?}, total_requests: {}, cache_size_mb: {}) (after {}ms)",
                        context.metadata.id, context.metadata.created_at, *last_used, total_reqs, cache_size, execution_time_ms
                    ),
                    error_code: ErrorCode::BrowserError as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }
        };

        // Extract domain for tracking
        // NOTE: Invalid URL is not a gRPC error - we return response with error code
        let domain = match utils::extract_domain(&req.url) {
            Ok(d) => d,
            Err(e) => {
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!("Invalid URL: {}", e),
                    error_code: ErrorCode::InvalidUrl as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }
        };

        // Track domain affinity
        context
            .metadata
            .primary_domains
            .write()
            .await
            .insert(domain.clone());

        // Update usage metrics
        *context.metadata.last_used_at.lock().await = Instant::now();
        context
            .metadata
            .total_requests
            .fetch_add(1, Ordering::SeqCst);

        // Get or create tab for this context.
        // Tab might be None if:
        // 1. Context was just recycled but Chrome WebSocket connection was dead
        // 2. Browser process was restarted or died due to idle_browser_timeout
        // In such cases, we lazily create a new tab on-demand.
        let mut browser_pool_guard = self.browser_pool.read().await;
        let mut browser = browser_pool_guard.get_browser();

        // Use context-specific proxy if available, otherwise use global proxy
        let proxy_config = match context.metadata.assigned_proxy_config.as_ref() {
            Some(config) => config.clone(),
            None => {
                // SECURITY: If provider supports per-context proxy, missing assigned_proxy_config is a critical error
                // This means the proxy pool failed to assign an IP, which would leak the worker's real IP
                if browser_pool_guard.supports_per_context_proxy() {
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!(
                            "SECURITY ERROR: Context {} has no assigned proxy, but provider '{}' requires per-context proxy assignment. \
                            This would expose the worker's real IP address. (after {}ms)",
                            context.metadata.id,
                            browser_pool_guard.get_proxy_provider_name(),
                            execution_time_ms
                        ),
                        error_code: ErrorCode::BrowserError as i32,
                        response_headers: std::collections::HashMap::new(),
                        execution_time_ms,
                        context_id: context.metadata.id.to_string(),
                        ray_id: ray_id.to_string(),
                    });
                }
                // For NoProxy and other non-pooled providers, use global config (safe for local dev)
                browser_pool_guard.get_proxy_config().clone()
            }
        };

        let mut tab_guard = context.tab.lock().await;

        if tab_guard.is_none() {
            debug!(
                ray_id = %ray_id,
                "Tab not found in context {} - creating new tab (likely after recycling)",
                context.metadata.id
            );

            // Try to create tab. If it fails, the browser process might be dead.
            match browser.new_tab() {
                Ok(new_tab) => {
                    *tab_guard = Some(new_tab);
                }
                Err(e) => {
                    let error_msg = e.to_string();

                    // Check if this is a "browser process dead" error
                    if Self::is_dead_browser_error(&error_msg) {
                        warn!(
                            ray_id = %ray_id,
                            "Browser process appears dead - attempting to recreate browser pool: {}",
                            error_msg
                        );

                        drop(browser_pool_guard); // Release read lock
                        drop(tab_guard); // Release tab lock

                        // Recreate browser pool
                        if let Err(e) = self.recreate_browser_pool().await {
                            let execution_time_ms = start_time.elapsed().as_millis() as u64;
                            return Ok(ScrapePageResponse {
                                success: false,
                                status_code: 0,
                                content: String::new(),
                                error_message: format!(
                                    "Failed to recreate browser pool for scope '{}': {} (after {}ms)",
                                    self.config.scope.name, e, execution_time_ms
                                ),
                                error_code: ErrorCode::BrowserError as i32,
                                response_headers: std::collections::HashMap::new(),
                                execution_time_ms,
                                context_id: context.metadata.id.to_string(),
                                ray_id: ray_id.to_string(),
                            });
                        }

                        // Re-acquire locks with new pool
                        browser_pool_guard = self.browser_pool.read().await;
                        browser = browser_pool_guard.get_browser();
                        tab_guard = context.tab.lock().await;

                        // Try again with new browser
                        match browser.new_tab() {
                            Ok(new_tab) => {
                                *tab_guard = Some(new_tab);
                                debug!(ray_id = %ray_id, "Successfully created tab after browser pool recreation");
                            }
                            Err(e) => {
                                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                return Ok(ScrapePageResponse {
                                    success: false,
                                    status_code: 0,
                                    content: String::new(),
                                    error_message: format!(
                                        "Failed to create tab after pool recreation for context {} (domain: {}): {} (after {}ms)",
                                        context.metadata.id, domain, e, execution_time_ms
                                    ),
                                    error_code: ErrorCode::BrowserError as i32,
                                    response_headers: std::collections::HashMap::new(),
                                    execution_time_ms,
                                    context_id: context.metadata.id.to_string(),
                                    ray_id: ray_id.to_string(),
                                });
                            }
                        }
                    } else {
                        // Some other error
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Ok(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!(
                                "Failed to create tab for context {} (domain: {}): {} (after {}ms)",
                                context.metadata.id, domain, error_msg, execution_time_ms
                            ),
                            error_code: ErrorCode::BrowserError as i32,
                            response_headers: std::collections::HashMap::new(),
                            execution_time_ms,
                            context_id: context.metadata.id.to_string(),
                            ray_id: ray_id.to_string(),
                        });
                    }
                }
            }
        }

        let mut tab = tab_guard.as_ref().unwrap().clone(); // Safe: we ensured tab exists above

        // Enable proxy authentication before first navigation on this tab
        // NOTE: This can fail if the tab's WebSocket connection has timed out (even though tab exists).
        // In such cases, we recreate the tab and try once more.
        if let Some((username, password)) = proxy_config.get_credentials() {
            // Try to enable Fetch domain to handle auth requests
            let enable_result = tab.enable_fetch(None, Some(true));

            if let Err(e) = enable_result {
                let error_msg = e.to_string();

                // Check if this is a "dead tab" error (tab's CDP session closed but browser alive).
                // This happens after hard timeout closes the tab to abort stuck CDP calls.
                // We can recover by creating a new tab in the same CDP context (preserving cookies/storage).
                if Self::is_dead_tab_error(&error_msg) {
                    warn!(
                        ray_id = %ray_id,
                        "Tab CDP session dead ('No session with given id') - recreating tab for context {} (cdp_context_id: {:?})",
                        context.metadata.id,
                        context.cdp_context_id
                    );

                    // Recreate tab in the appropriate context (isolated or shared)
                    let new_tab = if let Some(cdp_ctx_id) = &context.cdp_context_id {
                        // Isolated context - recreate tab in the same CDP BrowserContext
                        // This preserves cookies and storage from previous requests
                        match browser_pool_guard.create_tab_in_context(cdp_ctx_id, ray_id) {
                            Ok(t) => t,
                            Err(e) => {
                                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                return Ok(ScrapePageResponse {
                                    success: false,
                                    status_code: 0,
                                    content: String::new(),
                                    error_message: format!(
                                        "Failed to recreate tab in CDP context {} for context {}: {} (after {}ms)",
                                        cdp_ctx_id, context.metadata.id, e, execution_time_ms
                                    ),
                                    error_code: ErrorCode::BrowserError as i32,
                                    response_headers: std::collections::HashMap::new(),
                                    execution_time_ms,
                                    context_id: context.metadata.id.to_string(),
                                    ray_id: ray_id.to_string(),
                                });
                            }
                        }
                    } else {
                        // Shared context - recreate tab in default browser context
                        match browser_pool_guard.create_tab_shared(ray_id) {
                            Ok(t) => t,
                            Err(e) => {
                                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                return Ok(ScrapePageResponse {
                                    success: false,
                                    status_code: 0,
                                    content: String::new(),
                                    error_message: format!(
                                        "Failed to recreate tab in shared context for context {}: {} (after {}ms)",
                                        context.metadata.id, e, execution_time_ms
                                    ),
                                    error_code: ErrorCode::BrowserError as i32,
                                    response_headers: std::collections::HashMap::new(),
                                    execution_time_ms,
                                    context_id: context.metadata.id.to_string(),
                                    ray_id: ray_id.to_string(),
                                });
                            }
                        }
                    };

                    tab = new_tab.clone();
                    *tab_guard = Some(new_tab);

                    // Retry enable_fetch on the recreated tab
                    if let Err(e) = tab.enable_fetch(None, Some(true)) {
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Ok(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!(
                                "Failed to enable fetch after tab recreation for context {} (domain: {}): {} (after {}ms)",
                                context.metadata.id, domain, e, execution_time_ms
                            ),
                            error_code: ErrorCode::BrowserError as i32,
                            response_headers: std::collections::HashMap::new(),
                            execution_time_ms,
                            context_id: context.metadata.id.to_string(),
                            ray_id: ray_id.to_string(),
                        });
                    }

                    info!(
                        ray_id = %ray_id,
                        "Successfully recreated tab after dead session for context {} (cdp_context_id: {:?})",
                        context.metadata.id,
                        context.cdp_context_id
                    );
                }
                // Check if this is a "connection closed" error (WebSocket timeout or dead browser)
                else if error_msg.contains("connection is closed") {
                    warn!(
                        ray_id = %ray_id,
                        "Tab WebSocket connection closed - attempting tab recreation for context {}",
                        context.metadata.id
                    );

                    // Try to create new tab. If this also fails, browser process might be dead.
                    match browser.new_tab() {
                        Ok(new_tab) => {
                            tab = new_tab.clone();
                            *tab_guard = Some(new_tab);

                            // Retry enable_fetch on fresh tab
                            if let Err(e) = tab.enable_fetch(None, Some(true)) {
                                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                return Ok(ScrapePageResponse {
                                    success: false,
                                    status_code: 0,
                                    content: String::new(),
                                    error_message: format!(
                                        "Failed to enable fetch after tab recreation for context {} (domain: {}): {} (after {}ms)",
                                        context.metadata.id, domain, e, execution_time_ms
                                    ),
                                    error_code: ErrorCode::BrowserError as i32,
                                    response_headers: std::collections::HashMap::new(),
                                    execution_time_ms,
                                    context_id: context.metadata.id.to_string(),
                                    ray_id: ray_id.to_string(),
                                });
                            }
                        }
                        Err(e) => {
                            let tab_error = e.to_string();

                            // Browser process is dead - recreate entire pool
                            if Self::is_dead_browser_error(&tab_error) {
                                warn!(ray_id = %ray_id, "Browser process appears dead during tab recreation - recreating pool");

                                drop(browser_pool_guard);
                                drop(tab_guard);

                                if let Err(e) = self.recreate_browser_pool().await {
                                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                    return Ok(ScrapePageResponse {
                                        success: false,
                                        status_code: 0,
                                        content: String::new(),
                                        error_message: format!(
                                            "Failed to recreate browser pool for scope '{}' during tab recreation: {} (after {}ms)",
                                            self.config.scope.name, e, execution_time_ms
                                        ),
                                        error_code: ErrorCode::BrowserError as i32,
                                        response_headers: std::collections::HashMap::new(),
                                        execution_time_ms,
                                        context_id: context.metadata.id.to_string(),
                                        ray_id: ray_id.to_string(),
                                    });
                                }

                                // Re-acquire locks with new pool
                                browser_pool_guard = self.browser_pool.read().await;
                                browser = browser_pool_guard.get_browser();
                                tab_guard = context.tab.lock().await;

                                // Create new tab and enable fetch
                                let new_tab = match browser.new_tab() {
                                    Ok(t) => t,
                                    Err(e) => {
                                        let execution_time_ms =
                                            start_time.elapsed().as_millis() as u64;
                                        return Ok(ScrapePageResponse {
                                            success: false,
                                            status_code: 0,
                                            content: String::new(),
                                            error_message: format!(
                                                "Failed to create tab after pool recreation for context {} (domain: {}): {} (after {}ms)",
                                                context.metadata.id, domain, e, execution_time_ms
                                            ),
                                            error_code: ErrorCode::BrowserError as i32,
                                            response_headers: std::collections::HashMap::new(),
                                            execution_time_ms,
                                            context_id: context.metadata.id.to_string(),
                                            ray_id: ray_id.to_string(),
                                        });
                                    }
                                };
                                tab = new_tab.clone();
                                *tab_guard = Some(new_tab);

                                if let Err(e) = tab.enable_fetch(None, Some(true)) {
                                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                    return Ok(ScrapePageResponse {
                                        success: false,
                                        status_code: 0,
                                        content: String::new(),
                                        error_message: format!(
                                            "Failed to enable fetch after pool recreation for context {} (domain: {}): {} (after {}ms)",
                                            context.metadata.id, domain, e, execution_time_ms
                                        ),
                                        error_code: ErrorCode::BrowserError as i32,
                                        response_headers: std::collections::HashMap::new(),
                                        execution_time_ms,
                                        context_id: context.metadata.id.to_string(),
                                        ray_id: ray_id.to_string(),
                                    });
                                }

                                debug!(ray_id = %ray_id, "Successfully recreated pool and enabled fetch");
                            } else {
                                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                                return Ok(ScrapePageResponse {
                                    success: false,
                                    status_code: 0,
                                    content: String::new(),
                                    error_message: format!(
                                        "Failed to create tab for context {} (domain: {}): {} (after {}ms)",
                                        context.metadata.id, domain, tab_error, execution_time_ms
                                    ),
                                    error_code: ErrorCode::BrowserError as i32,
                                    response_headers: std::collections::HashMap::new(),
                                    execution_time_ms,
                                    context_id: context.metadata.id.to_string(),
                                    ray_id: ray_id.to_string(),
                                });
                            }
                        }
                    }
                } else {
                    // Some other error
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!(
                            "Failed to enable fetch for context {} (domain: {}): {} (after {}ms)",
                            context.metadata.id, domain, error_msg, execution_time_ms
                        ),
                        error_code: ErrorCode::BrowserError as i32,
                        response_headers: std::collections::HashMap::new(),
                        execution_time_ms,
                        context_id: context.metadata.id.to_string(),
                        ray_id: ray_id.to_string(),
                    });
                }
            }

            // Set proxy credentials
            if let Err(e) = tab.authenticate(Some(username.clone()), Some(password.clone())) {
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!(
                        "Failed to set auth for context {} (domain: {}): {} (after {}ms)",
                        context.metadata.id, domain, e, execution_time_ms
                    ),
                    error_code: ErrorCode::BrowserError as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }

            debug!(ray_id = %ray_id, "Enabled proxy authentication for tab before navigation");
        }
        drop(browser_pool_guard); // Release read lock

        // Navigate to URL (reusing existing tab!)
        // NOTE: Navigation errors are NOT critical - we still try to get content (chrome error page)
        // Wrap in spawn_blocking to avoid blocking tokio runtime + support cancellation + hard timeout
        let tab_clone = tab.clone();
        let tab_for_abort = tab.clone(); // Keep reference for forced close on timeout
        let url_clone = req.url.clone();
        let navigate_handle =
            tokio::task::spawn_blocking(move || tab_clone.navigate_to(&url_clone).map(|_| ()));

        let navigation_result = tokio::select! {
            _ = self.cancellation_token.cancelled() => {
                // Terminating - close tab and return immediately
                let _ = tab_for_abort.close(false);
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: "Worker is shutting down, please retry with another instance".to_string(),
                    error_code: ErrorCode::Terminating as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }
            _ = tokio::time::sleep(Duration::from_secs(NAVIGATION_TIMEOUT_SECS)) => {
                // Hard timeout on navigation - close tab to abort CDP call
                warn!(
                    ray_id = %ray_id,
                    "Navigation hard timeout after {}s - closing tab to abort",
                    NAVIGATION_TIMEOUT_SECS
                );
                let _ = tab_for_abort.close(false);
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!(
                        "Navigation stuck - hard timeout after {}s (tab closed to abort)",
                        NAVIGATION_TIMEOUT_SECS
                    ),
                    error_code: ErrorCode::TimeoutBrowser as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }
            result = navigate_handle => {
                match result {
                    Ok(nav_result) => nav_result,
                    Err(e) => {
                        // Join error - blocking task panicked
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Ok(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!("Navigation task failed: {}", e),
                            error_code: ErrorCode::BrowserError as i32,
                            response_headers: std::collections::HashMap::new(),
                            execution_time_ms,
                            context_id: context.metadata.id.to_string(),
                            ray_id: ray_id.to_string(),
                        });
                    }
                }
            }
        };

        if let Err(ref e) = navigation_result {
            error!(ray_id = %ray_id, "Navigation failed: {} - will still try to get content", e);
        }

        // Inject console logger and check headless markers (only if browser diagnostics enabled)
        // These are optional diagnostics that add overhead but help with debugging
        if self.config.scope.enable_browser_diagnostics && navigation_result.is_ok() {
            debug!(ray_id = %ray_id, "Injecting console logger to capture JavaScript errors...");
            let inject_console_logger = || -> Result<(), anyhow::Error> {
                tab.evaluate(
                    r#"
                    (function() {
                        if (window.__browserHiveConsoleLogger) return;
                        window.__browserHiveConsoleLogger = {
                            errors: [],
                            warnings: [],
                            logs: []
                        };

                        const originalError = console.error;
                        const originalWarn = console.warn;
                        const originalLog = console.log;

                        console.error = function(...args) {
                            window.__browserHiveConsoleLogger.errors.push(args.map(a => String(a)).join(' '));
                            originalError.apply(console, args);
                        };

                        console.warn = function(...args) {
                            window.__browserHiveConsoleLogger.warnings.push(args.map(a => String(a)).join(' '));
                            originalWarn.apply(console, args);
                        };

                        console.log = function(...args) {
                            window.__browserHiveConsoleLogger.logs.push(args.map(a => String(a)).join(' '));
                            originalLog.apply(console, args);
                        };
                    })()
                    "#,
                    false,
                )?;
                Ok(())
            };

            if let Err(e) = inject_console_logger() {
                warn!(ray_id = %ray_id, "Failed to inject console logger: {}", e);
            } else {
                debug!(ray_id = %ray_id, "Console logger injected successfully");
            }

            // Check for headless detection markers
            info!(ray_id = %ray_id, "Checking for headless detection markers...");
            let check_headless = || -> Result<(), anyhow::Error> {
                let result = tab.evaluate(
                    r#"
                    JSON.stringify({
                        webdriver: navigator.webdriver,
                        userAgent: navigator.userAgent,
                        languages: navigator.languages,
                        platform: navigator.platform,
                        hardwareConcurrency: navigator.hardwareConcurrency,
                        deviceMemory: navigator.deviceMemory,
                        hasChrome: typeof window.chrome !== 'undefined',
                        hasPermissions: typeof navigator.permissions !== 'undefined',
                        hasNotifications: 'Notification' in window,
                        hasServiceWorker: 'serviceWorker' in navigator,
                        documentHidden: document.hidden,
                        outerWidth: window.outerWidth,
                        outerHeight: window.outerHeight
                    })
                    "#,
                    false,
                )?;

                if let Some(value) = result.value {
                    if let Some(json_str) = value.as_str() {
                        info!(ray_id = %ray_id, "Headless detection markers: {}", json_str);
                    }
                }
                Ok(())
            };

            if let Err(e) = check_headless() {
                warn!(ray_id = %ray_id, "Failed to check headless markers: {}", e);
            }

            // Uncomment to check proxy exit IP (adds ~1sec overhead per request)
            // check_proxy_exit_ip(&tab);
        }

        // Validate and get effective timeout
        let wait_timeout = if req.wait_timeout_ms > 0 {
            validate_timeout(req.wait_timeout_ms).map_err(|e| {
                Status::invalid_argument(format!(
                    "Invalid wait_timeout_ms: {}. Maximum allowed: {} ms",
                    e, MAX_WAIT_TIMEOUT_MS
                ))
            })?
        } else {
            effective_timeout(req.wait_timeout_ms)
        };

        // Get wait strategy
        let strategy = if req.wait_strategy.is_empty() {
            self.wait_strategy_registry.default_strategy()
        } else {
            self.wait_strategy_registry
                .get(&req.wait_strategy)
                .ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "Unknown wait_strategy: '{}'. Available: {:?}",
                        req.wait_strategy,
                        self.wait_strategy_registry.list_strategies()
                    ))
                })?
        };

        // Prepare selectors for wait strategy (owned strings for spawn_blocking)
        let wait_selector_owned = if !req.wait_selector.is_empty() {
            Some(req.wait_selector.clone())
        } else {
            None
        };
        let skip_selector_owned = if !req.skip_selector.is_empty() {
            Some(req.skip_selector.clone())
        } else {
            None
        };

        // For logging - use as_str() views
        let wait_selector = wait_selector_owned.as_deref();
        let skip_selector = skip_selector_owned.as_deref();

        // Save strategy name before moving strategy into closure
        let strategy_name = strategy.name().to_string();

        // Log selector configuration
        info!(
            ray_id = %ray_id,
            "Wait strategy config: strategy={},  wait_timeout={}ms, wait_selector={:?}, skip_selector={:?}",
            strategy_name,
            wait_timeout,
            wait_selector,
            skip_selector
        );

        // Wait for page to load using selected strategy with selector checking
        // NOTE: Skip wait strategy if navigation failed (page didn't even start loading)
        // NOTE: Wait strategy errors are NOT critical - we still try to get content
        // Wrap in spawn_blocking + select for cancellation support + hard timeout
        let wait_result = if navigation_result.is_ok() {
            let tab_clone = tab.clone();
            let tab_for_abort = tab.clone(); // Keep reference for forced close on timeout
            let strategy_clone = strategy.clone();
            let cancellation_token = self.cancellation_token.clone();
            let wait_selector_for_closure = wait_selector_owned.clone();
            let skip_selector_for_closure = skip_selector_owned.clone();
            let ray_id_for_closure = ray_id.to_string();

            let wait_handle = tokio::task::spawn_blocking(move || {
                let wait_sel_ref = wait_selector_for_closure.as_deref();
                let skip_sel_ref = skip_selector_for_closure.as_deref();
                strategy_clone.wait(
                    &tab_clone,
                    wait_timeout,
                    wait_sel_ref,
                    skip_sel_ref,
                    &cancellation_token,
                    &ray_id_for_closure,
                )
            });

            // Hard timeout = wait_timeout + safety margin to catch stuck CDP calls
            let hard_timeout_ms = wait_timeout as u64 + (WAIT_STRATEGY_TIMEOUT_MARGIN_SECS * 1000);

            tokio::select! {
                _ = self.cancellation_token.cancelled() => {
                    // Terminating - close tab and return immediately
                    let _ = tab_for_abort.close(false);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: "Worker is shutting down, please retry with another instance".to_string(),
                        error_code: ErrorCode::Terminating as i32,
                        response_headers: std::collections::HashMap::new(),
                        execution_time_ms,
                        context_id: context.metadata.id.to_string(),
                        ray_id: ray_id.to_string(),
                    });
                }
                _ = tokio::time::sleep(Duration::from_millis(hard_timeout_ms)) => {
                    // Hard timeout - close tab to abort CDP call
                    warn!(
                        ray_id = %ray_id,
                        "Wait strategy hard timeout after {}ms (internal timeout was {}ms) - closing tab to abort",
                        hard_timeout_ms, wait_timeout
                    );
                    let _ = tab_for_abort.close(false);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!(
                            "Wait strategy stuck - hard timeout after {}ms (tab closed to abort)",
                            hard_timeout_ms
                        ),
                        error_code: ErrorCode::TimeoutBrowser as i32,
                        response_headers: std::collections::HashMap::new(),
                        execution_time_ms,
                        context_id: context.metadata.id.to_string(),
                        ray_id: ray_id.to_string(),
                    });
                }
                result = wait_handle => {
                    match result {
                        Ok(wait_result) => wait_result,
                        Err(e) => {
                            // Join error - blocking task panicked
                            let execution_time_ms = start_time.elapsed().as_millis() as u64;
                            return Ok(ScrapePageResponse {
                                success: false,
                                status_code: 0,
                                content: String::new(),
                                error_message: format!("Wait strategy task failed: {}", e),
                                error_code: ErrorCode::BrowserError as i32,
                                response_headers: std::collections::HashMap::new(),
                                execution_time_ms,
                                context_id: context.metadata.id.to_string(),
                                ray_id: ray_id.to_string(),
                            });
                        }
                    }
                }
            }
        } else {
            // Navigation failed - skip wait strategy, will return NetworkError
            Ok(WaitResult::Success) // Dummy value, will be overridden by navigation_result check
        };

        // Try to get content even if wait strategy failed
        // NOTE: If we can't get content, return response with empty content and BROWSER_ERROR
        // Wrap in spawn_blocking + select for cancellation support + hard timeout
        let tab_clone = tab.clone();
        let tab_for_abort = tab.clone(); // Keep reference for forced close on timeout
        let get_content_handle = tokio::task::spawn_blocking(move || tab_clone.get_content());

        let content = tokio::select! {
            _ = self.cancellation_token.cancelled() => {
                // Terminating - close tab and return immediately
                let _ = tab_for_abort.close(false);
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: "Worker is shutting down, please retry with another instance".to_string(),
                    error_code: ErrorCode::Terminating as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }
            _ = tokio::time::sleep(Duration::from_secs(GET_CONTENT_TIMEOUT_SECS)) => {
                // Hard timeout on get_content - close tab to abort CDP call
                warn!(
                    ray_id = %ray_id,
                    "get_content hard timeout after {}s - closing tab to abort",
                    GET_CONTENT_TIMEOUT_SECS
                );
                let _ = tab_for_abort.close(false);
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!(
                        "get_content stuck - hard timeout after {}s (tab closed to abort)",
                        GET_CONTENT_TIMEOUT_SECS
                    ),
                    error_code: ErrorCode::BrowserError as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }
            result = get_content_handle => {
                match result {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Ok(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!("Failed to get content: {}", e),
                            error_code: ErrorCode::BrowserError as i32,
                            response_headers: std::collections::HashMap::new(),
                            execution_time_ms,
                            context_id: context.metadata.id.to_string(),
                            ray_id: ray_id.to_string(),
                        });
                    }
                    Err(e) => {
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Ok(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!("Get content join error: {}", e),
                            error_code: ErrorCode::BrowserError as i32,
                            response_headers: std::collections::HashMap::new(),
                            execution_time_ms,
                            context_id: context.metadata.id.to_string(),
                            ray_id: ray_id.to_string(),
                        });
                    }
                }
            }
        };

        // EARLY DESTROY: In AlwaysNew mode, destroy context immediately after getting content
        // This frees the slot for the next request BEFORE we spend time on diagnostics/logging
        // Critical for high-throughput scenarios where max_contexts=1
        if self.config.scope.session_mode == SessionMode::AlwaysNew {
            let browser_pool = self.browser_pool.read().await;
            browser_pool
                .destroy_context(&context.metadata.id, ray_id)
                .await;
            info!(
                ray_id = %ray_id,
                "Early context destroy completed (AlwaysNew mode): {}",
                context.metadata.id
            );
        }

        // Collect browser diagnostics (optional, with timeout protection)
        if self.config.scope.enable_browser_diagnostics && navigation_result.is_ok() {
            let total_timeout = Duration::from_millis(DEFAULT_WAIT_TIMEOUT_MS as u64);
            let elapsed = start_time.elapsed();
            let remaining_time = total_timeout.saturating_sub(elapsed);

            if remaining_time > Duration::from_secs(MIN_TIME_FOR_DIAGNOSTICS_SECS) {
                info!(ray_id = %ray_id, "Collecting browser diagnostics (remaining time: {:?})...", remaining_time);
                // Use remaining_time minus 1 second as safety margin
                let diagnostics_timeout = remaining_time.saturating_sub(Duration::from_secs(1));
                Self::collect_browser_diagnostics(tab.clone(), diagnostics_timeout, ray_id).await;
            } else {
                info!(ray_id = %ray_id, "Skipping browser diagnostics - not enough time remaining ({:?})", remaining_time);
            }
        }

        // Get HTTP status code from the main document request
        // Returns 0 if status code cannot be determined
        let status_code = tab
            .evaluate(
                r#"
                (() => {
                    try {
                        // Performance API - most reliable for navigation
                        const nav = performance.getEntriesByType('navigation')[0];
                        if (nav && nav.responseStatus) {
                            return nav.responseStatus;
                        }

                        // Check for chrome error pages
                        const url = document.URL;
                        if (url.includes('chrome-error://')) {
                            return 0; // Connection error
                        }

                        // Unknown status
                        return 0;
                    } catch (e) {
                        return 0;
                    }
                })()
                "#,
                false,
            )
            .ok()
            .and_then(|result| result.value.and_then(|v| v.as_u64()))
            .unwrap_or(0) as u32;

        // DO NOT close tab - keep it for session reuse!

        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        // Build response based on navigation result and wait result
        // Priority: navigation error > wait result
        let (success, error_message, error_code) = if let Err(e) = navigation_result {
            // Navigation failed - this is a network/browser error
            (
                false,
                format!("Failed to navigate to URL: {}", e),
                ErrorCode::NetworkError,
            )
        } else {
            // Navigation succeeded - check wait result
            match wait_result {
                Ok(WaitResult::Success) => (true, String::new(), ErrorCode::None),
                Ok(WaitResult::SkipSelectorFound) => (
                    false,
                    format!("Skip selector '{}' was found", req.skip_selector),
                    ErrorCode::SkipSelectorFound,
                ),
                Ok(WaitResult::WaitSelectorNotFound) => (
                    false,
                    format!(
                        "Wait selector '{}' was not found within timeout",
                        req.wait_selector
                    ),
                    ErrorCode::SelectorNotFound,
                ),
                Err(e) => (
                    false,
                    format!("Wait strategy '{}' failed: {}", strategy_name, e),
                    ErrorCode::TimeoutBrowser,
                ),
            }
        };

        // For AlwaysNew mode, don't return context_id since it will be destroyed
        // and cannot be reused. This prevents coordinator from caching invalid session IDs.
        let context_id = if self.config.scope.session_mode == SessionMode::AlwaysNew {
            String::new()
        } else {
            context.metadata.id.to_string()
        };

        Ok(ScrapePageResponse {
            success,
            status_code,
            content,
            error_message,
            error_code: error_code as i32,
            response_headers: std::collections::HashMap::new(),
            execution_time_ms,
            context_id,
            ray_id: ray_id.to_string(),
        })
    }
}

#[tonic::async_trait]
impl WorkerServiceTrait for WorkerService {
    async fn scrape_page(
        &self,
        request: Request<ScrapePageRequest>,
    ) -> Result<Response<ScrapePageResponse>, Status> {
        let req = request.into_inner();

        // Get ray_id from request (coordinator generates it if not provided by client)
        let ray_id = req.ray_id.clone();

        // Track active request (automatically decrements on drop)
        let _active_guard = ActiveRequestGuard::new(self.active_requests.clone());

        let start_time = Instant::now();

        // Check if worker is terminating - return immediately
        if self.cancellation_token.is_cancelled() {
            let execution_time_ms = start_time.elapsed().as_millis() as u64;
            return Ok(Response::new(ScrapePageResponse {
                success: false,
                status_code: 0,
                content: String::new(),
                error_message: "Worker is shutting down, please retry with another instance"
                    .to_string(),
                error_code: ErrorCode::Terminating as i32,
                response_headers: std::collections::HashMap::new(),
                execution_time_ms,
                context_id: String::new(),
                ray_id: ray_id.clone(),
            }));
        }

        info!(ray_id = %ray_id, "Received scraping request for URL: {}", req.url);

        self.total_requests.fetch_add(1, Ordering::SeqCst);

        // Validate URL by extracting domain
        // NOTE: Invalid URL is not a gRPC error - we return response with error code
        let _domain = match utils::extract_domain(&req.url) {
            Ok(d) => d,
            Err(e) => {
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(Response::new(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!("Invalid URL: {}", e),
                    error_code: ErrorCode::InvalidUrl as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: String::new(), // No context created yet
                    ray_id: ray_id.clone(),
                }));
            }
        };

        // Build proxy params from request (country_code, etc.)
        let proxy_params = if req.country_code.is_empty() {
            ProxyParams::default()
        } else {
            ProxyParams::with_country(&req.country_code)
        };

        // Select context: either by session_id (Reusable modes only) or create a new one
        //
        // IMPORTANT: In AlwaysNew mode, we ALWAYS create a new context and ignore
        // any incoming context_id. This is because AlwaysNew contexts are destroyed after
        // each request, so they cannot be reused. The coordinator should not be caching
        // session IDs for AlwaysNew mode, but we ignore them here as a safety measure.
        let browser_pool_guard = self.browser_pool.read().await;
        let is_always_new = self.config.scope.session_mode == SessionMode::AlwaysNew;
        let should_use_existing_context = !req.context_id.is_empty() && !is_always_new;

        let context = if should_use_existing_context {
            // Session persistence (Reusable session mode only) - try to find existing context
            // Note: For existing sessions, we reuse the context's assigned proxy
            // (country_code in request is ignored for session continuation)
            info!(ray_id = %ray_id, "Looking for existing context: {}", req.context_id);
            match browser_pool_guard.find_context_by_id(&req.context_id).await {
                Some(ctx) => ctx,
                None => {
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!("Context not found or expired: {}", req.context_id),
                        error_code: ErrorCode::SessionNotFound as i32,
                        response_headers: std::collections::HashMap::new(),
                        execution_time_ms,
                        context_id: String::new(),
                        ray_id: ray_id.clone(),
                    }));
                }
            }
        } else {
            // No session or AlwaysNew mode - acquire context with automatic recovery
            drop(browser_pool_guard); // Release read lock before context acquisition

            match self
                .acquire_context_with_recovery(start_time, &ray_id, &proxy_params)
                .await
            {
                Ok(ctx) => ctx,
                Err(response) => return Ok(response),
            }
        };

        // Save context_id for fallback cleanup (AlwaysNew mode)
        let context_id = context.metadata.id;

        // Execute scraping
        // NOTE: In AlwaysNew mode, context is destroyed inside scrape_page_internal
        // immediately after getting content (before diagnostics) for faster slot release
        let result = self.scrape_page_internal(&req, context, &ray_id).await;

        // Fallback destroy: ensure context is destroyed even if scrape_page_internal returned early
        // This is idempotent - if context was already destroyed inside, this is a no-op
        if is_always_new {
            let browser_pool = self.browser_pool.read().await;
            browser_pool.destroy_context(&context_id, &ray_id).await;
        }

        // Update metrics
        match &result {
            Ok(response) => {
                // Success - metrics will be updated in get_stats
                let hash = content_hash(&response.content);

                info!(
                    ray_id = %ray_id,
                    "Request OK: ScrapePageResponse {{ success: {}, status_code: {}, content_sha256: {}, content_length: {}, error_message: {:?}, error_code: {}, execution_time_ms: {}, context_id: {} }}",
                    response.success,
                    response.status_code,
                    hash,
                    response.content.len(),
                    response.error_message,
                    response.error_code,
                    response.execution_time_ms,
                    response.context_id
                );
            }
            Err(status) => {
                self.failed_requests.fetch_add(1, Ordering::SeqCst);
                self.metrics
                    .requests_failed
                    .with_label_values(&[&self.config.scope.name])
                    .inc();
                error!(ray_id = %ray_id, "Request FAILED: {:?}", status);
            }
        }

        result.map(Response::new)
    }

    async fn health_check(
        &self,
        _request: Request<()>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let is_ready = self.is_ready.load(std::sync::atomic::Ordering::SeqCst);
        let is_terminating = self.cancellation_token.is_cancelled();

        Ok(Response::new(HealthCheckResponse {
            healthy: is_ready && !is_terminating,
            message: if is_terminating {
                "Worker is terminating".to_string()
            } else if !is_ready {
                "Worker is not ready".to_string()
            } else {
                format!("Worker healthy for scope: {}", self.config.scope.name)
            },
        }))
    }

    async fn get_stats(
        &self,
        _request: Request<()>,
    ) -> Result<Response<WorkerStatsResponse>, Status> {
        let browser_pool = self.browser_pool.read().await;
        let stats = browser_pool.get_stats().await;

        let success_count = self.total_requests.load(Ordering::SeqCst)
            - self.failed_requests.load(Ordering::SeqCst);
        let total_count = self.total_requests.load(Ordering::SeqCst);

        let success_rate = if total_count > 0 {
            success_count as f64 / total_count as f64
        } else {
            1.0
        };

        Ok(Response::new(WorkerStatsResponse {
            scope_name: self.config.scope.name.clone(),
            pod_name: self.config.pod_name.clone(),
            pod_ip: self.config.pod_ip.clone(),
            total_contexts: stats.total_contexts as u32,
            available_slots: stats.available_slots as u32,
            active_requests: stats.active_requests as u32,
            total_requests: stats.total_requests,
            total_contexts_created: stats.total_contexts_created,
            total_contexts_recycled: stats.total_contexts_recycled,
            success_rate,
        }))
    }
}
