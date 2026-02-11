# Browser Hive

Distributed web scraping system built in Rust with intelligent session management and extensible proxy rotation.

## Features

- 🚀 **Distributed Architecture** - Coordinator routes requests to worker pods
- 🔄 **Session Management** - Persist browser contexts across requests for logged-in scraping
- 🌍 **Extensible Proxy Support** - Implement custom proxy providers via trait system
- 🎯 **Scope-based Workers** - Logical groups with different proxy/lifecycle configs
- 📈 **Prometheus Metrics** - Built-in metrics for monitoring
- 🔒 **CDP BrowserContexts** - True isolation between sessions with separate cookies/storage

## Quick Start (Local Development)

```bash
# 1. Run with Docker Compose
docker-compose up --build

# 2. Test basic scraping
grpcurl -plaintext \
  -d '{
    "scope_name": "local_dev",
    "url": "https://example.com",
    "timeout_seconds": 30,
    "wait_strategy": "network_idle",
    "wait_timeout_ms": 10000
  }' \
  localhost:50051 \
  scraper.coordinator.ScraperCoordinator/ScrapePage

# 3. Test with wait selector
grpcurl -plaintext \
  -d '{
    "scope_name": "local_dev",
    "url": "https://example.com/dynamic-content",
    "wait_selector": "#content-loaded",
    "wait_timeout_ms": 15000
  }' \
  localhost:50051 \
  scraper.coordinator.ScraperCoordinator/ScrapePage
```

See [LOCAL_DEV.md](LOCAL_DEV.md) for detailed local development guide.

## Architecture

```
┌─────────────┐
│   Client    │
└──────┬──────┘
       │ gRPC
       ▼
┌─────────────────────┐
│   Coordinator       │ (routes requests to workers)
└──────┬──────────────┘
       │
       ├─── Worker Pod 1 (scope: scope_a)
       │    └── Browser Process
       │        ├── CDP Context 1 (session: w1:c1)
       │        │   └── Page (reusable)
       │        ├── CDP Context 2 (session: w1:c2)
       │        └── CDP Context 3 (session: w1:c3)
       │
       └─── Worker Pod 2 (scope: scope_b)
            └── Browser Process
                ├── CDP Context 1
                └── CDP Context 2
```

### Key Components

- **Coordinator** - Routes requests, manages sessions, discovers workers
- **Worker** - Manages browser pool with isolated CDP contexts
- **BrowserContext** - Isolated browsing environment (cookies, storage, cache)
- **Page** - Single tab within context (reused for session persistence)

## Project Structure

```
crates/
├── coordinator/           # gRPC service for request routing
├── worker/                # Browser pool and scraping execution
├── common/                # Shared types and utilities
├── proto/                 # Protocol buffer definitions
└── tokio-cancellation-ext/# Tokio CancellationToken extensions

src/
└── lib.rs                 # Root crate facade (re-exports public API)

ops/
└── docker/
    └── local/             # Docker Compose for local dev
```

## Session Management

Browser Hive supports session reuse for logged-in scraping:

```bash
# 1. First request - returns context_id
grpcurl -plaintext -d '{
  "scope_name": "local_dev",
  "url": "https://example.com/login"
}' localhost:50051 scraper.coordinator.ScraperCoordinator/ScrapePage

# Response includes: "context_id": "ctx-abc123"

# 2. Subsequent requests - reuse session
grpcurl -plaintext -d '{
  "scope_name": "local_dev",
  "url": "https://example.com/dashboard",
  "context_id": "ctx-abc123"
}' localhost:50051 scraper.coordinator.ScraperCoordinator/ScrapePage
```

The same browser context (with cookies) will be used! If you receive `ERROR_CODE_SESSION_NOT_FOUND` (4002), the session expired - retry without `context_id` to start a new session.

## Wait Strategies

Browser Hive supports flexible wait strategies to handle dynamic content:

### Available Strategies

- **`network_idle`** (default) - Wait until network is idle (no requests for 500ms)
- **`timeout`** - Wait for a fixed duration
- **`selector`** - Wait for a specific CSS selector to appear

### Wait Selector

Use `wait_selector` to wait for specific elements:

```bash
# Wait for login button to appear
grpcurl -plaintext -d '{
  "scope_name": "local_dev",
  "url": "https://example.com",
  "wait_selector": "#login-button",
  "wait_timeout_ms": 15000
}' localhost:50051 scraper.coordinator.ScraperCoordinator/ScrapePage
```

