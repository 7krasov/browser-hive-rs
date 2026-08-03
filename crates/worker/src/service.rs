use crate::browser_pool::{BrowserContext, BrowserPool};
use crate::diagnostics::DiagnosticsLimiter;
use crate::metrics::Metrics;
use anyhow::Result;
use browser_hive_common::{
    effective_timeout, utils, validate_timeout, ProxyParams, SessionMode, WaitResult,
    WaitStrategyRegistry, WorkerConfig, MAX_WAIT_TIMEOUT_MS,
};
use std::time::Duration;

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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn, Instrument};

/// RAII guard that removes a CDP event listener from a tab on drop.
///
/// Tabs are reused across requests in reusable session modes, so a listener
/// registered per request must be removed afterwards to avoid unbounded
/// accumulation on the shared tab. Removal only touches the tab's in-memory
/// listener vector (no CDP call), so it is safe even if the tab's CDP session
/// is already gone (e.g. a timeout/cancel closed it).
pub(crate) struct EventListenerGuard {
    remove: Option<Box<dyn FnOnce() + Send>>,
}

impl EventListenerGuard {
    pub(crate) fn new(remove: impl FnOnce() + Send + 'static) -> Self {
        Self {
            remove: Some(Box::new(remove)),
        }
    }
}

impl Drop for EventListenerGuard {
    fn drop(&mut self) {
        if let Some(remove) = self.remove.take() {
            remove();
        }
    }
}

/// Wire-level facts of the main navigation captured by the response observer from the
/// top-level document `Network.responseReceived` event. See RESPONSE_OBSERVERS.md.
///
/// This is a small struct rather than a `ResponseObserver` trait on purpose: two fixed
/// fields (status + headers) do not justify an abstraction. Extract a trait only when
/// signals become pluggable/per-scope or numerous (exit-IP, redirect chain, protocol…).
#[derive(Default)]
struct MainDocumentResponse {
    /// HTTP status of the final main-document response (0 if unknown/uncaptured).
    status: u32,
    /// Response headers of the final main-document response.
    headers: std::collections::HashMap<String, String>,
    /// URL of the final main-document response (empty if uncaptured). Used to detect
    /// off-domain redirects — this is the authoritative landing URL from the network layer.
    url: String,
}

/// How many distinct kinds of proxy failure are kept per request before only the count grows.
const MAX_PROXY_FAILURE_KINDS: usize = 8;

/// Chromium error texts that mean the **proxy path** failed rather than the origin.
///
/// This is deliberately keyed on Chromium's error taxonomy and not on any provider's status
/// code. For HTTPS the browser reaches the origin through a `CONNECT` tunnel, and the proxy's
/// reply to `CONNECT` is consumed by the network stack — an unavailable peer is typically
/// answered with HTTP 502, but that 502 never surfaces as a status code, only as
/// `ERR_TUNNEL_CONNECTION_FAILED`. Detection therefore works the same for every provider.
///
/// Origin-side failures (`ERR_CONNECTION_REFUSED`, `ERR_NAME_NOT_RESOLVED`, plain `ERR_FAILED`)
/// are deliberately absent: they say nothing about the proxy.
fn is_proxy_error(error_text: &str) -> bool {
    const PROXY_ERRORS: [&str; 8] = [
        "ERR_TUNNEL_CONNECTION_FAILED",
        "ERR_PROXY_CONNECTION_FAILED",
        "ERR_PROXY_AUTH_UNSUPPORTED",
        "ERR_PROXY_CERTIFICATE_INVALID",
        "ERR_UNEXPECTED_PROXY_AUTH",
        "ERR_MANDATORY_PROXY_CONFIGURATION_FAILED",
        "ERR_SOCKS_CONNECTION_FAILED",
        "ERR_SOCKS_CONNECTION_HOST_UNREACHABLE",
    ];
    PROXY_ERRORS.iter().any(|e| error_text.contains(e))
}

/// Resource loads that failed with a proxy/tunnel error during one request.
///
/// Collected by the same CDP `Network` listener that captures the main-document response, so it
/// costs no extra domain and works whether or not browser diagnostics are enabled — these
/// failures are rare, always actionable, and otherwise invisible: when they hit sub-resources
/// the document still returns 200 and only the content is wrong.
///
/// URLs are not tracked here on purpose. `Network.loadingFailed` does not carry one, so naming
/// the resource would require keeping a request-id → URL map for every request; browser
/// diagnostics already does that when it is switched on.
#[derive(Default)]
struct ProxyFailures {
    /// "<resource type> <error text>" → how many loads failed that way.
    by_kind: std::collections::BTreeMap<String, usize>,
    /// Total failures, including those past `MAX_PROXY_FAILURE_KINDS`.
    total: usize,
}

impl ProxyFailures {
    fn record(&mut self, kind: String) {
        self.total += 1;
        if self.by_kind.len() < MAX_PROXY_FAILURE_KINDS || self.by_kind.contains_key(&kind) {
            *self.by_kind.entry(kind).or_insert(0) += 1;
        }
    }

