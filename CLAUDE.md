# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Browser Hive is a distributed web scraping system built in Rust with intelligent session management and extensible proxy rotation. The system consists of a coordinator that receives scraping requests and distributes them to workers, where each worker manages multiple browser contexts with Chrome/Chromium or Brave browser.

## Architecture

### Workspace Structure

This is a Cargo workspace with 5 crates:

- **coordinator** (`crates/coordinator`): gRPC service that receives external scraping requests, discovers workers, manages session persistence, and routes requests to appropriate workers
- **worker** (`crates/worker`): gRPC service that manages browser pool with multiple contexts, executes scraping via headless_chrome, and handles context lifecycle (rotation, recycling)
- **proto** (`crates/proto`): Protocol buffer definitions for coordinator-to-worker and client-to-coordinator communication, built via tonic-build
- **common** (`crates/common`): Shared types, configurations, session management, wait strategies, and utilities
- **tokio-cancellation-ext** (`crates/tokio-cancellation-ext`): Extension utilities for tokio_util's CancellationToken

### Key Architectural Patterns

**Session Management**: Sessions use the format `{worker_id}:{context_id}` to enable clients to reuse the same browser context across requests. The coordinator's `SessionManager` (defined in `common/src/session.rs`) tracks active sessions and routes subsequent requests to the correct worker+context.

**Worker Discovery**: The coordinator can discover workers through various mechanisms (implemented in `coordinator/src/worker_discovery.rs::WorkerDiscovery`). In local mode, it uses hardcoded endpoints. Workers are organized by "scope" which defines proxy provider configuration and context lifecycle settings.

**Browser Context Lifecycle**: Workers maintain a pool of CDP BrowserContexts (managed by `worker/src/browser_pool.rs::BrowserPool`), each with configurable lifecycle based on time, request count, idle time, and cache size. Contexts are recycled according to `RotationStrategy` (enum defined in `common/src/config.rs`). Each context is created via `browser.new_tab()` for true isolation.

**One Tab Per Context**: The architecture uses 1 page (tab) per CDP BrowserContext. This simplifies resource management while maintaining full isolation. The page is reused across requests for session persistence (cookies are preserved).

**Domain Affinity**: Browser contexts track domains they've scraped (via `BrowserContextMetadata::primary_domains` field in `common/src/types.rs`) to optimize cache usage. When routing new requests, the system prefers contexts that have already visited the target domain.

**Resource Management**: Each context uses an `AtomicBool` (`is_busy`) to track availability (field in `BrowserContextMetadata` struct in `common/src/types.rs`). Workers report available slots to the coordinator, which uses this for load balancing.

**Error Handling**: Browser Hive uses a dual-layer error system. gRPC Status is ALWAYS 0 (OK) if the system can return a response to the client. All operational errors (navigation failures, timeouts, selectors not found, browser crashes) are returned as gRPC OK responses with appropriate `ErrorCode` in the response body. Only true infrastructure failures (network partition, coordinator unreachable) use non-zero gRPC status codes. This allows clients to always parse responses uniformly and get detailed error context (execution_time_ms, context metadata, etc.). All error responses include `execution_time_ms` field. ErrorCode enum is defined in `crates/proto/proto/worker.proto` with codes: INVALID_URL (4001), SESSION_NOT_FOUND (4002), TIMEOUT_BROWSER (4041), SELECTOR_NOT_FOUND (4042), SKIP_SELECTOR_FOUND (4043), BROWSER_ERROR (5003), NETWORK_ERROR (5004), CONTEXT_CREATION_FAILED (5005), TERMINATING (5006). See ERROR_HANDLING.md for complete details.

**Wait Strategies**: The system supports flexible wait strategies via `WaitStrategy` trait (`common/src/wait_strategy.rs`). Default strategy: `network_idle` (two-phase: wait for idle → search for selector). Behavior depends on `wait_timeout_ms` and `wait_selector`: (1) No selector: wait for idle with timeout=effective_timeout(wait_timeout_ms), (2) Selector present: total timeout always `DEFAULT_WAIT_TIMEOUT_MS`, wait for idle then search selector for min(wait_timeout_ms, remaining time). Clients specify `wait_selector` (CSS selector to find after idle), `skip_selector` (returns ERROR_CODE_SKIP_SELECTOR_FOUND if found), `wait_timeout_ms` (0=default, max=`MAX_WAIT_TIMEOUT_MS`, values above are capped). Final selector check always performed before returning. Returns ERROR_CODE_SELECTOR_NOT_FOUND with full page content if not found.

