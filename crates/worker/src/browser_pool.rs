use anyhow::Result;
use browser_hive_common::{
    BrowserContextMetadata, ContextIsolation, ContextLifecycleConfig, ProxyConfig, ProxyParams,
    ProxyProvider, RotationStrategy, ScopeConfig, SessionMode, TabInitMiddleware,
};
use headless_chrome::browser::tab::Tab;
use headless_chrome::protocol::cdp::Target::CreateTarget;
use headless_chrome::{Browser, LaunchOptions};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

pub struct BrowserPool {
    browser: Arc<Browser>,
    contexts: Arc<RwLock<Vec<Arc<BrowserContext>>>>,
    scope_config: ScopeConfig,
    lifecycle_config: ContextLifecycleConfig,
    proxy_config: ProxyConfig,
    proxy_provider: Box<dyn ProxyProvider>, // For per-context proxy assignment

    // Browser customization middlewares
    tab_init_middlewares: Vec<Box<dyn TabInitMiddleware>>, // Applied to each new tab after creation

    // Metrics
    total_contexts_created: Arc<AtomicU64>,
    total_contexts_recycled: Arc<AtomicU64>,
}

/// Remove leaked contexts from an `AlwaysNew` pool, returning the ones that were removed.
///
/// In `SessionMode::AlwaysNew` a context is owned by exactly one request: it is created
/// pre-marked busy and removed from the pool when that request's scope ends. A context that
/// is neither busy nor removed can therefore only be a leak — for example when the request
/// future was dropped mid-flight. Such a context is never reused (AlwaysNew always creates
/// fresh ones), so without reclamation it consumes a slot until the worker restarts.
///
/// Pre-marking at creation is what makes this safe: a context that was handed out but has
/// not started processing yet is already busy, so a concurrent request can never collect it.
///
/// The removed contexts are returned rather than dropped: dropping frees nothing inside Chrome,
/// so the caller must pass each to `close_context_tab` — which it can only do after releasing
/// the pool lock this function is called under.
fn reclaim_leaked_always_new_contexts(
    contexts: &mut Vec<Arc<BrowserContext>>,
) -> Vec<Arc<BrowserContext>> {
    let mut leaked = Vec::new();
    contexts.retain(|c| {
        if c.metadata.is_busy.load(Ordering::SeqCst) {
            true
        } else {
            leaked.push(c.clone());
            false
        }
    });
    leaked
}

/// Close a tab inside Chrome after its context has been removed from the pool.
///
/// Removing a context from the pool `Vec` frees nothing browser-side: headless_chrome's `Tab`
/// and `Context` have no `Drop` impl and the crate never disposes either, so a dropped handle
/// leaves a live tab — with its renderer, sockets and proxy tunnels — inside Chrome. In
/// `SessionMode::AlwaysNew` that would be one leaked tab per request, for the pod's lifetime.
///
/// The call runs **detached on the blocking pool**, for two reasons. `Tab::close` is a
/// synchronous CDP round-trip, so it must not run on a runtime thread. And its wait is bounded
/// only by `idle_browser_timeout` (1 hour, see `BrowserPool::new`), while the tab being closed
/// is frequently the one that just stopped responding — awaiting it would stall the request
/// path for that whole hour. Nothing depends on the outcome, so it is only logged.
///
/// The now-empty CDP BrowserContext is *not* disposed: `Target.disposeBrowserContext` is
/// rejected over a page session (`Not allowed`) and headless_chrome exposes no browser-level
/// method call. An empty context holds no renderer and no sockets, so that residue is minor.
fn close_tab_detached(tab: Arc<Tab>, context_id: uuid::Uuid) {
    // The request span does not cross spawn_blocking; re-enter it so these lines keep ray_id.
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _guard = span.enter();
        match tab.close(false) {
            Ok(_) => debug!("Closed tab of removed context {}", context_id),
            // Usually means the tab was already gone (dead CDP session) — benign, and the
            // opposite of the leak this guards against.
            Err(e) => debug!(
                "Could not close tab of removed context {}: {}",
                context_id, e
            ),
        }
    });
}