    /// One-line summary, e.g. `Script net::ERR_TUNNEL_CONNECTION_FAILED (x4)`.
    fn summary(&self) -> String {
        self.by_kind
            .iter()
            .map(|(kind, count)| {
                if *count > 1 {
                    format!("{} (x{})", kind, count)
                } else {
                    kind.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Detect a redirect that landed on a **different registrable domain** (eTLD+1) than the one
/// requested. Returns `Some(final_registrable_domain)` for an off-domain redirect, or `None`
/// when it is the same site (e.g. `www.example.com` → `shop.example.com`) or when either
/// URL/host/eTLD+1 cannot be determined. It errs toward `None`, so an odd-but-valid
/// navigation is never mislabelled as an off-domain redirect.
fn cross_site_redirect_target(requested_url: &str, final_url: &str) -> Option<String> {
    let host_of = |u: &str| -> Option<String> {
        url::Url::parse(u)
            .ok()?
            .host_str()
            .map(str::to_ascii_lowercase)
    };
    let requested_host = host_of(requested_url)?;
    let final_host = host_of(final_url)?;
    let requested_domain = psl::domain_str(&requested_host)?;
    let final_domain = psl::domain_str(&final_host)?;
    (requested_domain != final_domain).then(|| final_domain.to_string())
}

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

/// RAII guard that observes the end-to-end request duration into a histogram on
/// drop, so every return path of `scrape_page` (including early returns) is
/// covered uniformly.
struct RequestTimer {
    start: Instant,
    histogram: prometheus::Histogram,
}

impl RequestTimer {
    fn new(histogram: prometheus::Histogram) -> Self {
        Self {
            start: Instant::now(),
            histogram,
        }
    }
}

impl Drop for RequestTimer {
    fn drop(&mut self) {
        self.histogram.observe(self.start.elapsed().as_secs_f64());
    }
}

/// RAII guard that automatically sets context is_busy flag
struct ContextBusyGuard {
    is_busy: Arc<std::sync::atomic::AtomicBool>,
}

impl ContextBusyGuard {
    /// Adopt a context that was already marked busy at creation time (AlwaysNew mode).
    ///
    /// AlwaysNew contexts are pre-marked busy inside `create_always_new_context` while the
    /// pool write lock is held, so that leak reclamation can never collect a context that
    /// was handed out but has not started processing yet. The flag is already set here —
    /// this guard only takes over clearing it on drop.
    fn adopt(is_busy: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { is_busy }
    }

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

/// RAII guard that removes an AlwaysNew context from the pool when the request scope ends.
///
/// Destruction must not depend on control flow reaching a cleanup statement. If the gRPC
/// handler future is dropped mid-flight (client disconnect, coordinator deadline, connection
/// reset) or panics, any explicit cleanup at the end of the handler never runs. In AlwaysNew
/// mode that leaks the context into the pool permanently — it is never reused and never
/// reclaimed by the request path, so the slot is lost until the worker restarts.
///
/// Destruction is idempotent, so this composes with the early destroy that releases the slot
/// as soon as content is retrieved.
struct AlwaysNewContextGuard {
    browser_pool: Arc<RwLock<BrowserPool>>,
    context_id: uuid::Uuid,
    ray_id: String,
    scope: String,
}

impl Drop for AlwaysNewContextGuard {
    fn drop(&mut self) {
        // Drop may run in a non-async context, so the removal is spawned. If no runtime is
        // available (worker shutting down) the whole pool is going away anyway.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let browser_pool = self.browser_pool.clone();
        let context_id = self.context_id;
        let ray_id = std::mem::take(&mut self.ray_id);
        let scope = std::mem::take(&mut self.scope);

        // Drop runs in a freshly spawned task, so the request span does not propagate here.
        // Open a span carrying ray_id/scope so destroy_context's logs stay correlated
        // (span_ray_id, span_scope).
        let span = tracing::info_span!("always_new_context_drop", scope = %scope, ray_id = %ray_id);
        handle.spawn(
            async move {
                browser_pool.read().await.destroy_context(&context_id).await;
            }
            .instrument(span),
        );
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
    /// Worker-wide cap on how often diagnostics may be logged (see `diagnostics` module).
    diagnostics_limiter: Arc<DiagnosticsLimiter>,
    /// Bumped every time the browser process dies and the pool is replaced.
    ///
    /// A request holds an `Arc<BrowserContext>` taken from the pool it started with. When the
    /// browser dies mid-request that context belongs to a dead process — its CDP context is gone
    /// and cannot be recreated — so the request must start over against the new pool rather than
    /// carry on with a stale handle. Comparing this counter across the attempt is what detects
    /// that, and it is precise rather than heuristic: whoever replaced the pool, a pool replaced
    /// during the request means *this* request's context is stale too.
    pool_generation: Arc<AtomicU64>,
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

        if config.scope.diagnostics.enabled {
            info!("Browser diagnostics: {:?}", config.scope.diagnostics);
        }
        let diagnostics_limiter = Arc::new(DiagnosticsLimiter::new(
            config.scope.diagnostics.max_per_minute,
        ));

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
            diagnostics_limiter,
            pool_generation: Arc::new(AtomicU64::new(0)),
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

    /// Shared handle to the browser pool (used by the metrics server to
    /// refresh pool gauges on each Prometheus scrape)
    pub fn browser_pool_handle(&self) -> Arc<RwLock<BrowserPool>> {
        self.browser_pool.clone()
    }

    /// Count a response with a 5xxx operational error code as a failed request.
    /// 4xxx codes are client-side conditions (invalid URL, selector not found)
    /// and are not counted. Infrastructure gRPC errors are counted separately.
    fn record_failed_if_5xxx(&self, response: &ScrapePageResponse) {
        if (5000..6000).contains(&response.error_code) {
            self.failed_requests.fetch_add(1, Ordering::SeqCst);
            self.metrics
                .requests_failed
                .with_label_values(&[&self.config.scope.name])
                .inc();
        }
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

        // Bumped under the write lock, so no request can observe the new pool with the old
        // generation and conclude its context is still valid.
        let generation = self.pool_generation.fetch_add(1, Ordering::SeqCst) + 1;
        drop(pool_guard);

        info!("Browser pool successfully recreated (generation {generation})");
        Ok(())
    }

    /// Arm the AlwaysNew cleanup guard for a context, or nothing outside AlwaysNew mode.
    ///
    /// A helper because the retry path has to arm a second one for the replacement context, and
    /// the two must stay identical.
    fn always_new_guard_for(
        &self,
        is_always_new: bool,
        context: &Arc<BrowserContext>,
        ray_id: &str,
    ) -> Option<AlwaysNewContextGuard> {
        is_always_new.then(|| AlwaysNewContextGuard {
            browser_pool: self.browser_pool.clone(),
            context_id: context.metadata.id,
            ray_id: ray_id.to_string(),
            scope: self.config.scope.name.clone(),
        })
    }

    /// End the attempt after the browser process died and the pool was replaced under it.
    ///
    /// The context this request holds came from the old pool, and its CDP BrowserContext died
    /// with the process — it cannot be recreated, only replaced. Creating the tab in the *new*
    /// browser's default context would let the request finish, which is what this code used to
    /// do, at the cost of quietly dropping the context's isolation and, for providers that route
    /// per context, its proxy: the request would leave through the launch proxy while its span
    /// still named the assigned host, and concurrent requests recovering from the same death
    /// would share that one default context with each other.
    ///
    /// So the attempt ends instead. The handler sees the pool generation moved and retries the
    /// whole request against a context from the new pool, which is what
    /// `acquire_context_with_recovery` already does when the browser dies *before* a context is
    /// bound. A client normally never sees this response — it surfaces only if the retry itself
    /// cannot get a context.
    fn browser_restarted_response(
        &self,
        context: &Arc<BrowserContext>,
        ray_id: &str,
        start_time: Instant,
    ) -> ScrapePageResponse {
        warn!(
            "Browser pool was replaced while this request was running - context {} belonged to \
             the dead process, retrying the request with a fresh context",
            context.metadata.id
        );

        let execution_time_ms = start_time.elapsed().as_millis() as u64;
        ScrapePageResponse {
            success: false,
            status_code: 0,
            content: String::new(),
            error_message: format!(
                "Browser process died and the pool was recreated; context {} is stale (after {}ms)",
                context.metadata.id, execution_time_ms
            ),
            error_code: ErrorCode::BrowserError as i32,
            response_headers: std::collections::HashMap::new(),
            execution_time_ms,
            context_id: context.metadata.id.to_string(),
            ray_id: ray_id.to_string(),
        }
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
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    async fn acquire_context(
        &self,
        pool: &BrowserPool,
        proxy_params: &ProxyParams,
    ) -> anyhow::Result<Option<Arc<BrowserContext>>> {
        match self.config.scope.session_mode {
            SessionMode::AlwaysNew => pool.create_always_new_context(proxy_params).await,
            SessionMode::Reusable | SessionMode::ReusablePreinit => {
                pool.get_or_create_context(proxy_params).await
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

        debug!("Acquiring context ({} mode) with country_code={:?}", mode_name, proxy_params.country_code);

        let browser_pool_guard = self.browser_pool.read().await;

        match self
            .acquire_context(&browser_pool_guard, proxy_params)
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
                        .acquire_context(&new_pool_guard, proxy_params)
                        .await
                    {
                        Ok(Some(ctx)) => {
                            debug!("Successfully acquired context after browser pool recovery");
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

    async fn scrape_page_internal(
        &self,
        req: &ScrapePageRequest,
        context: Arc<BrowserContext>,
        ray_id: &str,
    ) -> Result<ScrapePageResponse, Status> {
        let start_time = Instant::now();

        // Set context as busy (RAII guard will clear on drop).
        // In AlwaysNew mode the context was already marked busy at creation (see
        // BrowserPool::create_always_new_context), so the guard adopts the flag instead of
        // setting it — otherwise the compare-and-swap would fail on a perfectly valid context.
        let is_always_new = self.config.scope.session_mode == SessionMode::AlwaysNew;
        let busy_guard_result = if is_always_new {
            Ok(ContextBusyGuard::adopt(context.metadata.is_busy.clone()))
        } else {
            ContextBusyGuard::new(context.metadata.is_busy.clone())
        };

        let _busy_guard = match busy_guard_result {
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
        // Neither is reassigned any more: a request never continues against a pool that was
        // replaced under it, so there is no "re-acquire with the new pool" step.
        let browser_pool_guard = self.browser_pool.read().await;
        let browser = browser_pool_guard.get_browser();

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
                "Tab not found in context {} - creating new tab (likely after recycling)",
                context.metadata.id
            );

            // An isolated slot whose CDP BrowserContext is gone has nowhere correct to put a tab:
            // cookies, storage and - for providers that route per context - the proxy all live on
            // that context, so a tab in the browser's default context would quietly have none of
            // them and the request would leave through the launch proxy while its span named the
            // assigned host. Drop the slot instead; the next request builds a correct one, whereas
            // keeping it would hand the same broken slot out on every request.
            if context.cdp_context_id.is_none() && browser_pool_guard.uses_isolated_contexts() {
                warn!(
                    "Context {} has no CDP context to create a tab in (its recycling failed) - \
                     removing it from the pool instead of serving the request from the browser's \
                     default context",
                    context.metadata.id
                );
                drop(tab_guard); // destroy_context closes the tab and needs this lock
                browser_pool_guard
                    .destroy_context(&context.metadata.id)
                    .await;

                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!(
                        "Context {} lost its isolated CDP context and was removed from the pool; \
                         retry to get a fresh one (after {}ms)",
                        context.metadata.id, execution_time_ms
                    ),
                    error_code: ErrorCode::BrowserError as i32,
                    response_headers: std::collections::HashMap::new(),
                    execution_time_ms,
                    context_id: context.metadata.id.to_string(),
                    ray_id: ray_id.to_string(),
                });
            }

            // Try to create tab. If it fails, the browser process might be dead.
            //
            // Isolated slots keep their CDP context across a failed recycling, so the tab goes
            // back inside it - same reasoning as the dead-tab recovery further down.
            let created = match &context.cdp_context_id {
                Some(cdp_ctx_id) => browser_pool_guard.create_tab_in_context(cdp_ctx_id),
                None => browser.new_tab(),
            };
            match created {
                Ok(new_tab) => {
                    *tab_guard = Some(new_tab);
                }
                Err(e) => {
                    let error_msg = e.to_string();

                    // Check if this is a "browser process dead" error
                    if Self::is_dead_browser_error(&error_msg) {
                        warn!(
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

                        // The browser we just replaced took this request's context with it, so
                        // this attempt is over - see `browser_restarted_response`. The handler
                        // retries against the new pool.
                        return Ok(self.browser_restarted_response(&context, ray_id, start_time));
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
                        "Tab CDP session dead ('No session with given id') - recreating tab for context {} (cdp_context_id: {:?})",
                        context.metadata.id,
                        context.cdp_context_id
                    );

                    // Recreate tab in the appropriate context (isolated or shared)
                    let new_tab = if let Some(cdp_ctx_id) = &context.cdp_context_id {
                        // Isolated context - recreate tab in the same CDP BrowserContext
                        // This preserves cookies and storage from previous requests
                        match browser_pool_guard.create_tab_in_context(cdp_ctx_id) {
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
                        match browser_pool_guard.create_tab_shared() {
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
                        "Successfully recreated tab after dead session for context {} (cdp_context_id: {:?})",
                        context.metadata.id,
                        context.cdp_context_id
                    );
                }
                // Check if this is a "connection closed" error (WebSocket timeout or dead browser)
                else if error_msg.contains("connection is closed") {
                    warn!(
                        "Tab WebSocket connection closed - attempting tab recreation for context {}",
                        context.metadata.id
                    );

                    // Try to create new tab. If this also fails, browser process might be dead.
                    //
                    // Only the tab's socket is suspect here, so an isolated slot's CDP context is
                    // still alive and the tab goes back inside it - same rule as the branch above
                    // and as the lazy creation earlier: a request is never served from the
                    // browser's default context, which would silently cost it its isolation and,
                    // for providers that route per context, its proxy.
                    let recreated = match &context.cdp_context_id {
                        Some(cdp_ctx_id) => browser_pool_guard.create_tab_in_context(cdp_ctx_id),
                        None => browser.new_tab(),
                    };
                    match recreated {
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
                                warn!("Browser process appears dead during tab recreation - recreating pool");

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

                                // Same as the lazy-creation path above: the replaced browser took
                                // this request's context with it, so the attempt ends here and
                                // the handler retries against the new pool.
                                return Ok(
                                    self.browser_restarted_response(&context, ray_id, start_time)
                                );
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

            debug!("Enabled proxy authentication for tab before navigation");
        }
        drop(browser_pool_guard); // Release read lock

        // Response Observer: main-document HTTP response headers.
        //
        // This is the first (and currently only) "response observer" — a per-request hook
        // that extracts wire-level facts of the top-level navigation that the DOM/JS cannot
        // see (see RESPONSE_OBSERVERS.md for the concept and the planned trait-based
        // generalization to status/exit-IP/redirects/protocol/etc.).
        //
        // Mechanism: enable the CDP Network domain and record the response for the top-level
        // frame (main frame id == target id). The Performance API cannot expose headers, and
        // CDP also carries the authoritative HTTP status, so both are read from this single
        // event. The listener is removed on drop so it does not accumulate on reused tabs.
        // Failure to enable is non-fatal: the request falls back to the Performance-API
        // status and empty headers.
        //
        // The same listener also records proxy/tunnel failures (see `ProxyFailures`): they
        // arrive on the already-enabled domain, and a sub-resource lost to a dead tunnel is
        // otherwise invisible — the document still returns 200.
        let main_response_holder: Arc<std::sync::Mutex<Option<MainDocumentResponse>>> =
            Arc::new(std::sync::Mutex::new(None));
        let proxy_failure_holder: Arc<std::sync::Mutex<ProxyFailures>> =
            Arc::new(std::sync::Mutex::new(ProxyFailures::default()));
        let _header_capture_guard: Option<EventListenerGuard> = {
            use headless_chrome::protocol::cdp::types::Event;
            use headless_chrome::protocol::cdp::Network;

            match tab.call_method(Network::Enable {
                max_total_buffer_size: None,
                max_resource_buffer_size: None,
                max_post_data_size: None,
            }) {
                Err(e) => {
                    debug!("Failed to enable Network domain for response capture: {}", e);
                    None
                }
                Ok(_) => {
                    let main_frame_id = tab.get_target_id().clone();
                    let holder = main_response_holder.clone();
                    let proxy_holder = proxy_failure_holder.clone();
                    let listener: Arc<
                        dyn headless_chrome::browser::tab::EventListener<Event> + Send + Sync,
                    > = Arc::new(move |event: &Event| {
                        if let Event::NetworkLoadingFailed(ev) = event {
                            if is_proxy_error(&ev.params.error_text) {
                                proxy_holder.lock().unwrap().record(format!(
                                    "{:?} {}",
                                    ev.params.Type, ev.params.error_text
                                ));
                            }
                        }
                        if let Event::NetworkResponseReceived(ev) = event {
                            // Only the top-level document response (main frame == target id).
                            if matches!(ev.params.Type, Network::ResourceType::Document)
                                && ev.params.frame_id.as_deref() == Some(main_frame_id.as_str())
                            {
                                let headers = match &ev.params.response.headers.0 {
                                    Some(serde_json::Value::Object(map)) => map
                                        .iter()
                                        .map(|(k, v)| {
                                            let val = match v {
                                                serde_json::Value::String(s) => s.clone(),
                                                other => other.to_string(),
                                            };
                                            (k.clone(), val)
                                        })
                                        .collect(),
                                    _ => std::collections::HashMap::new(),
                                };
                                // Last matching response wins (final URL after redirects).
                                *holder.lock().unwrap() = Some(MainDocumentResponse {
                                    status: ev.params.response.status,
                                    headers,
                                    url: ev.params.response.url.clone(),
                                });
                            }
                        }
                    });
                    match tab.add_event_listener(listener) {
                        Ok(weak) => {
                            let tab_for_remove = tab.clone();
                            Some(EventListenerGuard::new(move || {
                                let _ = tab_for_remove.remove_event_listener(&weak);
                            }))
                        }
                        Err(e) => {
                            debug!("Failed to add response header listener: {}", e);
                            None
                        }
                    }
                }
            }
        };

        // Start diagnostics capture BEFORE navigating: the failures worth explaining (blocked
        // bundles, JS that throws during load) all happen while the page loads, so a listener
        // registered afterwards sees nothing. Returns None when diagnostics are inactive for
        // this request, in which case no CDP domain is touched.
        //
        // The session emits on drop, which is what covers the early returns below (hard
        // timeouts, cancellation) without a call at every `return`. Its default outcome is
        // "failed"; the success path calls mark_success() further down.
        let mut diagnostics = crate::diagnostics::start_capture(
            &tab,
            &self.config.scope.diagnostics,
            &self.diagnostics_limiter,
            &req.url,
        );

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
            error!("Navigation failed: {} - will still try to get content", e);
        }

        // Headless-detection markers: a one-off snapshot of the stealth-relevant navigator
        // surface. Gated on the diagnostics session so it inherits the same domain filter and
        // never costs anything on a normal request.
        if diagnostics.is_some() && navigation_result.is_ok() {
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
                        info!("Diagnostics/headless markers: {}", json_str);
                    }
                }
                Ok(())
            };

            if let Err(e) = check_headless() {
                warn!("Failed to check headless markers: {}", e);
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

        // Enrich the request span now that the wait configuration is resolved.
        let span = tracing::Span::current();
        span.record("wait_strategy", strategy_name.as_str());
        span.record("wait_timeout_ms", wait_timeout as u64);

        // Log selector configuration
        info!(
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
            // spawn_blocking runs on a separate thread, so the current tracing span
            // (opened in `scrape_page` and carried via `.instrument(span)`) is NOT active
            // there. Capture it and re-enter inside the closure so all wait-strategy logs
            // (target = browser_hive_common::wait_strategy) inherit the request context
            // (ray_id, url, context_id, wait_strategy, wait_timeout_ms, …) from the span.
            let wait_span = tracing::Span::current();

            let wait_handle = tokio::task::spawn_blocking(move || {
                let _span_guard = wait_span.enter();
                let wait_sel_ref = wait_selector_for_closure.as_deref();
                let skip_sel_ref = skip_selector_for_closure.as_deref();
                strategy_clone.wait(
                    &tab_clone,
                    wait_timeout,
                    wait_sel_ref,
                    skip_sel_ref,
                    &cancellation_token,
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

        // Page-state snapshot, while the tab is still alive: in AlwaysNew mode the early destroy
        // below tears down the CDP context, after which evaluating anything on the tab fails.
        // Bounded by PAGE_STATE_BUDGET, independent of how much of the wait timeout was spent —
        // a leftover-based budget would be zero exactly on the timeouts worth diagnosing.
        if diagnostics.is_some() && navigation_result.is_ok() {
            if let Some(state) = crate::diagnostics::capture_page_state(tab.clone()).await {
                if let Some(session) = diagnostics.as_ref() {
                    session.set_page_state(state);
                }
            }
        }

        // EARLY DESTROY: In AlwaysNew mode, destroy context immediately after getting content
        // This frees the slot for the next request BEFORE we spend time on diagnostics/logging
        // Critical for high-throughput scenarios where max_contexts=1
        if self.config.scope.session_mode == SessionMode::AlwaysNew {
            // Release the tab lock first. `destroy_context` -> `close_context_tab` takes the very
            // same `context.tab` mutex to hand the tab to the detached closer, so holding it here
            // makes this task await itself: the request never returns and the slot is only freed
            // when the client's deadline drops the future. `tab` is an `Arc<Tab>` clone taken
            // above and stays valid for the status/URL reads further down.
            drop(tab_guard);

            let browser_pool = self.browser_pool.read().await;
            browser_pool.destroy_context(&context.metadata.id).await;
            info!(
                "Early context destroy completed (AlwaysNew mode): {}",
                context.metadata.id
            );
        }

        // DO NOT close tab - keep it for session reuse!

        // Drain the captured main-document response (status + headers + final URL).
        let MainDocumentResponse {
            status: observed_status,
            headers: response_headers,
            url: observed_url,
        } = main_response_holder
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();

        // HTTP status code: prefer the authoritative value from the CDP response observer.
        // Fall back to the Performance API only when the observer captured nothing — the
        // Network domain could not be enabled, or the navigation produced no response such
        // as a chrome-error:// page. Returns 0 if it still cannot be determined.
        let status_code = if observed_status > 0 {
            observed_status
        } else {
            tab.evaluate(
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
            .unwrap_or(0) as u32
        };

        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        // Off-domain redirect detection. The final landing URL comes from the response
        // observer (authoritative network-layer URL); fall back to the committed tab URL if
        // the observer captured nothing. Only meaningful when navigation succeeded.
        let final_url = if observed_url.is_empty() {
            tab.get_url()
        } else {
            observed_url
        };
        let redirect_target = if navigation_result.is_ok() {
            cross_site_redirect_target(&req.url, &final_url)
        } else {
            None
        };
        if let Some(target_domain) = &redirect_target {
            debug!(
                "Off-domain redirect detected: {} -> {} (registrable domain '{}')",
                req.url, final_url, target_domain
            );
        }

        // Resource loads lost to the proxy path, recorded by the response observer. Always
        // logged when present, whatever the request's outcome and independent of the browser
        // diagnostics switch: they are rare, actionable, and invisible in the returned content.
        let (proxy_failure_count, proxy_failure_summary) = {
            let failures = proxy_failure_holder.lock().unwrap();
            (failures.total, failures.summary())
        };
        if proxy_failure_count > 0 {
            warn!(
                "{} resource load(s) failed with a proxy/tunnel error: {}",
                proxy_failure_count, proxy_failure_summary
            );
        }

        // Build response based on navigation result and wait result.
        // Priority: navigation error > off-domain redirect > wait result.
        let (success, error_message, error_code) = if let Err(e) = navigation_result {
            // Navigation failed. Separate a dead proxy path from a site-side network error:
            // the former is retryable by the client and says nothing about the target site.
            let text = e.to_string();
            if is_proxy_error(&text) {
                (
                    false,
                    format!("Proxy/tunnel failure while navigating: {}", text),
                    ErrorCode::ProxyError,
                )
            } else {
                (
                    false,
                    format!("Failed to navigate to URL: {}", text),
                    ErrorCode::NetworkError,
                )
            }
        } else if redirect_target.is_some() {
            // Redirected to another domain. wait_selector/skip_selector were evaluated
            // against a foreign page, so their outcome is meaningless — discard it and report
            // the redirect instead. This deliberately suppresses any "wait selector not found"
            // / "skip selector found" message.
            (
                false,
                format!(
                    "Redirected to another domain: {} (from {})",
                    final_url, req.url
                ),
                ErrorCode::RedirectToAnotherDomain,
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

        // A page whose scripts were lost to a dead tunnel renders as an unfilled template, so
        // it fails as a plain "selector not found" that blames the site. Report the cause
        // instead — but only when the request already failed for a reason those missing
        // resources can explain:
        //   - a successful request stays successful: the client got what it asked for, and the
        //     failures are only in the log;
        //   - a found skip_selector stays as it is: that element really was present, which a
        //     network failure cannot invalidate;
        //   - an off-domain redirect stays as it is: it is a definitive observation.
        // Hard-timeout and cancellation paths return earlier and are not covered here.
        let (error_message, error_code) = if proxy_failure_count > 0
            && matches!(
                error_code,
                ErrorCode::SelectorNotFound | ErrorCode::TimeoutBrowser
            ) {
            (
                format!(
                    "{} — {} resource load(s) failed with a proxy/tunnel error: {}",
                    error_message, proxy_failure_count, proxy_failure_summary
                ),
                ErrorCode::ProxyError,
            )
        } else {
            (error_message, error_code)
        };

        // Tell the diagnostics session how this request ended, so `on_error` mode can decide
        // whether to log. A found skip_selector counts as success: the client asked for that
        // check, so it is an expected outcome rather than a malfunction to explain.
        if let Some(session) = diagnostics.as_mut() {
            if success || error_code == ErrorCode::SkipSelectorFound {
                session.mark_success();
            }
        }

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
            response_headers,
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

        // Per-request span: every log line emitted while handling this request inherits
        // this context, so in JSON logs it lands under the `span` object (Loki: span_ray_id,
        // span_url, …) while the message stays clean. Optional fields start Empty and are
        // either recorded below (when present in the request) or later once known
        // (context_id, wait_strategy, wait_timeout_ms); Empty fields are omitted from output.
        let span = tracing::info_span!(
            "scrape_page",
            scope = %self.config.scope.name,
            ray_id = %req.ray_id,
            url = %req.url,
            wait_selector = tracing::field::Empty,
            skip_selector = tracing::field::Empty,
            country_code = tracing::field::Empty,
            wait_strategy = tracing::field::Empty,
            wait_timeout_ms = tracing::field::Empty,
            context_id = tracing::field::Empty,
            proxy_host = tracing::field::Empty,
        );
        if !req.wait_selector.is_empty() {
            span.record("wait_selector", req.wait_selector.as_str());
        }
        if !req.skip_selector.is_empty() {
            span.record("skip_selector", req.skip_selector.as_str());
        }
        if !req.country_code.is_empty() {
            span.record("country_code", req.country_code.as_str());
        }

        async move {
        // Get ray_id from request (coordinator generates it if not provided by client)
        let ray_id = req.ray_id.clone();

        // Track active request (automatically decrements on drop)
        let _active_guard = ActiveRequestGuard::new(self.active_requests.clone());

        // Observe end-to-end request duration on every return path (drop).
        let _duration_timer = RequestTimer::new(
            self.metrics
                .request_duration_seconds
                .with_label_values(&[&self.config.scope.name]),
        );

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

        info!("Received scraping request for URL: {}", req.url);

        self.total_requests.fetch_add(1, Ordering::SeqCst);
        self.metrics
            .requests_total
            .with_label_values(&[&self.config.scope.name])
            .inc();

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
            info!("Looking for existing context: {}", req.context_id);
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
                Err(response) => {
                    self.record_failed_if_5xxx(response.get_ref());
                    return Ok(response);
                }
            }
        };

        // Now that a context is bound to the request, add it to the request span so every
        // subsequent log line carries it.
        tracing::Span::current().record("context_id", context.metadata.id.to_string().as_str());

        // The exit the response came through. Without it, correlating a block or a rate limit
        // with an address means reproducing the request by hand — which is how the pool's
        // effective host went unnoticed for months. Credentials are never part of this value.
        if let Some(proxy_host) = context.proxy_host.as_deref() {
            tracing::Span::current().record("proxy_host", proxy_host);
        }

        // Guaranteed cleanup (AlwaysNew mode): tie the context's removal to this scope's
        // lifetime rather than to control flow, so it also runs when the handler future is
        // dropped mid-request or panics. Idempotent with the early destroy below.
        let mut always_new_guard = self.always_new_guard_for(is_always_new, &context, &ray_id);

        // Read *after* the context is bound, so only a pool replacement that happens during the
        // attempt below counts - `acquire_context_with_recovery` may legitimately have replaced
        // the pool while getting us this context, and that one is already recovered from.
        let generation_at_start = self.pool_generation.load(Ordering::SeqCst);

        // Execute scraping
        // NOTE: In AlwaysNew mode, context is destroyed inside scrape_page_internal
        // immediately after getting content (before diagnostics) for faster slot release
        let mut result = self.scrape_page_internal(&req, context, &ray_id).await;

        // The browser process died mid-request and the pool was replaced, so the context that
        // attempt held belonged to a dead process: retry the whole thing once against the new
        // pool. The two conditions together are precise, not a heuristic - whoever replaced the
        // pool, it is the same browser process every context in it lived in, so a replacement
        // plus a failure means this request's own browser died and nothing was scraped. A
        // *successful* response is never retried, so no page is ever loaded twice.
        //
        // Exactly once: a second death during the retry is a browser that cannot stay alive, and
        // looping on it would only hold the slot.
        let pool_was_replaced = self.pool_generation.load(Ordering::SeqCst) != generation_at_start;
        if pool_was_replaced && matches!(&result, Ok(response) if !response.success) {
            // The old context's tab lives in the dead browser, so there is nothing to close; the
            // guard would only look for it in the new pool and find nothing.
            drop(always_new_guard);

            match self
                .acquire_context_with_recovery(start_time, &ray_id, &proxy_params)
                .await
            {
                Ok(fresh_context) => {
                    info!(
                        "Retrying with context {} from the recreated pool",
                        fresh_context.metadata.id
                    );
                    // Re-record: the span must name the context that produced the response.
                    let span = tracing::Span::current();
                    span.record("context_id", fresh_context.metadata.id.to_string().as_str());
                    if let Some(proxy_host) = fresh_context.proxy_host.as_deref() {
                        span.record("proxy_host", proxy_host);
                    }

                    always_new_guard =
                        self.always_new_guard_for(is_always_new, &fresh_context, &ray_id);
                    result = self
                        .scrape_page_internal(&req, fresh_context, &ray_id)
                        .await;
                }
                Err(response) => {
                    self.record_failed_if_5xxx(response.get_ref());
                    return Ok(response);
                }
            }
        }
        // The scrape is finished and its response is in hand, so the AlwaysNew context can go.
        drop(always_new_guard);

        // Update metrics
        match &result {
            Ok(response) => {
                // Pool gauges (contexts/slots) are refreshed on each Prometheus scrape
                self.record_failed_if_5xxx(response);
                let hash = content_hash(&response.content);

                info!(
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
                error!("Request FAILED: {:?}", status);
            }
        }

        result.map(Response::new)
        }
        .instrument(span)
        .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// The classifier must fire on the proxy path only. Origin-side failures share the same
    /// event and the same shape, and mistaking one for the other would blame the proxy for a
    /// site being down — or, worse, hide a site outage behind a "retry, it's the proxy" code.
    #[test]
    fn proxy_errors_are_told_apart_from_origin_errors() {
        assert!(is_proxy_error("net::ERR_TUNNEL_CONNECTION_FAILED"));
        assert!(is_proxy_error("net::ERR_PROXY_CONNECTION_FAILED"));
        assert!(is_proxy_error(
            "Navigate failed: net::ERR_TUNNEL_CONNECTION_FAILED"
        ));

        assert!(!is_proxy_error("net::ERR_FAILED"));
        assert!(!is_proxy_error("net::ERR_CONNECTION_REFUSED"));
        assert!(!is_proxy_error("net::ERR_NAME_NOT_RESOLVED"));
        assert!(!is_proxy_error("net::ERR_ABORTED"));
        assert!(!is_proxy_error(""));
    }

    #[test]
    fn proxy_failures_collapse_duplicates_and_cap_distinct_kinds() {
        let mut failures = ProxyFailures::default();
        for _ in 0..4 {
            failures.record("Script net::ERR_TUNNEL_CONNECTION_FAILED".to_string());
        }
        failures.record("Stylesheet net::ERR_PROXY_CONNECTION_FAILED".to_string());

        assert_eq!(failures.total, 5);
        assert_eq!(
            failures.summary(),
            "Script net::ERR_TUNNEL_CONNECTION_FAILED (x4), \
             Stylesheet net::ERR_PROXY_CONNECTION_FAILED"
        );

        // Past the cap only the total grows, so a page failing every resource cannot grow the
        // log line without bound.
        for i in 0..50 {
            failures.record(format!("Kind{} net::ERR_TUNNEL_CONNECTION_FAILED", i));
        }
        assert_eq!(failures.total, 55);
        assert_eq!(failures.by_kind.len(), MAX_PROXY_FAILURE_KINDS);
    }

    #[test]
    fn busy_guard_rejects_a_context_that_is_already_busy() {
        let is_busy = Arc::new(AtomicBool::new(true));

        assert!(ContextBusyGuard::new(is_busy).is_err());
    }

    /// AlwaysNew contexts arrive pre-marked busy from `create_always_new_context`, so the
    /// guard must adopt the flag. Using `new` here would reject a perfectly valid context.
    #[test]
    fn busy_guard_adopts_a_pre_marked_context_and_clears_it_on_drop() {
        let is_busy = Arc::new(AtomicBool::new(true));

        {
            let _guard = ContextBusyGuard::adopt(is_busy.clone());
            assert!(is_busy.load(Ordering::SeqCst), "stays busy while in scope");
        }

        assert!(
            !is_busy.load(Ordering::SeqCst),
            "adopted flag is cleared on drop, so the context becomes reclaimable"
        );
    }

    #[test]
    fn cross_site_redirect_is_flagged_only_across_registrable_domains() {
        // Same host / same page → not a redirect.
        assert_eq!(
            cross_site_redirect_target("https://example.com/a", "https://example.com/b"),
            None
        );
        // Subdomains of the same eTLD+1 are the SAME site → allowed.
        assert_eq!(
            cross_site_redirect_target("https://www.example.com/", "https://shop.example.com/"),
            None
        );
        assert_eq!(
            cross_site_redirect_target("https://example.com/", "https://www.example.com/"),
            None
        );
        // Different registrable domain → flagged, returning the landing domain.
        assert_eq!(
            cross_site_redirect_target("https://example.com/", "https://evil.org/login"),
            Some("evil.org".to_string())
        );
        // Multi-label public suffix (eTLD) must not be mis-split: these are the SAME site.
        assert_eq!(
            cross_site_redirect_target("https://www.example.co.uk/", "https://shop.example.co.uk/"),
            None
        );
        // …but a different eTLD+1 under the same ccTLD is still cross-site.
        assert_eq!(
            cross_site_redirect_target("https://example.co.uk/", "https://other.co.uk/"),
            Some("other.co.uk".to_string())
        );
        // Unparseable / hostless inputs err toward "not a redirect".
        assert_eq!(
            cross_site_redirect_target("not a url", "https://example.com/"),
            None
        );
        assert_eq!(cross_site_redirect_target("https://example.com/", ""), None);
    }
}
