use crate::browser_cdp::BrowserCdpClient;
use anyhow::{bail, Result};
use browser_hive_common::{
    BrowserContextMetadata, ContextIsolation, ContextLifecycleConfig, ProxyConfig, ProxyParams,
    ProxyProvider, RotationStrategy, ScopeConfig, SessionMode, TabInitMiddleware,
};
use headless_chrome::browser::context::Context as CdpContext;
use headless_chrome::browser::tab::Tab;
use headless_chrome::protocol::cdp::Target::CreateTarget;
use headless_chrome::{Browser, LaunchOptions};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn, Instrument};

pub struct BrowserPool {
    browser: Arc<Browser>,
    contexts: Arc<RwLock<Vec<Arc<BrowserContext>>>>,
    scope_config: ScopeConfig,
    lifecycle_config: ContextLifecycleConfig,
    proxy_config: ProxyConfig,
    proxy_provider: Box<dyn ProxyProvider>, // For per-context proxy assignment

    /// Browser-level CDP client, present only when the provider assigns proxy hosts per
    /// context. Every other provider keeps using `Browser::new_context()`, so no second
    /// socket is opened for them.
    ///
    /// Shared with the lifecycle monitor, which recycles contexts and must route them the
    /// same way the request path does.
    cdp_client: Option<Arc<BrowserCdpClient>>,

    /// Latches the one-time warning about a provider handing out hosts that do not route.
    proxy_host_mismatch_warned: AtomicBool,

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
/// Choose the idle context with the lowest request count, or `None` if all are busy.
///
/// Free function rather than a method so both lookups — the optimistic one under the read lock
/// and the double-check under the write lock — select identically. Ties keep the earlier
/// context, which only matters for a pool that has just started and where every count is 0.
///
/// `origin` is the registrable domain the request is for. A context this origin has recently
/// refused (403/429) is skipped **for that origin only** — it stays fully eligible for every
/// other site, because a block belongs to the pair (exit IP, origin), not to the context. Pass
/// `None` to ignore quarantines (the caller has no origin, or the feature is off).
fn select_least_used_idle(
    contexts: &[Arc<BrowserContext>],
    origin: Option<&str>,
) -> Option<Arc<BrowserContext>> {
    contexts
        .iter()
        .filter(|c| !c.metadata.is_busy.load(Ordering::SeqCst))
        .filter(|c| match origin {
            Some(origin) => c.metadata.quarantined_until(origin).is_none(),
            None => true,
        })
        .min_by_key(|c| c.metadata.total_requests.load(Ordering::SeqCst))
        .cloned()
}

/// Of the idle contexts quarantined for `origin`, the one whose quarantine ends soonest.
///
/// The last resort when the pool is at `max_contexts` and every idle context is quarantined for
/// this origin. Serving the request from a refused context is a poor outcome, but it is the
/// outcome the caller already gets today, whereas failing the request would be a new one — the
/// client is answered with the origin's own status either way and decides what it means.
fn select_soonest_unquarantined(
    contexts: &[Arc<BrowserContext>],
    origin: &str,
) -> Option<(Arc<BrowserContext>, Instant)> {
    contexts
        .iter()
        .filter(|c| !c.metadata.is_busy.load(Ordering::SeqCst))
        .filter_map(|c| c.metadata.quarantined_until(origin).map(|until| (c, until)))
        .min_by_key(|(_, until)| *until)
        .map(|(c, until)| (c.clone(), until))
}

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
    /// Proxy host this context's traffic actually leaves through, credentials stripped.
    ///
    /// Not the same thing as `metadata.assigned_proxy_config`: that config is read for its
    /// credentials, and its host only routes anything when the provider assigns hosts per
    /// context. This field records the host that ends up carrying the traffic either way, so a
    /// log line can name it without the reader having to know which of the two applies.
    pub proxy_host: Option<String>,
}