/// Take a removed context's tab and close it in Chrome. Must be called with the pool lock
/// released. See `close_tab_detached`.
/// How long context teardown may wait for `context.tab` before giving up.
///
/// The lock is normally uncontended: whoever owns the context has finished with it by the time it
/// is destroyed. The bound exists because the failure mode of an unbounded wait is far worse than
/// the failure mode of a timeout — see [`close_context_tab`].
const TAB_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Hand the context's tab to the detached closer.
///
/// The wait for `context.tab` is bounded on purpose. This runs on the request path (the
/// `AlwaysNew` early destroy), so a caller still holding that mutex would park this task forever —
/// which is exactly what shipped in v0.17.0, where `scrape_page_internal` held its `tab_guard`
/// across the destroy and every `always_new` request hung until its client gave up. Timing out
/// instead leaks one tab in Chrome: bad, but bounded, loud, and it still returns the response.
async fn close_context_tab(context: &Arc<BrowserContext>) {
    let tab = match tokio::time::timeout(TAB_LOCK_TIMEOUT, context.tab.lock()).await {
        Ok(mut guard) => guard.take(),
        Err(_) => {
            warn!(
                "Timed out after {:?} waiting for the tab lock of context {} - leaving its tab \
                 open in Chrome. Some caller is holding context.tab across the destroy; that is a \
                 bug in the caller, not a transient condition.",
                TAB_LOCK_TIMEOUT, context.metadata.id
            );
            return;
        }
    };

    if let Some(tab) = tab {
        close_tab_detached(tab, context.metadata.id);
    }
}

pub struct BrowserContext {
    pub metadata: BrowserContextMetadata,
    pub tab: Arc<Mutex<Option<Arc<Tab>>>>, // Reusable tab for session persistence
    /// CDP BrowserContext ID when running in isolated mode.
    /// None for shared mode (tab in default browser context).
    /// Some(id) for isolated mode (tab in dedicated CDP BrowserContext).
    pub cdp_context_id: Option<String>,
}

impl BrowserPool {
    pub async fn new(scope_config: ScopeConfig) -> Result<Self> {
        info!("Launching Chrome process for scope: {}", scope_config.name);

        // Build proxy configuration
        let proxy_config = scope_config.proxy_provider.build_config()?;

        // Get proxy server URL (None if no proxy)
        let proxy_server = proxy_config.build_proxy_server();

        // Log proxy configuration
        if let Some(server) = &proxy_server {
            if proxy_config.get_credentials().is_some() {
                info!(
                    "Using proxy: {} (with authentication via Fetch API)",
                    server
                );
            } else {
                info!("Using proxy: {} (no authentication)", server);
            }
        } else {
            info!("No proxy configured - using direct connection");
        }

        // Build Chrome args using binary params middlewares
        let mut chrome_args: Vec<&'static OsStr> = Vec::new();

        info!(
            "Applying {} binary params middleware(s)",
            scope_config.binary_params_middlewares.len()
        );
        for middleware in &scope_config.binary_params_middlewares {
            info!("  - Applying middleware: {}", middleware.name());
            middleware.apply_args(&mut chrome_args, scope_config.headless);
        }

        if scope_config.headless {
            info!(
                "Launching Chrome in HEADLESS mode (faster, more detectable) with {} args",
                chrome_args.len()
            );
        } else {
            info!(
                "Launching Chrome in HEADFULL mode (slower, better stealth) with {} args",
                chrome_args.len()
            );
        }

        // Build launch options
        let mut launch_builder = LaunchOptions::default_builder();
        launch_builder
            .headless(scope_config.headless)
            .proxy_server(proxy_server.as_deref())
            // Set a very long idle_browser_timeout to prevent WebSocket from closing
            // Default is 30 seconds which causes "connection is closed" errors during navigation
            // We set it to 1 hour - if browser is truly idle for that long, it's safe to restart
            .idle_browser_timeout(Duration::from_secs(3600))
            .args(chrome_args);

        // Use custom browser path if specified (e.g., for Brave: /usr/bin/brave-browser)
        if let Some(ref browser_path) = scope_config.browser_path {
            info!("Using custom browser binary: {}", browser_path.display());
            launch_builder.path(Some(browser_path.clone()));
        }

        let launch_options = launch_builder
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build launch options: {}", e))?;

        // Pre-flight: verify browser binary exists and is executable
        let browser_binary = scope_config
            .browser_path
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("chromium"));
        Self::verify_browser_binary(browser_binary);

        // Launch browser
        info!("Attempting to launch browser process...");
        let browser = Browser::new(launch_options).map_err(|e| {
            tracing::error!(
                "FATAL: Browser failed to launch. Error: {}. \
                 Common causes: (1) --no-sandbox not set but running in a container without SYS_ADMIN capability, \
                 (2) browser binary not found at specified path, \
                 (3) missing shared libraries. \
                 Check that the browser binary exists and has correct permissions.",
                e
            );
            e
        })?;
        info!("Browser process launched successfully");

        let lifecycle_config = scope_config.lifecycle.clone();

        // Clone proxy provider for per-context assignment
        let proxy_provider = scope_config.proxy_provider.clone();