If the selector is not found within `wait_timeout_ms`, the request returns:
- `success: false`
- `error_code: ERROR_CODE_SELECTOR_NOT_FOUND` (4042)
- `content`: Full page HTML (still available for inspection)

### Skip Selector

Use `skip_selector` to detect unwanted content (CAPTCHA, login walls, etc.):

```bash
# Skip if CAPTCHA appears
grpcurl -plaintext -d '{
  "scope_name": "local_dev",
  "url": "https://example.com",
  "skip_selector": ".captcha-challenge"
}' localhost:50051 scraper.coordinator.ScraperCoordinator/ScrapePage
```

If the skip selector is found:
- `success: false`
- `error_code: ERROR_CODE_SKIP_SELECTOR_FOUND` (4043)
- `content`: Full page HTML (for analysis)

This is **expected behavior**, not an error. Your client should handle this gracefully.

## Error Handling

Browser Hive uses a dual-layer error system:

1. **gRPC Status** - Always `OK (0)` if response can be returned
2. **ErrorCode** - Operational errors in response body

### Key Principle

> gRPC Status is only non-zero for infrastructure failures (network partition, etc.). All operational errors (navigation failures, timeouts, selectors not found) return gRPC OK with appropriate `error_code` in the response.

### Response Structure

Every response includes:
- `success: bool` - Overall operation success
- `error_code: ErrorCode` - Machine-readable error code (0 = success)
- `error_message: string` - Human-readable error details
- `execution_time_ms: uint64` - Request execution time
- `content: string` - Page HTML (may be partial on errors)

### Common ErrorCodes

| Code | Value | Description | Action |
|------|-------|-------------|--------|
| `ERROR_CODE_NONE` | 0 | Success | Use content |
| `ERROR_CODE_INVALID_URL` | 4001 | Invalid URL format | Fix URL |
| `ERROR_CODE_SESSION_NOT_FOUND` | 4002 | Session expired | Retry without session |
| `ERROR_CODE_TIMEOUT_BROWSER` | 4041 | Browser timeout | Increase timeout |
| `ERROR_CODE_SELECTOR_NOT_FOUND` | 4042 | Wait selector not found | Check selector |
| `ERROR_CODE_SKIP_SELECTOR_FOUND` | 4043 | Skip selector found | Expected - skip content |
| `ERROR_CODE_BROWSER_ERROR` | 5003 | Browser crashed | Retry - auto-recovers |
| `ERROR_CODE_NETWORK_ERROR` | 5004 | Network error | Retry |

See [ERROR_HANDLING.md](ERROR_HANDLING.md) for complete error handling guide with examples and best practices.

## Configuration

### Scopes

Each worker scope has independent configuration:
- Custom proxy provider implementation
- Contexts per pod
- Lifecycle settings (max lifetime, max requests, cache limits)
- Resource limits

### Proxy Providers

Browser Hive uses a trait-based system for proxy providers. Implement the `ProxyProvider` trait to add your own proxy service:

```rust
use browser_hive::prelude::*;

#[derive(Debug, Clone)]
pub struct MyProxyProvider {
    address: String,
    port: u16,
    credentials: (String, String),
}

impl ProxyProvider for MyProxyProvider {
    fn build_config(&self) -> anyhow::Result<ProxyConfig> {
        Ok(ProxyConfig {
            proxy_url: None,
            scheme: ProxyScheme::Http,
            address: Some(self.address.clone()),
            port: Some(self.port),
            username: Some(self.credentials.0.clone()),
            password: Some(self.credentials.1.clone()),
        })
    }

    fn name(&self) -> &str { "my_provider" }
    fn clone_box(&self) -> Box<dyn ProxyProvider> { Box::new(self.clone()) }
}
```

See [`src/lib.rs`](src/lib.rs) for the public API facade and re-exports.

### Metrics

Workers expose Prometheus metrics on port `9090`:
- `browser_hive_worker_total_contexts{scope}`
- `browser_hive_worker_active_contexts{scope}`
- `browser_hive_worker_available_slots{scope}`
- `browser_hive_worker_requests_total{scope}`
- `browser_hive_worker_requests_failed{scope}`

## Development

```bash
# Build all
cargo build

# Run tests
cargo test

# Check
cargo check

# Format
cargo fmt

# Lint
cargo clippy
```

## License

MIT OR Apache-2.0
