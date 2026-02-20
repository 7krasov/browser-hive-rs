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
                 headless_chrome will attempt auto-detection.",
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
            let context = self.create_new_context("startup", &default_params).await?;
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

    async fn create_new_context(
        &self,
        ray_id: &str,
        proxy_params: &ProxyParams,
    ) -> Result<BrowserContext> {
        let start_time = std::time::Instant::now();

        let mut metadata = BrowserContextMetadata::new();

        // Assign per-context proxy if provider supports it
        if self.proxy_provider.supports_per_context_proxy() {
            if let Some(context_proxy) = self
                .proxy_provider
                .get_context_proxy_with_params(&metadata.id.to_string(), proxy_params)
            {
                info!(
                    ray_id = %ray_id,
                    "Assigning context-specific proxy to context {} (country_code: {:?})",
                    metadata.id,
                    proxy_params.country_code
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

                info!(
                    ray_id = %ray_id,
                    "Created ISOLATED CDP context {} with tab",
                    context_id
                );

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
                    ray_id = %ray_id,
                    "Failed to apply tab init middleware '{}': {}",
                    middleware.name(),
                    e
                );
            } else {
                tracing::debug!(
                    ray_id = %ray_id,
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
            ray_id = %ray_id,
            "Created browser context ({}) for metadata id: {} in {}ms",
            isolation_mode,
            metadata.id,
            creation_time_ms
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

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;

                let mut contexts_guard = contexts.write().await;
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

                        // Close tab if it exists
                        // Note: For isolated contexts, Chrome will automatically clean up
                        // the CDP BrowserContext when all tabs in it are closed
                        if let Some(_tab) = context.tab.lock().await.take() {
                            // Tab will be dropped and closed automatically
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
    /// * `ray_id` - Request tracing ID for logging
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    pub async fn get_or_create_context(
        &self,
        ray_id: &str,
        proxy_params: &ProxyParams,
    ) -> Result<Option<Arc<BrowserContext>>> {
        // If request has proxy routing overrides (e.g. country_code), we must create
        // a dedicated context because these params affect the proxy connection identity
        // (exit IP, geo) and can't be changed on an existing context.
        if !proxy_params.requires_dedicated_context() {
            // No routing overrides - try to reuse an idle context
            if let Some(context) = self.find_least_busy_context().await {
                info!(ray_id = %ray_id, "Reusing idle context: {} (total_requests: {})", context.metadata.id, context.metadata.total_requests.load(std::sync::atomic::Ordering::SeqCst));
                return Ok(Some(context));
            }
        } else {
            debug!(
                ray_id = %ray_id,
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
                ray_id = %ray_id,
                "Creating new context on-demand ({}/{}){}",
                contexts.len() + 1,
                self.scope_config.max_contexts,
                if proxy_params.requires_dedicated_context() { " [dedicated]" } else { "" }
            );

            let context = self.create_new_context(ray_id, proxy_params).await?;
            let context_arc = Arc::new(context);
            contexts.push(context_arc.clone());

            Ok(Some(context_arc))
        } else {
            // At maximum capacity
            info!(
                ray_id = %ray_id,
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
    /// * `ray_id` - Request tracing ID for logging
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    ///
    /// Returns:
    /// - Ok(Some(context)) - New context created successfully
    /// - Ok(None) - Max contexts limit reached
    /// - Err - Failed to create context
    pub async fn create_always_new_context(
        &self,
        ray_id: &str,
        proxy_params: &ProxyParams,
    ) -> Result<Option<Arc<BrowserContext>>> {
        let mut contexts = self.contexts.write().await;

        // Check if we're under the limit
        if contexts.len() < self.scope_config.max_contexts as usize {
            info!(
                ray_id = %ray_id,
                "Creating new context (AlwaysNew mode) ({}/{}) with proxy params: country_code={:?}",
                contexts.len() + 1,
                self.scope_config.max_contexts,
                proxy_params.country_code
            );

            let context = self.create_new_context(ray_id, proxy_params).await?;
            let context_arc = Arc::new(context);
            contexts.push(context_arc.clone());

            Ok(Some(context_arc))
        } else {
            // At maximum capacity
            info!(
                ray_id = %ray_id,
                "Cannot create new context (AlwaysNew mode) - at max capacity ({}/{})",
                contexts.len(),
                self.scope_config.max_contexts
            );
            Ok(None)
        }
    }

    /// Remove a context from the pool by its ID.
    ///
    /// This is used in SessionMode::AlwaysNew to destroy the context after
    /// the request completes, freeing up the slot for the next request.
    ///
    /// Note: For isolated contexts, Chrome will automatically clean up the
    /// CDP BrowserContext when all tabs in it are closed (which happens when
    /// the BrowserContext is dropped).
    pub async fn destroy_context(&self, context_id: &uuid::Uuid, ray_id: &str) {
        let mut contexts = self.contexts.write().await;
        let initial_len = contexts.len();

        // Find if context was isolated (for logging)
        let was_isolated = contexts
            .iter()
            .find(|c| &c.metadata.id == context_id)
            .map(|c| c.cdp_context_id.is_some())
            .unwrap_or(false);

        contexts.retain(|c| &c.metadata.id != context_id);

        if contexts.len() < initial_len {
            let isolation_info = if was_isolated { " (isolated)" } else { "" };
            info!(
                ray_id = %ray_id,
                "Destroyed context (AlwaysNew mode) {}{} ({} contexts remaining)",
                context_id,
                isolation_info,
                contexts.len()
            );
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
    /// * `ray_id` - Request tracing ID for logging
    ///
    /// # Returns
    /// * `Ok(Arc<Tab>)` - The newly created tab
    /// * `Err` - Failed to create tab (context may be invalid)
    pub fn create_tab_in_context(&self, cdp_context_id: &str, ray_id: &str) -> Result<Arc<Tab>> {
        info!(
            ray_id = %ray_id,
            "Recreating tab in existing CDP context: {}",
            cdp_context_id
        );

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
                    ray_id = %ray_id,
                    "Failed to apply tab init middleware '{}' to recreated tab: {}",
                    middleware.name(),
                    e
                );
            }
        }

        info!(
            ray_id = %ray_id,
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
    /// * `ray_id` - Request tracing ID for logging
    ///
    /// # Returns
    /// * `Ok(Arc<Tab>)` - The newly created tab
    /// * `Err` - Failed to create tab
    pub fn create_tab_shared(&self, ray_id: &str) -> Result<Arc<Tab>> {
        info!(
            ray_id = %ray_id,
            "Recreating tab in shared browser context"
        );

        let new_tab = self
            .browser
            .new_tab()
            .map_err(|e| anyhow::anyhow!("Failed to create tab in shared context: {}", e))?;

        // Apply tab init middlewares to the recreated tab
        for middleware in &self.tab_init_middlewares {
            if let Err(e) = middleware.apply(&new_tab) {
                warn!(
                    ray_id = %ray_id,
                    "Failed to apply tab init middleware '{}' to recreated tab: {}",
                    middleware.name(),
                    e
                );
            }
        }

        info!(
            ray_id = %ray_id,
            "Successfully recreated tab in shared context"
        );

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