        // Clone tab init middlewares for tab customization
        let tab_init_middlewares = scope_config.tab_init_middlewares.clone();

        info!(
            "Browser initialized with {} tab init middleware(s)",
            tab_init_middlewares.len()
        );
        for middleware in &tab_init_middlewares {
            info!("  - Registered middleware: {}", middleware.name());
        }

        // Log context isolation mode
        let isolation_mode = match scope_config.context_isolation {
            ContextIsolation::Isolated => "ISOLATED (each context has separate cookies/storage)",
            ContextIsolation::Shared => "SHARED (all contexts share cookies/storage)",
        };
        info!("Context isolation mode: {}", isolation_mode);

        let pool = Self {
            browser: Arc::new(browser),
            contexts: Arc::new(RwLock::new(Vec::new())),
            scope_config: scope_config.clone(),
            lifecycle_config,
            proxy_config,
            proxy_provider,
            tab_init_middlewares,
            total_contexts_created: Arc::new(AtomicU64::new(0)),
            total_contexts_recycled: Arc::new(AtomicU64::new(0)),
        };

        // Log session mode and conditionally pre-initialize contexts
        match scope_config.session_mode {
            SessionMode::AlwaysNew => {
                info!("Session mode: ALWAYS_NEW (fresh context per request, destroyed after)");
                info!(
                    "Starting with 0 contexts - will create on-demand up to {}",
                    scope_config.max_contexts
                );
            }
            SessionMode::Reusable => {
                info!(
                    "Session mode: REUSABLE (contexts reused until recycled by lifecycle monitor)"
                );
                info!(
                    "Starting with 0 contexts - will create on-demand up to {}",
                    scope_config.max_contexts
                );
            }
            SessionMode::ReusablePreinit => {
                info!("Session mode: REUSABLE_PREINIT (reusable contexts, pre-created on startup)");
                info!(
                    "Pre-initializing {} contexts on startup (min: {}, max: {})",
                    scope_config.min_contexts, scope_config.min_contexts, scope_config.max_contexts
                );
                pool.populate_initial_contexts().await?;
            }
        }

        // Start lifecycle monitor
        pool.start_lifecycle_monitor();