impl BrowserPool {
    pub async fn new(scope_config: ScopeConfig) -> Result<Self> {
        info!("Launching Chrome process for scope: {}", scope_config.name);

        let per_context_proxy_host = scope_config.proxy_provider.assigns_proxy_host_per_context();

        // Refuse the combinations that cannot deliver what the provider promises. Both fail at
        // startup rather than warn: downgraded to a warning either would be lost in the first
        // minute of a pod's logs, while the pool went on serving a single address under a log
        // line announcing rotation.
        if per_context_proxy_host {
            // Shared isolation runs every request in the browser's default context, which is
            // pinned to the launch proxy for the process's lifetime.
            if matches!(scope_config.context_isolation, ContextIsolation::Shared) {
                bail!(
                    "Proxy provider '{}' assigns a proxy host per context, which requires \
                     WORKER_CONTEXT_ISOLATION=isolated. In shared isolation every request runs in \
                     the browser's default context and would silently use a single host from the \
                     pool.",
                    scope_config.proxy_provider.name()
                );
            }

            // The per-context host is read off the config `get_context_proxy` returns, and that
            // call only happens when `supports_per_context_proxy()` is true. Without it there is
            // no per-context config to take a host from, so every context would fall back to the
            // launch proxy - the very bug this flag exists to remove, and invisible, since the
            // flag alone is enough to log that per-context routing is on.
            if !scope_config.proxy_provider.supports_per_context_proxy() {
                bail!(
                    "Proxy provider '{}' returns assigns_proxy_host_per_context() = true but \
                     supports_per_context_proxy() = false. The per-context host comes from \
                     get_context_proxy(), which is only called when the latter is true, so this \
                     combination would route every context through the browser-wide proxy. \
                     Override both.",
                    scope_config.proxy_provider.name()
                );
            }
        }

        // Build proxy configuration
        let proxy_config = scope_config.proxy_provider.build_config()?;

        // Get proxy server URL (None if no proxy).
        //
        // An empty string is normalised to None here rather than passed on: Chrome reads
        // `--proxy-server=` as "no proxy" and scrapes directly, which is the one outcome this
        // whole block exists to make impossible by accident.
        let proxy_server = proxy_config
            .build_proxy_server()
            .map(|server| server.trim().to_string())
            .filter(|server| !server.is_empty());

        // Refuse to start without a proxy unless the provider asks for a direct connection.
        //
        // This is the only remaining way a scrape can leave through the pod's own IP: every
        // in-request fallback (a tab created in the default context, a slot whose recycling
        // failed, a recreated tab after a dead CDP session) lands on this launch proxy, so if it
        // exists, no request can escape it. If it does not, they all go out directly - and
        // nothing downstream fails, which is why this has to fail here.
        if proxy_server.is_none() && !scope_config.proxy_provider.allows_direct_connection() {
            bail!(
                "Proxy provider '{}' produced no proxy server, so every request would be made \
                 from this pod's own public IP. Check the provider's configuration (an \
                 unparseable port, an empty URL and an empty pool all end up here). If a direct \
                 connection is intended, override ProxyProvider::allows_direct_connection() to \
                 return true.",
                scope_config.proxy_provider.name()
            );
        }

        // Log proxy configuration. Which of the two routing modes is in force is worth a line of
        // its own: "one proxy for the whole browser" is a legitimate mode, but it is also what a
        // provider gets by silently not overriding `assigns_proxy_host_per_context`, and the
        // difference is otherwise only visible by correlating `span_proxy_host` across requests.
        match &proxy_server {
            Some(server) if per_context_proxy_host => {
                info!(
                    "Proxy: per-context hosts from provider '{}' - {} carries the browser's \
                     default context only",
                    scope_config.proxy_provider.name(),
                    server
                );
            }
            Some(server) => {
                let credentials = if proxy_config.get_credentials().is_some() {
                    "with authentication via Fetch API"
                } else {
                    "no authentication"
                };
                let per_context = if scope_config.proxy_provider.supports_per_context_proxy() {
                    ", credentials vary per context"
                } else {
                    ""
                };
                info!(
                    "Proxy: {} carries every context of this browser ({}{})",
                    server, credentials, per_context
                );
            }
            None => {
                info!(
                    "No proxy configured - using direct connection (provider '{}' asked for it)",
                    scope_config.proxy_provider.name()
                );
            }
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

        // Only providers that route per context need the second CDP client, so no other scope
        // pays for the extra socket. Connecting now turns a broken endpoint into a startup
        // failure instead of into a failure of the first scrape request.
        let cdp_client = if per_context_proxy_host {
            let client = BrowserCdpClient::connect(browser.get_ws_url())?;
            info!(
                "Per-context proxy hosts enabled for provider '{}' - each context gets its own \
                 proxy from the pool",
                scope_config.proxy_provider.name()
            );
            Some(Arc::new(client))
        } else {
            None
        };

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
            cdp_client,
            proxy_host_mismatch_warned: AtomicBool::new(false),
            tab_init_middlewares,
            total_contexts_created: Arc::new(AtomicU64::new(0)),
            total_contexts_recycled: Arc::new(AtomicU64::new(0)),
        };

        // Log session mode. What `max_contexts` counts differs per mode, so the line says it
        // rather than printing a number whose meaning the reader has to look up.
        match scope_config.session_mode {
            SessionMode::AlwaysNew => {
                info!(
                    "Session mode: ALWAYS_NEW (fresh context per request, destroyed after) - \
                     up to {} concurrent requests",
                    scope_config.max_contexts
                );
            }
            SessionMode::Reusable => {
                info!(
                    "Session mode: REUSABLE (anonymous pool, contexts reused by any request until \
                     recycled) - up to {} concurrent requests",
                    scope_config.max_contexts
                );
            }
            SessionMode::Dedicated => {
                info!(
                    "Session mode: DEDICATED (one context per session, removed after {:?} idle) - \
                     up to {} concurrent SESSIONS, not requests per second",
                    scope_config.lifecycle.max_idle_time, scope_config.max_contexts
                );
            }
        }

        // Pre-initialization is an option, not a mode. It only helps where a pre-created context
        // can serve an arbitrary request: in the other modes a context with no client is either
        // never handed out (dedicated) or never reused (always_new), so it would occupy a slot
        // until the lifecycle monitor removed it again. `ScopeConfig::validate` warns at startup.
        if scope_config.min_contexts > 0 && scope_config.session_mode == SessionMode::Reusable {
            info!(
                "Pre-initializing {} contexts on startup (max: {})",
                scope_config.min_contexts, scope_config.max_contexts
            );
            pool.populate_initial_contexts().await?;
        } else {
            info!(
                "Starting with 0 contexts - will create on-demand up to {}",
                scope_config.max_contexts
            );
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

        let context_proxy_host = metadata
            .assigned_proxy_config
            .as_ref()
            .and_then(ProxyConfig::build_proxy_server);
        let launch_proxy_host = self.proxy_config.build_proxy_server();
        self.warn_if_assigned_host_does_not_route(&context_proxy_host, &launch_proxy_host);

        // The host that will actually carry this context's traffic. Without per-context
        // routing the assigned config contributes credentials only, so the launch proxy is the
        // honest answer - and the one worth logging.
        let proxy_host = if self.cdp_client.is_some() {
            context_proxy_host
                .clone()
                .or_else(|| launch_proxy_host.clone())
        } else {
            launch_proxy_host.clone()
        };

        // Create tab based on isolation mode
        let (new_tab, cdp_context_id) = match self.scope_config.context_isolation {
            ContextIsolation::Isolated => {
                // Create isolated CDP BrowserContext (like incognito - separate cookies/storage)
                let context_id = match (&self.cdp_client, &context_proxy_host) {
                    // Providers whose pool is a list of hosts: the context is created with its
                    // own proxy, because `Browser::new_context()` cannot carry one.
                    //
                    // No fallback to the plain path on failure. Falling back would put the
                    // request on the launch proxy while the logs claimed rotation - the exact
                    // silent single-host behaviour this exists to remove.
                    (Some(client), Some(host)) => {
                        client.create_browser_context(host).map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to create isolated CDP context with its own proxy: {}",
                                e
                            )
                        })?
                    }
                    _ => self
                        .browser
                        .new_context()
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to create isolated CDP context: {}", e)
                        })?
                        .get_id()
                        .to_string(),
                };

                // Tab creation, Fetch auth and navigation all stay on headless_chrome's own
                // transport - the context id is browser state, so the crate can adopt it.
                let cdp_context = CdpContext::new(&self.browser, context_id.clone());
                let tab = cdp_context.new_tab().map_err(|e| {
                    anyhow::anyhow!("Failed to create tab in isolated context: {}", e)
                })?;

                info!(
                    "Created ISOLATED CDP context {} with tab (proxy: {})",
                    context_id,
                    proxy_host.as_deref().unwrap_or("direct connection")
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
            proxy_host,
        })
    }

    /// Warn once when a provider hands out a proxy host that does not route anything.
    ///
    /// `supports_per_context_proxy` delivers *credentials* per context; the host of that config
    /// is discarded unless the provider also opts into per-context routing. A provider whose
    /// pool is a list of distinct hosts therefore looks like it is rotating while every request
    /// leaves through the single host picked at browser launch - and, worse, authenticates that
    /// host with credentials taken from a different pool entry. That works only as long as every
    /// entry shares one username and password; the day they differ it becomes a fleet-wide 407
    /// with nothing in the logs pointing at the cause.
    ///
    /// Once per pool: this is a static property of the provider, so repeating it per context
    /// would only add noise to a log that already carries the effective host.
    fn warn_if_assigned_host_does_not_route(
        &self,
        context_proxy_host: &Option<String>,
        launch_proxy_host: &Option<String>,
    ) {
        if self.cdp_client.is_some() {
            return;
        }
        let Some(assigned) = context_proxy_host else {
            return;
        };
        if Some(assigned) == launch_proxy_host.as_ref() {
            return;
        }
        if self.proxy_host_mismatch_warned.swap(true, Ordering::SeqCst) {
            return;
        }

        warn!(
            "Proxy provider '{}' assigns host {} per context, but all traffic leaves through the \
             browser-wide proxy {} - only the credentials of the per-context config are used, so \
             the pool is NOT being rotated. If this provider's exits are distinct hosts, override \
             ProxyProvider::assigns_proxy_host_per_context() to return true; if its exits are \
             selected through the username (gateway providers), make build_config() and \
             get_context_proxy() agree on the host.",
            self.proxy_provider.name(),
            assigned,
            launch_proxy_host.as_deref().unwrap_or("<none>"),
        );
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
        let cdp_client = self.cdp_client.clone();
        let launch_proxy_host = self.proxy_config.build_proxy_server();

        // Standalone background task: the request span never reaches it, so it opens its own.
        // `scope` matches the request span's field, and `ray_id` keeps the sentinel value these
        // lines have always carried — both now live under the `span` object (Loki: span_scope,
        // span_ray_id) instead of being repeated as an event field on every call site.
        let span = tracing::info_span!(
            "lifecycle_monitor",
            scope = %self.scope_config.name,
            ray_id = "lifecycle-monitor",
        );

        let monitor = async move {
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

                // Dedicated: a context belongs to one session and nothing else can free its
                // slot — there is no release RPC, and a session-less request never takes an
                // existing context. So an expired context is *removed*, not recycled: replacing
                // it would put a fresh context nobody owns in the same slot, and the pool would
                // report itself full with no session in sight.
                //
                // Expiry is the ordinary lifecycle predicate, so a session also ends when it
                // outlives max_lifetime or exhausts max_requests. `max_idle_time` is the one
                // that matters in practice, and `validate()` guarantees it is consulted.
                if session_mode == SessionMode::Dedicated {
                    let mut expired = Vec::new();
                    let mut still_alive = Vec::with_capacity(contexts_guard.len());
                    for context in contexts_guard.drain(..) {
                        let idle = !context.metadata.is_busy.load(Ordering::SeqCst);
                        if idle && Self::should_recycle_context(&context, &lifecycle_config).await {
                            expired.push(context);
                        } else {
                            still_alive.push(context);
                        }
                    }
                    *contexts_guard = still_alive;

                    if !expired.is_empty() {
                        info!(
                            "Released {} idle dedicated session context(s) ({} still claimed)",
                            expired.len(),
                            contexts_guard.len()
                        );
                        for context in &expired {
                            info!(
                                "Released dedicated context {} (age: {:?}, requests: {})",
                                context.metadata.id,
                                context.metadata.created_at.elapsed(),
                                context.metadata.total_requests.load(Ordering::SeqCst)
                            );
                        }
                    }

                    drop(contexts_guard);
                    for context in &expired {
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
                                    "Assigning context-specific proxy to recycled context {}",
                                    metadata.id
                                );
                                metadata.assigned_proxy_config = Some(context_proxy);
                            }
                        }

                        let context_proxy_host = metadata
                            .assigned_proxy_config
                            .as_ref()
                            .and_then(ProxyConfig::build_proxy_server);
                        let proxy_host = if cdp_client.is_some() {
                            context_proxy_host
                                .clone()
                                .or_else(|| launch_proxy_host.clone())
                        } else {
                            launch_proxy_host.clone()
                        };

                        // Create tab based on isolation mode
                        let (tab, cdp_context_id) = match context_isolation {
                            ContextIsolation::Isolated => {
                                // Recycling must route the new context exactly like the request
                                // path does. Creating it with the plain call for a provider that
                                // routes per context would drop it onto the launch proxy, so the
                                // pool would stop rotating after the first lifecycle tick.
                                let created = match (&cdp_client, &context_proxy_host) {
                                    (Some(client), Some(host)) => client
                                        .create_browser_context(host)
                                        .map_err(|e| e.to_string()),
                                    _ => browser
                                        .new_context()
                                        .map(|cdp_context| cdp_context.get_id().to_string())
                                        .map_err(|e| e.to_string()),
                                };

                                match created {
                                    Ok(ctx_id) => {
                                        let cdp_context = CdpContext::new(&browser, ctx_id.clone());
                                        match cdp_context.new_tab() {
                                            Ok(new_tab) => {
                                                // Apply tab init middlewares
                                                for middleware in &tab_init_middlewares {
                                                    if let Err(e) = middleware.apply(&new_tab) {
                                                        tracing::warn!(
                                                            "Failed to apply tab init middleware '{}': {}",
                                                            middleware.name(),
                                                            e
                                                        );
                                                    }
                                                }
                                                (Some(new_tab), Some(ctx_id))
                                            }
                                            Err(e) => {
                                                // Keep the context id: the CDP BrowserContext was
                                                // created and carries this slot's proxy, only the
                                                // tab failed. Dropping the id here would send the
                                                // next request's lazy tab creation into the
                                                // browser's default context - no isolation, and
                                                // the launch proxy instead of the assigned host.
                                                tracing::warn!(
                                                    "Failed to create tab in isolated context {} - \
                                                     keeping the context, the tab is created lazily \
                                                     on the next request: {}",
                                                    ctx_id,
                                                    e
                                                );
                                                (None, Some(ctx_id))
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
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
                                            "Successfully created tab during context recycling"
                                        );

                                        // Apply tab init middlewares to customize the tab
                                        for middleware in &tab_init_middlewares {
                                            if let Err(e) = middleware.apply(&new_tab) {
                                                tracing::warn!(
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
                            proxy_host,
                        };

                        contexts_guard[*idx] = Arc::new(new_context);
                        total_recycled.fetch_add(1, Ordering::SeqCst);
                        total_created.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        };

        tokio::spawn(monitor.instrument(span));
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

    /// Pick the idle context that has served the fewest requests.
    ///
    /// "Least busy" used to mean "the first one that is not busy", which is a different thing:
    /// at moderate load the earliest context in the vector took a disproportionate share of the
    /// traffic while the later ones idled. With per-context proxy routing that concentrated most
    /// requests on the first exit IP — the opposite of why a pool of addresses is bought.
    pub async fn find_least_busy_context(&self) -> Option<Arc<BrowserContext>> {
        let contexts = self.contexts.read().await;
        select_least_used_idle(&contexts, None)
    }

    /// Get or create a new context on-demand.
    ///
    /// This method is used in on-demand mode to create contexts when needed.
    /// It will:
    /// 1. Try to find an idle existing context that this origin has not just refused
    /// 2. If none found and under max_contexts limit, create a new one
    /// 3. If at max_contexts limit, fall back to a quarantined idle context, or return None
    ///    (resource exhausted) when every context is busy
    ///
    /// # Parameters
    /// * `proxy_params` - Proxy parameters (country_code, etc.) for context creation
    /// * `origin` - Registrable domain of the request, or `None` when quarantines do not apply
    ///   (feature disabled, or the URL has no domain). Step 1 skips contexts quarantined for it,
    ///   which is what turns step 2 into "grow the pool because the current exit IP is refused"
    ///   rather than only "grow the pool because everything is busy".
    pub async fn get_or_create_context(
        &self,
        proxy_params: &ProxyParams,
        origin: Option<&str>,
    ) -> Result<Option<Arc<BrowserContext>>> {
        // If request has proxy routing overrides (e.g. country_code), we must create
        // a dedicated context because these params affect the proxy connection identity
        // (exit IP, geo) and can't be changed on an existing context.
        if !proxy_params.requires_dedicated_context() {
            // No routing overrides - try to reuse an idle context this origin has not refused
            let reusable = {
                let contexts = self.contexts.read().await;
                select_least_used_idle(&contexts, origin)
            };
            if let Some(context) = reusable {
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
            if let Some(context) = select_least_used_idle(&contexts, origin) {
                return Ok(Some(context));
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

            return Ok(Some(context_arc));
        }

        // At maximum capacity. If the only reason nothing was selected is that every idle context
        // is quarantined for this origin, serve the request from the one that recovers soonest:
        // the pool has nothing better to offer, and the alternative — refusing the request — would
        // report a capacity problem the scope does not have.
        if let Some(origin) = origin.filter(|_| !proxy_params.requires_dedicated_context()) {
            if let Some((context, until)) = select_soonest_unquarantined(&contexts, origin) {
                warn!(
                    "All {} contexts are quarantined for {} - reusing context {} anyway \
                     (quarantine ends in {:?}); the pool is at max_contexts, so no fresh exit IP \
                     is available",
                    contexts.len(),
                    origin,
                    context.metadata.id,
                    until.saturating_duration_since(Instant::now())
                );
                return Ok(Some(context));
            }
        }

        info!(
            "Cannot create new context - at max capacity ({}/{})",
            contexts.len(),
            self.scope_config.max_contexts
        );
        Ok(None)
    }

    /// Create a context that will belong to one session (Dedicated mode).
    ///
    /// Never reuses an existing context: in `Dedicated` every context in the pool is already
    /// claimed by a client, and handing one to a request that arrived without its session id is
    /// exactly the confusion this mode exists to remove. A session reaches its own context
    /// through `find_context_by_id`, not through here.
    ///
    /// The capacity check is therefore a check on **concurrent sessions**. `Ok(None)` means every
    /// session slot is taken — including by sessions that are merely idle between requests, which
    /// is why the idle timeout has to be short.
    pub async fn create_dedicated_context(
        &self,
        proxy_params: &ProxyParams,
    ) -> Result<Option<Arc<BrowserContext>>> {
        let mut contexts = self.contexts.write().await;

        if contexts.len() >= self.scope_config.max_contexts as usize {
            info!(
                "Cannot start a new session (Dedicated mode) - all {} session slots are claimed",
                self.scope_config.max_contexts
            );
            return Ok(None);
        }

        info!(
            "Starting a new session context (Dedicated mode) ({}/{}) with proxy params: country_code={:?}",
            contexts.len() + 1,
            self.scope_config.max_contexts,
            proxy_params.country_code
        );

        let context = Arc::new(self.create_new_context(proxy_params).await?);
        contexts.push(context.clone());

        Ok(Some(context))
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
    /// the request completes, freeing up the slot for the next request — and in any mode to drop
    /// a slot that can no longer be served correctly (an isolated context whose CDP context is
    /// gone), so the next request builds a fresh one instead of meeting the same broken slot.
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
                    "Destroyed context {}{} ({} contexts remaining)",
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

        // Slots that cannot be given to a new client.
        //
        // In `Dedicated` every context in the pool belongs to a session, so it is claimed whether
        // or not a request is running in it right now — a client between two requests still owns
        // its slot. Everywhere else nothing is held between requests, so claimed is exactly the
        // busy count. Defined this way the number is a superset of `active_requests` in every
        // mode, which is what makes it usable as a single autoscaling signal.
        let claimed_contexts = if self.scope_config.session_mode == SessionMode::Dedicated {
            contexts.len().max(active_requests)
        } else {
            active_requests
        };

        // Available slots = capacity minus what is claimed. Using the busy count here would let
        // the coordinator route a new session to a `Dedicated` worker whose every context is
        // already spoken for, and the request would come back as "all session slots claimed".
        let available_slots = total_slots.saturating_sub(claimed_contexts);

        let total_requests: u64 = contexts
            .iter()
            .map(|c| c.metadata.total_requests.load(Ordering::SeqCst))
            .sum();

        BrowserPoolStats {
            total_contexts: contexts.len(),
            total_slots,
            available_slots,
            active_requests,
            claimed_contexts,
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

    /// Whether each pool context owns a CDP BrowserContext rather than sharing the default one.
    ///
    /// Callers that create a tab need this to tell "no tab yet" from "no tab and nowhere correct
    /// to put one": under isolation, cookies, storage and (for providers that route per context)
    /// the proxy all live on the CDP context, so a tab in the default context silently has none
    /// of them.
    pub fn uses_isolated_contexts(&self) -> bool {
        matches!(
            self.scope_config.context_isolation,
            ContextIsolation::Isolated
        )
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
    /// Slots that cannot be given to a new client: busy contexts everywhere, plus the idle-but-
    /// owned contexts of `Dedicated` sessions. Always ≥ `active_requests`.
    pub claimed_contexts: usize,
    pub total_requests: u64,
    pub total_contexts_created: u64,
    pub total_contexts_recycled: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a context without touching a browser: an AlwaysNew pool slot is fully described
    /// by its metadata, so `tab`/`cdp_context_id`/`proxy_host` may stay empty for reclamation
    /// tests.
    fn make_context(busy: bool) -> Arc<BrowserContext> {
        let metadata = BrowserContextMetadata::new();
        metadata.is_busy.store(busy, Ordering::SeqCst);
        Arc::new(BrowserContext {
            metadata,
            tab: Arc::new(Mutex::new(None)),
            cdp_context_id: None,
            proxy_host: None,
        })
    }

    /// Provider that claims per-context routing without needing any real proxy: the guards under
    /// test run before the browser is launched, so nothing here is ever connected to.
    ///
    /// `supports_per_context_proxy` is a field rather than a constant so the inconsistent
    /// combination (routing per host, but no per-context config to take the host from) can be
    /// built at all — the trait's defaults make it easy to write by accident.
    #[derive(Debug, Clone)]
    struct HostPoolProvider {
        supports_per_context_proxy: bool,
    }

    impl browser_hive_common::ProxyProvider for HostPoolProvider {
        fn build_config(&self) -> Result<ProxyConfig> {
            Ok(ProxyConfig {
                proxy_url: Some("http://user:pass@203.0.113.1:60000".to_string()),
                scheme: browser_hive_common::ProxyScheme::Http,
                address: None,
                port: None,
                username: None,
                password: None,
            })
        }

        fn name(&self) -> &str {
            "host_pool_test"
        }

        fn clone_box(&self) -> Box<dyn ProxyProvider> {
            Box::new(self.clone())
        }

        fn supports_per_context_proxy(&self) -> bool {
            self.supports_per_context_proxy
        }

        fn assigns_proxy_host_per_context(&self) -> bool {
            true
        }
    }

    /// Provider that hands back a config with nothing in it — the shape every misconfiguration
    /// collapses into (unparseable port, empty URL, empty pool) by the time the pool sees it.
    #[derive(Debug, Clone)]
    struct ProxylessProvider;

    impl browser_hive_common::ProxyProvider for ProxylessProvider {
        fn build_config(&self) -> Result<ProxyConfig> {
            Ok(ProxyConfig {
                proxy_url: None,
                scheme: browser_hive_common::ProxyScheme::Http,
                address: None,
                port: None,
                username: None,
                password: None,
            })
        }

        fn name(&self) -> &str {
            "proxyless_test"
        }

        fn clone_box(&self) -> Box<dyn ProxyProvider> {
            Box::new(self.clone())
        }
    }

    fn scope_with(isolation: ContextIsolation, supports_per_context_proxy: bool) -> ScopeConfig {
        scope_with_provider(
            Box::new(HostPoolProvider {
                supports_per_context_proxy,
            }),
            isolation,
        )
    }

    fn scope_with_provider(
        proxy_provider: Box<dyn ProxyProvider>,
        isolation: ContextIsolation,
    ) -> ScopeConfig {
        ScopeConfig {
            name: "guard_test".to_string(),
            proxy_provider,
            min_contexts: 0,
            max_contexts: 1,
            session_mode: SessionMode::AlwaysNew,
            headless: true,
            lifecycle: ContextLifecycleConfig::default(),
            browser_path: None,
            diagnostics: browser_hive_common::DiagnosticsConfig::default(),
            binary_params_middlewares: vec![],
            tab_init_middlewares: vec![],
            context_isolation: isolation,
            destroy_session_on_block: false,
            block_quarantine: Duration::ZERO,
        }
    }

    /// Shared isolation runs every request in the browser's default context, which no CDP call
    /// can re-point at another proxy — a provider that routes per context would silently serve
    /// one address from its pool. The refusal has to happen before the browser is launched,
    /// which is also what makes this testable without one.
    #[tokio::test]
    async fn refuses_per_context_proxy_hosts_in_shared_isolation() {
        let Err(error) = BrowserPool::new(scope_with(ContextIsolation::Shared, true)).await else {
            panic!("shared isolation must not start with per-context proxy hosts");
        };

        let message = error.to_string();
        assert!(
            message.contains("host_pool_test") && message.contains("isolated"),
            "the error must name the provider and the required isolation mode: {message}"
        );
    }

    /// The host to route by comes from `get_context_proxy`, which is only consulted when
    /// `supports_per_context_proxy()` is true. With only the routing flag set there is no host to
    /// use, so every context would quietly fall back to the browser-wide proxy — while startup
    /// logs "each context gets its own proxy from the pool". Refusing beats routing nothing.
    #[tokio::test]
    async fn refuses_per_context_proxy_hosts_without_per_context_configs() {
        let Err(error) = BrowserPool::new(scope_with(ContextIsolation::Isolated, false)).await
        else {
            panic!("per-context routing must not start without per-context proxy configs");
        };

        let message = error.to_string();
        assert!(
            message.contains("host_pool_test") && message.contains("supports_per_context_proxy"),
            "the error must name the provider and the flag that is missing: {message}"
        );
    }

    /// The launch proxy is the floor under every in-request fallback: a tab that ends up in the
    /// default context still leaves through it. Without one, those same fallbacks - and the happy
    /// path too - scrape from the pod's own public IP, and nothing about the response says so.
    ///
    /// The opt-in half of this guard cannot be asserted here (starting successfully means
    /// launching a real browser); it is pinned by `NoProxyProvider`'s own test instead.
    #[tokio::test]
    async fn refuses_to_start_without_a_proxy() {
        let scope = scope_with_provider(Box::new(ProxylessProvider), ContextIsolation::Isolated);

        let Err(error) = BrowserPool::new(scope).await else {
            panic!("a provider with no proxy must not start a browser that scrapes directly");
        };

        let message = error.to_string();
        assert!(
            message.contains("proxyless_test") && message.contains("allows_direct_connection"),
            "the error must name the provider and the opt-in that would allow this: {message}"
        );
    }

    /// Selection must spread traffic, not pile it onto the first context. With per-context proxy
    /// routing the old "first idle wins" behaviour meant one exit IP carried most of the load
    /// while the rest of the purchased pool sat idle.
    #[test]
    fn selects_the_idle_context_with_the_fewest_requests() {
        let heavily_used = make_context(false);
        heavily_used
            .metadata
            .total_requests
            .store(40, Ordering::SeqCst);
        let barely_used = make_context(false);
        barely_used
            .metadata
            .total_requests
            .store(3, Ordering::SeqCst);

        // `heavily_used` comes first, which is exactly what the old implementation returned.
        let contexts = vec![heavily_used, barely_used.clone()];

        let selected = select_least_used_idle(&contexts, None).expect("one context is idle");
        assert_eq!(selected.metadata.id, barely_used.metadata.id);
    }

    /// A busy context is never a candidate, however little it has been used.
    #[test]
    fn skips_busy_contexts_however_lightly_used() {
        let busy_and_fresh = make_context(true);
        let idle_and_worn = make_context(false);
        idle_and_worn
            .metadata
            .total_requests
            .store(999, Ordering::SeqCst);

        let contexts = vec![busy_and_fresh, idle_and_worn.clone()];

        let selected = select_least_used_idle(&contexts, None).expect("one context is idle");
        assert_eq!(selected.metadata.id, idle_and_worn.metadata.id);

        assert!(select_least_used_idle(&[make_context(true)], None).is_none());
    }

    /// A context the origin has just refused must not be the one this origin gets next: with one
    /// exit IP pinned per context that request is refused too, and nothing else rotates a
    /// `reusable` context for hours.
    #[test]
    fn skips_contexts_quarantined_for_this_origin() {
        let blocked = make_context(false);
        blocked
            .metadata
            .quarantine_origin("example.com", Duration::from_secs(300));
        let fresh = make_context(false);
        fresh.metadata.total_requests.store(500, Ordering::SeqCst);

        let contexts = vec![blocked.clone(), fresh.clone()];

        // `blocked` has the lower request count and would win on capacity alone.
        let selected =
            select_least_used_idle(&contexts, Some("example.com")).expect("one context is idle");
        assert_eq!(selected.metadata.id, fresh.metadata.id);
    }

    /// The quarantine is a property of the pair (context, origin), not of the context: throwing
    /// away a context's warm cookies for every other site it serves is what this exists to avoid.
    #[test]
    fn a_quarantined_context_still_serves_other_origins() {
        let blocked = make_context(false);
        blocked
            .metadata
            .quarantine_origin("example.com", Duration::from_secs(300));

        let contexts = vec![blocked.clone()];

        assert!(select_least_used_idle(&contexts, Some("example.com")).is_none());
        let selected = select_least_used_idle(&contexts, Some("example.org"))
            .expect("another origin is unaffected");
        assert_eq!(selected.metadata.id, blocked.metadata.id);
    }

    /// An expired quarantine is simply gone — the first request after the cooldown is what decides
    /// whether the block is really over.
    #[test]
    fn quarantine_expires() {
        let context = make_context(false);
        context
            .metadata
            .quarantine_origin("example.com", Duration::ZERO);

        assert!(select_least_used_idle(&[context], Some("example.com")).is_some());
    }

    /// The last resort when the pool is at `max_contexts` and every idle context is quarantined:
    /// the one that recovers soonest, rather than refusing a request over a capacity problem the
    /// scope does not have.
    #[test]
    fn falls_back_to_the_soonest_recovering_context() {
        let long = make_context(false);
        long.metadata
            .quarantine_origin("example.com", Duration::from_secs(600));
        let short = make_context(false);
        short
            .metadata
            .quarantine_origin("example.com", Duration::from_secs(30));
        let busy = make_context(true);
        busy.metadata
            .quarantine_origin("example.com", Duration::from_secs(1));

        let contexts = vec![long, short.clone(), busy];

        let (selected, _) = select_soonest_unquarantined(&contexts, "example.com")
            .expect("a quarantined idle context is available");
        assert_eq!(
            selected.metadata.id, short.metadata.id,
            "a busy context is never a candidate, however soon its quarantine ends"
        );

        assert!(
            select_soonest_unquarantined(&contexts, "example.org").is_none(),
            "no fallback is needed for an origin nothing is quarantined for"
        );
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
