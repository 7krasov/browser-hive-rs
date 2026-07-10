# Local Development Guide

This guide helps you run Browser Hive locally using Docker Compose.

## Running with Docker Compose

### Quick Start

```bash
# 1. Build and run
docker-compose up --build

# Coordinator: http://localhost:50051 (gRPC)
# Worker metrics: http://localhost:9090/metrics
```

### Rebuild after code changes

```bash
# Rebuild and restart
docker-compose up --build

# Or rebuild specific services:
docker-compose up --build coordinator
docker-compose up --build worker
```

### View logs

```bash
# All services
docker-compose logs -f

# Coordinator only
docker-compose logs -f coordinator

# Worker only
docker-compose logs -f worker
```

### Stop

```bash
docker-compose down
```

## Proxy Configuration (Optional)

By default, the worker connects directly without a proxy (`PROXY_TYPE=none`).

The base worker supports a generic HTTP/SOCKS5 proxy out of the box via environment variables (see `crates/worker/src/providers.rs`):

```yaml
# Option 1: No proxy (default)
- PROXY_TYPE=none

# Option 2: Generic proxy with full URL
- PROXY_TYPE=generic
- PROXY_URL=http://user:pass@proxy.example.com:8080

# Option 3: Generic proxy with components
- PROXY_TYPE=generic
- PROXY_ADDRESS=proxy.example.com
- PROXY_PORT=8080
- PROXY_USERNAME=user
- PROXY_PASSWORD=pass
- PROXY_SCHEME=http  # http, https, or socks5
```

For provider-specific features (geo-targeting via `country_code`, sticky sessions), implement the `ProxyProvider` trait in your production crate and wire it into the worker binary.

## Testing

### Using grpcurl

```bash
# Install grpcurl
brew install grpcurl  # macOS
# or apt-get install grpcurl (Linux)

# Basic scraping request
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

# Wait for specific selector
grpcurl -plaintext \
  -d '{
    "scope_name": "local_dev",
    "url": "https://example.com/dynamic-page",
    "wait_selector": "#content-loaded",
    "wait_timeout_ms": 15000
  }' \
  localhost:50051 \
  scraper.coordinator.ScraperCoordinator/ScrapePage

# Skip if CAPTCHA detected
grpcurl -plaintext \
  -d '{
    "scope_name": "local_dev",
    "url": "https://example.com",
    "skip_selector": ".captcha-challenge"
  }' \
  localhost:50051 \
  scraper.coordinator.ScraperCoordinator/ScrapePage
```

### Testing Session Persistence

```bash
# Step 1: Create initial session (e.g., login page)
response=$(grpcurl -plaintext \
  -d '{
    "scope_name": "local_dev",
    "url": "https://example.com/login"
  }' \
  localhost:50051 \
  scraper.coordinator.ScraperCoordinator/ScrapePage)

# Extract session_id from response (format: "{worker_id}:{context_id}")
session_id=$(echo $response | jq -r '.session_id')
echo "Session ID: $session_id"

# Step 2: Reuse session for subsequent request
grpcurl -plaintext \
  -d "{
    \"scope_name\": \"local_dev\",
    \"url\": \"https://example.com/dashboard\",
    \"session_id\": \"$session_id\"
  }" \
  localhost:50051 \
  scraper.coordinator.ScraperCoordinator/ScrapePage

# The same browser context (with cookies) will be used!
```

### Understanding Error Responses

All responses include error information even when successful:

```json
{
  "success": true,
  "status_code": 200,
  "content": "<html>...</html>",
  "error_message": "",
  "error_code": 0,
  "execution_time_ms": 2341,
  "session_id": "worker-local:ctx-abc123",
  "worker_id": "worker-local",
  "context_id": "ctx-abc123",
  "ray_id": "ray_1a2b3c"
}
```

Use `session_id` to reuse the same browser context in subsequent requests; `worker_id`/`context_id` are its components (for logging/debugging).

**Important**: Check `success` field first, not just HTTP `status_code`.

**Common error codes**:
- `0` - Success
- `4001` - Invalid URL (fix URL and retry)
- `4002` - Session not found (retry without context_id)
- `4042` - Wait selector not found (check selector)
- `4043` - Skip selector found (expected - skip content)
- `5003` - Browser error (retry - auto-recovers)
- `5004` - Network error (retry with backoff)

See [ERROR_HANDLING.md](ERROR_HANDLING.md) for complete error handling guide.

### Prometheus Metrics

```bash
# View worker metrics
curl http://localhost:9090/metrics
```

See [METRICS.md](METRICS.md) for the metric list and semantics.

## Local Mode Architecture

```
┌─────────────────┐
│   Coordinator   │ (port 50051)
│  COORDINATOR_   │
│   MODE=local    │
└────────┬────────┘
         │ hardcoded: worker:50052
         ▼
┌─────────────────┐
│     Worker      │ (ports 50052, 9090)
│   + Chrome/     │
│     Chromium    │
└─────────────────┘
```

**Local mode features:**
- ✅ Coordinator uses a fixed endpoint from env: `WORKER_ENDPOINT` (default `worker:50052`) + `WORKER_SCOPE_NAME` (default `local_dev`)
- ✅ Multiple workers/scopes supported via `WORKER_ENDPOINTS=scope1:host1:port1,scope2:host2:port2,...` (takes precedence over `WORKER_ENDPOINT`)
- ✅ Chromium runs in Docker container (Chrome and Brave also supported via `WORKER_BROWSER_PATH`)
- ✅ Easy development and testing environment

## Troubleshooting

### Coordinator can't connect to worker

Ensure both services are in the same network:
```bash
docker-compose ps
```

### Chrome fails to start

Increase `shm_size` in `docker-compose.yml`:
```yaml
worker:
  shm_size: '4gb'  # was 2gb
```

### Build takes too long

Docker Compose rebuilds the entire project. For faster development:

1. Run coordinator locally:
   ```bash
   cargo run --bin coordinator
   # Set COORDINATOR_MODE=local, WORKER_ENDPOINT=localhost:50052
   ```

2. Keep worker in Docker:
   ```bash
   docker-compose up worker
   ```

## Next Steps

For production deployment:
1. Implement custom proxy providers for your needs
2. Configure production-grade infrastructure
3. Set up monitoring and metrics collection