## Development Commands

### Local Development (Docker Compose)

```bash
# Quick start - builds and runs coordinator + worker
docker-compose up --build

# View logs
docker-compose logs -f

# Stop
docker-compose down
```

See [LOCAL_DEV.md](LOCAL_DEV.md) for detailed local development guide.

### Building

```bash
# Build all crates
cargo build

# Build in release mode
cargo build --release

# Build specific crate
cargo build -p browser-hive-coordinator
cargo build -p browser-hive-worker
```

### Running (native)

```bash
# Run coordinator (local mode)
COORDINATOR_MODE=local \
WORKER_ENDPOINT=localhost:50052 \
WORKER_SCOPE_NAME=local_dev \
cargo run --bin coordinator

# Run worker (requires Chrome installed)
WORKER_SCOPE_NAME=local_dev \
WORKER_GRPC_PORT=50052 \
WORKER_MAX_CONTEXTS=3 \
cargo run --bin worker
```

### Testing

```bash
# Run all tests
cargo test

# Run tests for specific crate
cargo test -p browser-hive-common
cargo test -p browser-hive-coordinator

# Run a single test
cargo test test_name

# Run tests with output
cargo test -- --nocapture
```

### Linting and Formatting

```bash
# Check formatting
cargo fmt --check

# Format code
cargo fmt

# Run clippy
cargo clippy

# Run clippy with all targets
cargo clippy --all-targets --all-features
```

### Proto Generation

Proto files are automatically compiled during build via build.rs in the proto crate. To regenerate:

```bash
cargo build -p browser-hive-proto
```

Proto definitions are in `crates/proto/proto/`:
- `coordinator.proto`: Client-facing ScraperCoordinator service
- `worker.proto`: Internal WorkerService for pod-to-pod communication

## Important Implementation Details

### Proxy Configuration

Proxy providers use a trait-based system (`trait ProxyProvider` in `common/src/proxy.rs`) allowing users to implement custom providers. Each scope has its own proxy configuration via `Box<dyn ProxyProvider>` in `ScopeConfig`.

### Browser Middleware System

Browser Hive uses a middleware system to customize browser behavior without modifying core code. There are **two independent middleware types** that run at different stages:

**BrowserBinaryParamsMiddleware** (`common/src/browser_middleware.rs`)
- **Purpose**: Modifies browser launch arguments **before browser creation**
- **Runs**: Once per browser launch during `BrowserPool::new()`
- **Default implementations**:
  - `DefaultBinaryParamsMiddleware` - for Chrome/Chromium: anti-bot args, cache config, performance optimizations
  - `BraveBinaryParamsMiddleware` - for Brave browser: same as above plus `--disable-brave-extension` to prevent Shields from interfering with scraping
- **Use cases**: Custom Chrome flags, feature toggles, performance tuning

**TabInitMiddleware** (`common/src/browser_middleware.rs`)
- **Purpose**: Executes CDP commands on tabs **after creation**
- **Runs**: Every time a new tab is created (initial contexts + recycled contexts)
- **Default**: `DefaultTabInitMiddleware` - overrides User-Agent to replace "HeadlessChrome" with "Chrome" in headless mode using lazy initialization (detects UA on first tab, caches for subsequent tabs)
- **Use cases**: JavaScript injection, navigator property overrides, CDP configuration

**Configuration**: Each `ScopeConfig` contains two separate vectors: `binary_params_middlewares` and `tab_init_middlewares`. Users must explicitly add middleware to these vectors. The base worker (`crates/worker/src/main.rs`) demonstrates default configuration.

**Performance**: `BinaryParamsMiddleware` runs once (not performance-critical). `TabInitMiddleware` runs per tab (must be fast).

See `MIDDLEWARE_EXAMPLES.md` for detailed usage examples and production patterns.

### Context Lifecycle Monitoring

The browser pool starts a background task (via `BrowserPool::start_lifecycle_monitor` in `worker/src/browser_pool.rs`) that periodically checks contexts against lifecycle thresholds and recycles them when needed. Contexts are only recycled when no active pages are running.

### Browser Pool Recovery

The system automatically recovers from two types of failures:

**1. Dead Browser Process** ("connection is closed")

Chrome browser processes are configured with `idle_browser_timeout` (default: 1 hour) in `LaunchOptions`. When the browser is idle for this duration, `headless_chrome` terminates the process to free resources.

