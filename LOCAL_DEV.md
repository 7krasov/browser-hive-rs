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

By default, worker uses example proxy configuration (won't work for real sites).

To use a real proxy, you'll need to:
1. Implement the `ProxyProvider` trait for your proxy service
2. Update the worker binary to use your provider
3. Configure environment variables in `docker-compose.yml`

See the project documentation for details on implementing custom proxy providers.

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

# Extract context_id from response
context_id=$(echo $response | jq -r '.context_id')
echo "Session ID: $context_id"

# Step 2: Reuse session for subsequent request
grpcurl -plaintext \
  -d "{
    \"scope_name\": \"local_dev\",
    \"url\": \"https://example.com/dashboard\",
    \"context_id\": \"$context_id\"
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
  "context_id": "ctx-abc123"
}
```

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
- ✅ Coordinator uses hardcoded endpoint: `worker:50052`
- ✅ Single scope: `local_dev`
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