        Ok(pool)
    }

    /// Pre-flight check: verify browser binary exists and log useful diagnostics
    fn verify_browser_binary(binary_path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        if !binary_path.exists() {
            warn!(
                "Browser binary not found at '{}'. \
                 Browser launch will likely fail.",
                binary_path.display()
            );
            return;
        }

        match std::fs::metadata(binary_path) {
            Ok(metadata) => {
                let mode = metadata.permissions().mode();
                let is_executable = mode & 0o111 != 0;
                if !is_executable {
                    warn!(
                        "Browser binary '{}' exists but is NOT executable (mode: {:o})",
                        binary_path.display(),
                        mode
                    );
                } else {
                    info!(
                        "Browser binary verified: '{}' (mode: {:o})",
                        binary_path.display(),
                        mode
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Cannot read metadata for browser binary '{}': {}",
                    binary_path.display(),
                    e
                );
            }
        }

        // Log current user — helps diagnose sandbox/permission issues
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            info!(
                "Running as non-root user (uid: {}). If browser fails to start, \
                 verify that container security context and capabilities are configured correctly.",
                uid
            );
        }
    }

    async fn populate_initial_contexts(&self) -> Result<()> {
        let total_start = std::time::Instant::now();

        info!(
            "Creating {} initial browser contexts (tabs)",
            self.scope_config.min_contexts
        );

        let mut contexts = self.contexts.write().await;
        let default_params = ProxyParams::default();

        for _ in 0..self.scope_config.min_contexts {
            let context = self.create_new_context(&default_params).await?;
            contexts.push(Arc::new(context));
        }

        let total_time_ms = total_start.elapsed().as_millis();
        let avg_time_ms = if contexts.len() > 0 {
            total_time_ms / contexts.len() as u128
        } else {
            0
        };

        info!(
            "Successfully created {} browser contexts in {}ms (avg {}ms per context)",
            contexts.len(),
            total_time_ms,
            avg_time_ms
        );

        Ok(())
    }

    async fn create_new_context(&self, proxy_params: &ProxyParams) -> Result<BrowserContext> {
        let start_time = std::time::Instant::now();

        let mut metadata = BrowserContextMetadata::new();

        // Assign per-context proxy if provider supports it
        if self.proxy_provider.supports_per_context_proxy() {
            if let Some(context_proxy) = self
                .proxy_provider
                .get_context_proxy_with_params(&metadata.id.to_string(), proxy_params)
            {
                info!(
                    "Assigning context-specific proxy to context {} (country_code: {:?})",
                    metadata.id, proxy_params.country_code
                );
                metadata.assigned_proxy_config = Some(context_proxy);
            }
        }

        // Create tab based on isolation mode
        let (new_tab, cdp_context_id) = match self.scope_config.context_isolation {
            ContextIsolation::Isolated => {
                // Create isolated CDP BrowserContext (like incognito - separate cookies/storage)
                let cdp_context = self
                    .browser
                    .new_context()
                    .map_err(|e| anyhow::anyhow!("Failed to create isolated CDP context: {}", e))?;

                let context_id = cdp_context.get_id().to_string();

                // Create tab within the isolated context
                let tab = cdp_context.new_tab().map_err(|e| {
                    anyhow::anyhow!("Failed to create tab in isolated context: {}", e)
                })?;

                info!("Created ISOLATED CDP context {} with tab", context_id);

                (tab, Some(context_id))
            }
            ContextIsolation::Shared => {
                // Create tab in default browser context (shared cookies/storage)
                let tab = self
                    .browser
                    .new_tab()
                    .map_err(|e| anyhow::anyhow!("Failed to create tab: {}", e))?;

                (tab, None)
            }
        };

        // Apply tab init middlewares to customize the newly created tab
        for middleware in &self.tab_init_middlewares {
            if let Err(e) = middleware.apply(&new_tab) {
                warn!(
                    "Failed to apply tab init middleware '{}': {}",
                    middleware.name(),
                    e
                );
            } else {
                tracing::debug!(
                    "Successfully applied tab init middleware '{}'",
                    middleware.name()
                );
            }
        }

        let creation_time_ms = start_time.elapsed().as_millis();
        let isolation_mode = if cdp_context_id.is_some() {
            "isolated"
        } else {
            "shared"
        };

        info!(
            "Created browser context ({}) for metadata id: {} in {}ms",
            isolation_mode, metadata.id, creation_time_ms
        );

        self.total_contexts_created.fetch_add(1, Ordering::SeqCst);

        Ok(BrowserContext {
            metadata,
            tab: Arc::new(Mutex::new(Some(new_tab))),
            cdp_context_id,
        })
    }

    pub fn start_lifecycle_monitor(&self) {
        let contexts = self.contexts.clone();
        let lifecycle_config = self.lifecycle_config.clone();
        let total_recycled = self.total_contexts_recycled.clone();
        let browser = self.browser.clone();
        let total_created = self.total_contexts_created.clone();
        let proxy_provider = self.proxy_provider.clone();
        let tab_init_middlewares = self.tab_init_middlewares.clone();
        let context_isolation = self.scope_config.context_isolation;
        let session_mode = self.scope_config.session_mode;

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;

                let mut contexts_guard = contexts.write().await;

                // AlwaysNew: contexts are owned by a single request and removed when it ends,
                // so an idle context here is a leak. Drop it instead of recycling it —
                // recycling would replace it with a fresh context that keeps holding the slot,
                // which is what previously turned a one-off leak into a permanently full pool.
                if session_mode == SessionMode::AlwaysNew {
                    let leaked = reclaim_leaked_always_new_contexts(&mut contexts_guard);
                    if !leaked.is_empty() {
                        warn!(
                            ray_id = "lifecycle-monitor",
                            "Removed {} leaked idle context(s) in AlwaysNew mode ({} remaining)",
                            leaked.len(),
                            contexts_guard.len()
                        );
                    }
                    drop(contexts_guard);
                    for context in &leaked {
                        close_context_tab(context).await;
                    }
                    continue;
                }

                let mut to_recycle = Vec::new();

                for (idx, context) in contexts_guard.iter().enumerate() {
                    if Self::should_recycle_context(context, &lifecycle_config).await {
                        to_recycle.push(idx);
                    }
                }

                // Recycle contexts (only if not actively processing a request)
                for idx in to_recycle.iter().rev() {
                    let context = &contexts_guard[*idx];

                    // Check if context is idle (not busy)
                    if !context
                        .metadata
                        .is_busy
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        info!(
                            ray_id = "lifecycle-monitor",
                            "Recycling context {} (age: {:?}, requests: {})",
                            context.metadata.id,
                            context.metadata.created_at.elapsed(),
                            context.metadata.total_requests.load(Ordering::SeqCst)
                        );

                        // Close the old tab in Chrome. Dropping the handle does not close it —
                        // headless_chrome's Tab has no Drop — so the recycled-away tab would
                        // otherwise keep its renderer, sockets and proxy tunnel alive forever.
                        let old_tab = context.tab.lock().await.take();
                        if let Some(old_tab) = old_tab {
                            close_tab_detached(old_tab, context.metadata.id);
                        }

                        // Create new context metadata
                        let mut metadata = BrowserContextMetadata::new();

                        // Assign per-context proxy if provider supports it
                        // Note: Lifecycle recycling uses default ProxyParams (no country_code)
                        // since we don't have request context here
                        if proxy_provider.supports_per_context_proxy() {
                            let default_params = ProxyParams::default();
                            if let Some(context_proxy) = proxy_provider
                                .get_context_proxy_with_params(
                                    &metadata.id.to_string(),
                                    &default_params,
                                )
                            {
                                info!(
                                    ray_id = "lifecycle-monitor",
                                    "Assigning context-specific proxy to recycled context {}",
                                    metadata.id
                                );
                                metadata.assigned_proxy_config = Some(context_proxy);
                            }
                        }

                        // Create tab based on isolation mode
                        let (tab, cdp_context_id) = match context_isolation {
                            ContextIsolation::Isolated => {
                                // Create isolated CDP BrowserContext
                                match browser.new_context() {
                                    Ok(cdp_context) => {
                                        let ctx_id = cdp_context.get_id().to_string();
                                        match cdp_context.new_tab() {
                                            Ok(new_tab) => {
                                                // Apply tab init middlewares
                                                for middleware in &tab_init_middlewares {
                                                    if let Err(e) = middleware.apply(&new_tab) {
                                                        tracing::warn!(
                                                            ray_id = "lifecycle-monitor",
                                                            "Failed to apply tab init middleware '{}': {}",
                                                            middleware.name(),
                                                            e
                                                        );
                                                    }
                                                }
                                                (Some(new_tab), Some(ctx_id))
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    ray_id = "lifecycle-monitor",
                                                    "Failed to create tab in isolated context: {}",
                                                    e
                                                );
                                                (None, None)
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            ray_id = "lifecycle-monitor",
                                            "Failed to create isolated context during recycling: {}",
                                            e
                                        );
                                        (None, None)
                                    }
                                }
                            }
                            ContextIsolation::Shared => {
                                // Create tab in default browser context (shared cookies/storage)
                                match browser.new_tab() {
                                    Ok(new_tab) => {
                                        tracing::debug!(
                                            ray_id = "lifecycle-monitor",
                                            "Successfully created tab during context recycling"
                                        );

                                        // Apply tab init middlewares to customize the tab
                                        for middleware in &tab_init_middlewares {
                                            if let Err(e) = middleware.apply(&new_tab) {
                                                tracing::warn!(
                                                    ray_id = "lifecycle-monitor",
                                                    "Failed to apply tab init middleware '{}' during recycling: {}",
                                                    middleware.name(),
                                                    e
                                                );
                                            }
                                        }

                                        (Some(new_tab), None)
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            ray_id = "lifecycle-monitor",
                                            "Failed to create tab during recycling (likely WebSocket timeout): {}. \
                                            Context will be created without tab - it will be lazily initialized on next request.",
                                            e
                                        );
                                        (None, None)
                                    }
                                }
                            }
                        };

                        // Always replace the old context with a new one, even if tab creation failed.
                        // This ensures we break the cycle of trying to recycle the same broken context.
                        let new_context = BrowserContext {
                            metadata,
                            tab: Arc::new(Mutex::new(tab)),
                            cdp_context_id,
                        };

                        contexts_guard[*idx] = Arc::new(new_context);
                        total_recycled.fetch_add(1, Ordering::SeqCst);
                        total_created.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
    }

    async fn should_recycle_context(
        context: &BrowserContext,
        config: &ContextLifecycleConfig,
    ) -> bool {
        match config.rotation_strategy {
            RotationStrategy::TimeBasedOnly => {
                context.metadata.created_at.elapsed() > config.max_lifetime
            }
            RotationStrategy::RequestBasedOnly => {
                context.metadata.total_requests.load(Ordering::SeqCst) > config.max_requests
            }
            RotationStrategy::Hybrid => {
                let age_exceeded = context.metadata.created_at.elapsed() > config.max_lifetime;
                let requests_exceeded =
                    context.metadata.total_requests.load(Ordering::SeqCst) > config.max_requests;
                let idle_too_long =
                    context.metadata.last_used_at.lock().await.elapsed() > config.max_idle_time;
                let cache_too_large = context.metadata.cache_size_mb.load(Ordering::SeqCst)
                    > config.max_cache_size_mb;

                age_exceeded || requests_exceeded || idle_too_long || cache_too_large
            }
        }
    }

    /// Find context by ID (for session persistence)
    pub async fn find_context_by_id(&self, context_id: &str) -> Option<Arc<BrowserContext>> {
        let contexts = self.contexts.read().await;

        contexts
            .iter()
            .find(|c| c.metadata.id.to_string() == context_id)
            .cloned()
    }

    /// Find best context for a specific domain (warm cache optimization)
    pub async fn find_best_context_for_domain(&self, domain: &str) -> Option<Arc<BrowserContext>> {
        let contexts = self.contexts.read().await;

        // First, try to find an idle context that already scraped this domain
        for context in contexts.iter() {
            let domains = context.metadata.primary_domains.read().await;
            let is_busy = context
                .metadata
                .is_busy
                .load(std::sync::atomic::Ordering::SeqCst);
            if domains.contains(domain) && !is_busy {
                return Some(context.clone());
            }
        }

        // If not found, get any idle context
        self.find_least_busy_context().await
    }

    pub async fn find_least_busy_context(&self) -> Option<Arc<BrowserContext>> {
        let contexts = self.contexts.read().await;

        // Find first idle context
        contexts
            .iter()
            .find(|c| !c.metadata.is_busy.load(std::sync::atomic::Ordering::SeqCst))
            .cloned()
    }

    /// Get or create a new context on-demand.
    ///
    /// This method is used in on-demand mode to create contexts when needed.
    /// It will:
    /// 1. Try to find an idle existing context
    /// 2. If none found and under max_contexts limit, create a new one
    /// 2. If none found and under max_contexts limit, create a new one
    /// 3. If at max_contexts limit, return None (resource exhausted)
    ///
    /// # Parameters
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    pub async fn get_or_create_context(
        &self,
        proxy_params: &ProxyParams,
    ) -> Result<Option<Arc<BrowserContext>>> {
        // If request has proxy routing overrides (e.g. country_code), we must create
        // a dedicated context because these params affect the proxy connection identity
        // (exit IP, geo) and can't be changed on an existing context.
        if !proxy_params.requires_dedicated_context() {
            // No routing overrides - try to reuse an idle context
            if let Some(context) = self.find_least_busy_context().await {
                info!(
                    "Reusing idle context: {} (total_requests: {})",
                    context.metadata.id,
                    context
                        .metadata
                        .total_requests
                        .load(std::sync::atomic::Ordering::SeqCst)
                );
                return Ok(Some(context));
            }
        } else {
            debug!(
                "Request has proxy routing overrides (country_code={:?}) - creating dedicated context",
                proxy_params.country_code
            );
        }

        // No idle context available (or dedicated context required) - try to create a new one
        let mut contexts = self.contexts.write().await;

        if !proxy_params.requires_dedicated_context() {
            // Double-check after acquiring write lock (another task might have created one)
            for context in contexts.iter() {
                if !context
                    .metadata
                    .is_busy
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Ok(Some(context.clone()));
                }
            }
        }

        // Check if we're under the limit
        if contexts.len() < self.scope_config.max_contexts as usize {
            info!(
                "Creating new context on-demand ({}/{}){}",
                contexts.len() + 1,
                self.scope_config.max_contexts,
                if proxy_params.requires_dedicated_context() {
                    " [dedicated]"
                } else {
                    ""
                }
            );

            let context = self.create_new_context(proxy_params).await?;
            let context_arc = Arc::new(context);
            contexts.push(context_arc.clone());

            Ok(Some(context_arc))
        } else {
            // At maximum capacity
            info!(
                "Cannot create new context - at max capacity ({}/{})",
                contexts.len(),
                self.scope_config.max_contexts
            );
            Ok(None)
        }
    }

    /// Create a new context without reusing idle ones (AlwaysNew mode).
    ///
    /// This method is used in AlwaysNew session mode where each request
    /// without session_id should get a fresh context.
    ///
    /// # Parameters
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    ///
    /// Returns:
    /// - Ok(Some(context)) - New context created successfully
    /// - Ok(None) - Max contexts limit reached
    /// - Err - Failed to create context
    pub async fn create_always_new_context(
        &self,
        proxy_params: &ProxyParams,
    ) -> Result<Option<Arc<BrowserContext>>> {
        let mut contexts = self.contexts.write().await;

        // Reclaim leaked contexts before measuring capacity, so a leak cannot consume a slot
        // until the lifecycle monitor's next tick.
        let leaked = reclaim_leaked_always_new_contexts(&mut contexts);
        if !leaked.is_empty() {
            warn!(
                "Purged {} leaked idle context(s) in AlwaysNew mode before creating a new one",
                leaked.len()
            );
        }

        // Check if we're under the limit
        let result = if contexts.len() < self.scope_config.max_contexts as usize {
            info!(
                "Creating new context (AlwaysNew mode) ({}/{}) with proxy params: country_code={:?}",
                contexts.len() + 1,
                self.scope_config.max_contexts,
                proxy_params.country_code
            );

            // No `?` here: an early return would skip closing the purged tabs below.
            match self.create_new_context(proxy_params).await {
                Ok(context) => {
                    // Pre-mark as busy while still holding the write lock, so the context is
                    // never visible to the purge above as an idle (and therefore collectable)
                    // context. ContextBusyGuard adopts this flag instead of setting it.
                    context.metadata.is_busy.store(true, Ordering::SeqCst);

                    let context_arc = Arc::new(context);
                    contexts.push(context_arc.clone());

                    Ok(Some(context_arc))
                }
                Err(e) => Err(e),
            }
        } else {
            // At maximum capacity
            info!(
                "Cannot create new context (AlwaysNew mode) - at max capacity ({}/{})",
                contexts.len(),
                self.scope_config.max_contexts
            );
            Ok(None)
        };

        // Close the purged tabs only after the pool lock is released.
        drop(contexts);
        for context in &leaked {
            close_context_tab(context).await;
        }

        result
    }

    /// Remove a context from the pool by its ID.
    ///
    /// This is used in SessionMode::AlwaysNew to destroy the context after
    /// the request completes, freeing up the slot for the next request.
    ///
    /// The tab is closed in Chrome as well — removing the context from the pool `Vec` alone
    /// leaks it, see `close_tab_detached`. Idempotent: a second call finds neither the context
    /// nor a tab to close.
    pub async fn destroy_context(&self, context_id: &uuid::Uuid) {
        let removed = {
            let mut contexts = self.contexts.write().await;

            let mut removed = Vec::new();
            contexts.retain(|c| {
                if &c.metadata.id == context_id {
                    removed.push(c.clone());
                    false
                } else {
                    true
                }
            });

            if let Some(context) = removed.first() {
                let isolation_info = if context.cdp_context_id.is_some() {
                    " (isolated)"
                } else {
                    ""
                };
                info!(
                    "Destroyed context (AlwaysNew mode) {}{} ({} contexts remaining)",
                    context_id,
                    isolation_info,
                    contexts.len()
                );
            }

            removed
        };

        // Close the tab only after the pool lock is released.
        for context in &removed {
            close_context_tab(context).await;
        }
    }

    pub async fn get_stats(&self) -> BrowserPoolStats {
        let contexts = self.contexts.read().await;

        // Total slots is the configured max_contexts (potential capacity)
        // This is important for SessionMode::AlwaysNew and SessionMode::Reusable
        // where contexts are created on-demand
        let total_slots = self.scope_config.max_contexts as usize;

        let active_requests: usize = contexts
            .iter()
            .filter(|c| c.metadata.is_busy.load(std::sync::atomic::Ordering::SeqCst))
            .count();

        // Available slots = max capacity minus currently busy contexts
        let available_slots = total_slots.saturating_sub(active_requests);

        let total_requests: u64 = contexts
            .iter()
            .map(|c| c.metadata.total_requests.load(Ordering::SeqCst))
            .sum();

        BrowserPoolStats {
            total_contexts: contexts.len(),
            total_slots,
            available_slots,
            active_requests,
            total_requests,
            total_contexts_created: self.total_contexts_created.load(Ordering::SeqCst),
            total_contexts_recycled: self.total_contexts_recycled.load(Ordering::SeqCst),
        }
    }

    pub fn get_browser(&self) -> Arc<Browser> {
        self.browser.clone()
    }

    pub fn get_proxy_config(&self) -> &ProxyConfig {
        &self.proxy_config
    }

    pub fn supports_per_context_proxy(&self) -> bool {
        self.proxy_provider.supports_per_context_proxy()
    }

    pub fn get_proxy_provider_name(&self) -> &str {
        self.proxy_provider.name()
    }

    /// Create a new tab in an existing CDP BrowserContext.
    ///
    /// This is used to recreate a tab after it dies (e.g., after hard timeout
    /// closes it) while preserving the CDP context's session state (cookies, storage).
    ///
    /// # Parameters
    /// * `cdp_context_id` - The CDP BrowserContext ID to create the tab in
    ///
    /// # Returns
    /// * `Ok(Arc<Tab>)` - The newly created tab
    /// * `Err` - Failed to create tab (context may be invalid)
    pub fn create_tab_in_context(&self, cdp_context_id: &str) -> Result<Arc<Tab>> {
        info!("Recreating tab in existing CDP context: {}", cdp_context_id);

        let create_target = CreateTarget {
            url: "about:blank".to_string(),
            width: None,
            height: None,
            browser_context_id: Some(cdp_context_id.into()),
            enable_begin_frame_control: None,
            new_window: None,
            background: None,
            for_tab: None,
        };

        let new_tab = self
            .browser
            .new_tab_with_options(create_target)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to create tab in CDP context {}: {}",
                    cdp_context_id,
                    e
                )
            })?;

        // Apply tab init middlewares to the recreated tab
        for middleware in &self.tab_init_middlewares {
            if let Err(e) = middleware.apply(&new_tab) {
                warn!(
                    "Failed to apply tab init middleware '{}' to recreated tab: {}",
                    middleware.name(),
                    e
                );
            }
        }

        info!(
            "Successfully recreated tab in CDP context: {}",
            cdp_context_id
        );

        Ok(new_tab)
    }

    /// Create a new tab in shared (default) browser context.
    ///
    /// This is used to recreate a tab when the previous one died in shared mode.
    ///
    /// # Parameters
    ///
    /// # Returns
    /// * `Ok(Arc<Tab>)` - The newly created tab
    /// * `Err` - Failed to create tab
    pub fn create_tab_shared(&self) -> Result<Arc<Tab>> {
        info!("Recreating tab in shared browser context");

        let new_tab = self
            .browser
            .new_tab()
            .map_err(|e| anyhow::anyhow!("Failed to create tab in shared context: {}", e))?;

        // Apply tab init middlewares to the recreated tab
        for middleware in &self.tab_init_middlewares {
            if let Err(e) = middleware.apply(&new_tab) {
                warn!(
                    "Failed to apply tab init middleware '{}' to recreated tab: {}",
                    middleware.name(),
                    e
                );
            }
        }

        info!("Successfully recreated tab in shared context");

        Ok(new_tab)
    }
}