When a browser process dies (either from timeout or crash), subsequent `browser.new_tab()` calls fail with "connection is closed" error. The worker service detects this condition and automatically:
1. Creates a new `BrowserPool` with a fresh Chrome process (via `WorkerService::recreate_browser_pool`)
2. Replaces the dead pool atomically using `RwLock`
3. Retries the failed operation with the new pool

**Session Impact**: When browser pool is recreated, all existing sessions are lost. Clients receive new session IDs on their next request. This is acceptable since browser death only occurs after extended idle periods (1+ hours).

**2. Dead Tab / CDP Session** ("No session with given id")

When a request hits a hard timeout (navigation or wait strategy stuck), the worker closes the tab via `tab.close(false)` to abort the stuck CDP call. The tab's CDP session is destroyed, but:
- The browser process remains alive
- In isolated mode, the CDP BrowserContext survives (cookies/storage preserved)
- The context remains in the pool (Reusable/ReusablePreinit modes)

When the next request uses this context, CDP operations fail with "No session with given id". The worker detects this and automatically:
1. Creates a new tab in the existing CDP BrowserContext (for isolated mode) using `BrowserPool::create_tab_in_context()`
2. Or creates a new tab in the shared context (for shared mode) using `BrowserPool::create_tab_shared()`
3. Applies tab init middlewares to the new tab
4. Continues processing the request normally

**Session Impact**: Session state (cookies, storage) is preserved because the CDP BrowserContext survives - only the tab is recreated. This is transparent to clients.

**Implementation**: Tab recovery is implemented in `worker/src/service.rs::scrape_page_internal` via `is_dead_tab_error()` check. Tab creation methods are in `worker/src/browser_pool.rs`.

### gRPC Communication

Both coordinator and worker use tonic for gRPC. The coordinator acts as both a gRPC server (for clients) and client (to workers). Workers only act as gRPC servers.

### Browser Stealth and Anti-Bot Protection

The system is designed to bypass anti-bot systems naturally through browser middleware:

- **Chrome arguments**: `DefaultBinaryParamsMiddleware` disables automation markers (`--disable-blink-features=AutomationControlled`, `--exclude-switches=enable-automation`)
- **User-Agent override**: `DefaultTabInitMiddleware` replaces "HeadlessChrome" with "Chrome" via CDP in headless mode
- **Brave browser**: Use `BraveBinaryParamsMiddleware` instead of `DefaultBinaryParamsMiddleware` for Brave. Brave has built-in anti-fingerprinting and tracking protection. Set `WORKER_BROWSER_PATH=/usr/bin/brave-browser` to use Brave.
- **Customization**: Users can add custom middleware for additional stealth (WebGL spoofing, timezone override, navigator properties, etc.)

The browser configuration is entirely controlled through middleware, allowing full customization while keeping core code clean.

### Metrics and Monitoring

Workers expose Prometheus metrics on port 9090 at `/metrics`:

- `browser_hive_worker_total_contexts{scope}` - Total browser contexts in pool
- `browser_hive_worker_active_contexts{scope}` - Busy contexts processing requests
- `browser_hive_worker_available_slots{scope}` - Available tab slots
- `browser_hive_worker_requests_total{scope}` - Total requests processed
- `browser_hive_worker_requests_failed{scope}` - Failed requests

Metrics are implemented in `worker/src/metrics.rs` and updated after each request. The coordinator can also query worker stats via gRPC `GetStats` endpoint.

### Deployment Modes

The coordinator supports multiple deployment modes:
- **Local mode**: Uses hardcoded worker endpoints (set `COORDINATOR_MODE=local`)
- **Production mode**: Can be extended with custom worker discovery mechanisms

### Configuration

**Worker configuration** is loaded from environment variables (via `load_config_from_env` function in `worker/src/main.rs`). Required variables:
- `WORKER_SCOPE_NAME` - Scope identifier
- `WORKER_GRPC_PORT` - gRPC server port
- `WORKER_MAX_CONTEXTS` - Maximum concurrent browser contexts (= max concurrent requests)
- Proxy settings (if using real proxy): `PROXY_TYPE`, credentials, etc.