#[derive(Debug, Clone)]
pub struct BrowserPoolStats {
    pub total_contexts: usize,
    pub total_slots: usize,
    pub available_slots: usize,
    pub active_requests: usize,
    pub total_requests: u64,
    pub total_contexts_created: u64,
    pub total_contexts_recycled: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a context without touching a browser: an AlwaysNew pool slot is fully described
    /// by its metadata, so `tab`/`cdp_context_id` may stay empty for reclamation tests.
    fn make_context(busy: bool) -> Arc<BrowserContext> {
        let metadata = BrowserContextMetadata::new();
        metadata.is_busy.store(busy, Ordering::SeqCst);
        Arc::new(BrowserContext {
            metadata,
            tab: Arc::new(Mutex::new(None)),
            cdp_context_id: None,
        })
    }

    #[test]
    fn reclaims_idle_contexts_and_keeps_busy_ones() {
        let busy = make_context(true);
        let mut contexts = vec![make_context(false), busy.clone(), make_context(false)];

        let removed = reclaim_leaked_always_new_contexts(&mut contexts);

        assert_eq!(removed.len(), 2);
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].metadata.id, busy.metadata.id);
    }

    #[test]
    fn reclaims_nothing_while_all_contexts_are_busy() {
        let mut contexts = vec![make_context(true), make_context(true)];

        assert!(reclaim_leaked_always_new_contexts(&mut contexts).is_empty());
        assert_eq!(contexts.len(), 2);
    }

    /// The race that pre-marking at creation exists to prevent: a context is in the pool and
    /// handed to a request that has not started processing yet. It must survive a concurrent
    /// request's reclamation pass, otherwise reclamation would destroy live work.
    #[test]
    fn keeps_context_handed_out_but_not_yet_processing() {
        let just_created = make_context(true); // pre-marked busy under the pool write lock
        let mut contexts = vec![just_created.clone()];

        assert!(reclaim_leaked_always_new_contexts(&mut contexts).is_empty());
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].metadata.id, just_created.metadata.id);
    }

    /// Regression: a request whose future was dropped mid-flight leaves its context idle in
    /// the pool. Before reclamation this permanently consumed a slot, and the lifecycle
    /// monitor made it worse by replacing the leaked context with a fresh one.
    #[test]
    fn frees_the_slot_after_a_request_leaks_its_context() {
        let leaked = make_context(true);
        let mut contexts = vec![leaked.clone()];
        let max_contexts = 1;

        // Request scope ends: the busy guard clears the flag, but removal never ran.
        leaked.metadata.is_busy.store(false, Ordering::SeqCst);
        assert!(
            contexts.len() >= max_contexts,
            "slot is occupied by the leak"
        );

        let removed = reclaim_leaked_always_new_contexts(&mut contexts);

        assert_eq!(removed.len(), 1);
        assert!(contexts.len() < max_contexts, "slot is available again");
    }
}