**Optional worker variables**:
- `WORKER_MIN_CONTEXTS` - Minimum browser contexts pre-created on startup, used by `reusable_preinit` mode (default: `WORKER_MAX_CONTEXTS`)
- `WORKER_SESSION_MODE` - Session management mode (see table below)
- `WORKER_ENABLE_BROWSER_DIAGNOSTICS` - Enable console logs and page diagnostics collection (default: `false`, disable for faster response)
- `WORKER_HEADLESS` - Run browser in headless mode (default: `true`)
- `WORKER_MAX_IDLE_TIME_SECS` - Max idle time before context recycling (default: 300)
- `WORKER_CONTEXT_ISOLATION` - Context isolation mode: `shared` or `isolated` (default: `isolated`)
- `WORKER_BROWSER_PATH` - Path to browser binary (default: auto-detect Chrome/Chromium). Set to `/usr/bin/brave-browser` for Brave

**Session modes (`WORKER_SESSION_MODE`)**:

| Mode | Reusable? | Behavior | Best for |
|------|-----------|----------|----------|
| `always_new` | No | Fresh context per request, destroyed after | One-shot scraping, per-request geo-targeting |
| `reusable` (default) | Yes | Contexts reused until recycled by lifecycle | Multi-page workflows, login flows |
| `reusable_preinit` | Yes | Same as `reusable`, pre-created on startup | Low-latency first requests |

**Note**: Reusable contexts are NOT kept forever. They are recycled based on lifecycle settings:
- `WORKER_MAX_IDLE_TIME_SECS` (default: 5 min) - recycled after being idle
- `max_lifetime` (default: 6 hours) - recycled after this age
- `max_requests` (default: 10,000) - recycled after this many requests

**Session modes and proxy country behavior:**

The `country_code` field in scrape requests controls proxy geo-targeting. How it interacts with session modes:

| Mode | Client sends `country_code` | Client doesn't send `country_code` |
|------|---------------------------|-----------------------------------|
| `always_new` | New context with requested country, no sticky session | New context, provider picks random IP/country per request |
| `reusable` | **Dedicated** new context with requested country + sticky session (if provider supports it) | Reuses idle context or creates new; sticky session keeps same IP |
| `reusable_preinit` | Same as `reusable` | Pre-created contexts have no country; sticky session keeps same IP |

Key behavior: In `reusable`/`reusable_preinit` modes, when `country_code` is specified, `BrowserPool::get_or_create_context()` **always creates a dedicated new context** instead of reusing an idle one. This is because proxy country affects connection identity (exit IP) and cannot be changed on an existing context. The decision is driven by `ProxyParams::requires_dedicated_context()`.

**Sticky sessions** (if supported by proxy provider): The proxy provider uses the browser context ID as a session identifier in proxy credentials, ensuring the same proxy IP is assigned to all requests through that context. This is automatically enabled for `reusable`/`reusable_preinit` modes. Production proxy providers should accept a `use_session: bool` parameter and include session ID in proxy credentials when enabled.

**Coordinator modes**:
- Local mode: Set `COORDINATOR_MODE=local` to use hardcoded worker endpoint from `WORKER_ENDPOINT` env var
- Production mode: Can be customized with different worker discovery implementations

### Session Management

- Session cleanup runs every 60 seconds with configurable TTL (via `SessionManager::start_cleanup_task` in `common/src/session.rs`)
- Sessions persist browser context state (cookies, storage) between requests

## Common Patterns

When adding new features:

1. Proto changes require updating both `coordinator.proto` and/or `worker.proto`, then rebuilding the proto crate
2. Session-related features should consider the session lifecycle in `SessionManager`
3. Context management changes must account for the recycling logic in `BrowserPool`
4. New metrics should be added to both `WorkerStats` and `ClusterStats` types
5. Configuration changes need updates in `ScopeConfig`, `WorkerConfig`, or `CoordinatorConfig`
6. **Browser customization**: Use middleware pattern instead of hardcoding browser behavior. Create `BrowserBinaryParamsMiddleware` for Chrome args or `TabInitMiddleware` for CDP operations. Add middleware to `ScopeConfig` in production code, not in core library. Default middlewares are in `common/src/browser_middleware.rs`. See MIDDLEWARE_EXAMPLES.md for patterns.
7. **Error handling**: NEVER return `Status::internal` or `Status::resource_exhausted` for operational errors. Always return `Ok(ScrapePageResponse)` with appropriate `ErrorCode` and detailed `error_message`. Only use gRPC error statuses for true infrastructure failures. All error responses must include `execution_time_ms`. See ERROR_HANDLING.md and worker/src/service.rs for patterns.
8. **Wait strategies**: New wait strategies should implement the `WaitStrategy` trait in `common/src/wait_strategy.rs` and be registered in `WaitStrategyRegistry`. Always handle both success and timeout cases gracefully.
