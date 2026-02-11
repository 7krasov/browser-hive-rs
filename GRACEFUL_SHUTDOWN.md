# Graceful Shutdown Implementation

This document describes the graceful shutdown implementation for Browser Hive's distributed web scraping system.

## Overview

Browser Hive implements graceful shutdown to handle POSIX signals (SIGTERM, SIGINT) during pod termination, ensuring:
- No request data loss
- Clean browser context cleanup
- Proper response to clients (including TERMINATING error code)
- Automatic retry on healthy workers

## Architecture

### Worker Graceful Shutdown

When a Worker receives SIGTERM:

1. **Immediate Response** (t=0ms)
   - Sets `is_ready = false` → K8s readiness probe fails
   - Calls `cancellation_token.cancel()`
   - New requests immediately return `ERROR_CODE_TERMINATING`

2. **Active Request Handling**
   - Blocking operations wrapped in `spawn_blocking` + `tokio::select!`
   - When signal arrives, client gets TERMINATING response immediately
   - Blocking thread continues as "zombie" until SIGKILL (acceptable)

3. **Graceful Wait** (t=0-60s)
   - Waits for all active requests to complete
   - Polls every 500ms and logs remaining count
   - Terminates after all requests finish or 60s timeout

4. **K8s Integration**
   - Readiness probe removes pod from Service endpoints (2-5s propagation)
   - `preStop` hook delays 5s for endpoint propagation
   - `terminationGracePeriodSeconds: 60`

### Coordinator Graceful Shutdown

When a Coordinator receives SIGTERM:

1. **Immediate Response** (t=0ms)
   - Calls `cancellation_token.cancel()`
   - New requests immediately return `ERROR_CODE_TERMINATING`

2. **Health Monitoring**
   - Background task polls worker health every 1 second
   - Maintains `healthy_workers` set for filtering
   - Removes unhealthy workers from selection

3. **Retry Logic with Deadline Tracking**
   ```rust
   deadline = start_time + timeout_seconds
   for attempt in 1..=3 {
       remaining_time = deadline - now
       if remaining_time < 10s { break; }

       response = worker.scrape_page(remaining_time)
       if response.error_code != TERMINATING {
           return response;  // Success or non-retryable error
       }

       exclude_worker(failed_worker)
       select_new_worker(healthy_workers, excluded_workers)
   }
   ```

4. **Graceful Wait**
   - Waits for all active requests to complete
   - Active requests may include retries to other workers
   - Terminates after all complete or 60s timeout

## Error Codes

### ERROR_CODE_TERMINATING (5006)

Returned when Worker or Coordinator is shutting down.

**Client should**:
- Retry the request immediately (coordinator will route to healthy worker)
- Or wait and retry after a few seconds

**Coordinator automatically**:
- Retries on different healthy worker (max 3 attempts)
- Respects client timeout deadline
- Returns final TERMINATING if all retries fail

## Implementation Details

### tokio-cancellation-ext Crate

Custom crate providing cancellation utilities:

```rust
// Async operations
use tokio_cancellation_ext::{CancellationExt, CancellationToken};

async_operation()
    .with_cancellation::<MyError>(&token, "operation_name")
    .await?;

// Sync operations
use tokio_cancellation_ext::check_cancellation;

loop {
    check_cancellation(&token, "loop_iteration")?;
    // ... blocking work
}
```

### Blocking Operations Pattern

Worker uses `spawn_blocking` + `tokio::select!` for CDP operations:

```rust
let handle = tokio::task::spawn_blocking(move || {
    tab.navigate_to(&url)
});

tokio::select! {
    _ = cancellation_token.cancelled() => {
        // Client gets response immediately
        return Ok(ScrapePageResponse {
            error_code: ErrorCode::Terminating,
            ...
        });
    }
    result = handle => {
        // Process result normally
    }
}
```

### Wait Strategy Cancellation

Wait strategies check cancellation every 500ms:

```rust
impl WaitStrategy for NetworkIdleStrategy {
    fn wait(&self, ..., cancellation_token: &CancellationToken) -> Result<WaitResult> {
        loop {
            check_cancellation(cancellation_token, "wait_strategy:phase1")?;
            // ... network idle check
        }
    }
}
```

## Testing Locally

### Quick Test with Docker Compose

```bash
# Start services
docker-compose up -d

# Wait for ready
sleep 10

# Start a long request (in another terminal)
grpcurl -plaintext -d '{
  "scope_name": "local_dev",
  "url": "https://example.com",
  "timeout_seconds": 60
}' localhost:50051 scraper.coordinator.ScraperCoordinator/ScrapePage

# Trigger graceful shutdown
docker-compose kill -s SIGTERM worker

# Watch logs
docker-compose logs -f worker
```

### Automated Test Script

```bash
# Run complete test scenario
./test-graceful-shutdown.sh
```

Expected output:
```
✓ Worker receives SIGTERM
✓ Worker sets is_ready = false
✓ Worker cancels operations
✓ Active requests return TERMINATING or complete
✓ Worker waits for active requests
✓ Worker shuts down gracefully
```

## Kubernetes Deployment

See [K8S_DEPLOYMENT.md](./K8S_DEPLOYMENT.md) for complete K8s configuration including:
- gRPC readiness probes
- terminationGracePeriodSeconds
- preStop hooks
- RBAC configuration
- Service setup

### Key K8s Configuration

Worker Deployment:
```yaml
spec:
  terminationGracePeriodSeconds: 60
  containers:
  - name: worker
    readinessProbe:
      grpc:
        port: 50052
        service: scraper.worker.WorkerService
      initialDelaySeconds: 5
      periodSeconds: 2
    lifecycle:
      preStop:
        exec:
          command: ["/bin/sh", "-c", "sleep 5"]
```

## Monitoring

### Worker Logs

During graceful shutdown, worker logs:
```
WARN  Received SIGTERM signal
INFO  Worker marked as not ready (readiness probe will fail)
INFO  Cancelling all active operations...
INFO  Starting graceful shutdown, waiting for 3 active request(s) to complete...
INFO  Waiting for 3 request(s) to complete...
INFO  Waiting for 2 request(s) to complete...
INFO  Waiting for 1 request(s) to complete...
INFO  All active requests completed
INFO  Worker shutdown complete
```

### Coordinator Logs

During retry:
```
WARN  Worker worker-1 returned TERMINATING (attempt 1)
INFO  Attempting to retry on another worker (remaining time: 55s)
INFO  Retrying on worker: worker-2
INFO  Attempt 2: Forwarding to worker worker-2 (timeout: 55s, ...)
```

### Metrics

Workers expose metrics on port 9090:
- `browser_hive_worker_active_contexts{scope}` - Should decrease to 0 before shutdown
- `browser_hive_worker_requests_total{scope}` - Total processed

## Timing Breakdown

### Worker Shutdown Timeline

```
t=0s:     SIGTERM → is_ready=false, cancel token
t=0-2s:   Readiness probe starts failing
t=2-5s:   K8s removes pod from Service endpoints
t=5s:     preStop hook completes
t=0-60s:  Wait for active requests (check every 500ms)
t=60s:    SIGKILL (force termination)
```

### Request During Shutdown

```
t=0s:     Client sends request to Coordinator
t=0s:     Coordinator selects Worker A (healthy in cache)
t=1s:     Worker A receives SIGTERM → is_ready=false
t=1.1s:   Worker A receives gRPC request
t=1.1s:   Worker A returns TERMINATING immediately
t=1.2s:   Coordinator receives TERMINATING
t=1.2s:   Coordinator excludes Worker A
t=1.3s:   Coordinator selects Worker B from healthy set
t=1.4s:   Worker B processes request normally
t=15s:    Worker B returns success
t=15s:    Coordinator forwards to client
```

## Troubleshooting

### Request fails with TERMINATING

**Symptom**: Client receives ERROR_CODE_TERMINATING

**Causes**:
1. All workers in scope are terminating
2. K8s rolling update with insufficient healthy replicas
3. Timeout too short for retry

**Solutions**:
- Ensure `replicas >= 2` in Deployment
- Use `maxUnavailable: 1` in RollingUpdate strategy
- Client should retry immediately (coordinator handles selection)

### Worker takes 60s to shutdown

**Symptom**: Worker logs "SIGKILL sent after 60s"

**Causes**:
1. Active requests taking longer than 60s
2. Blocking operation doesn't respect cancellation
3. Browser context not releasing

**Solutions**:
- Check request timeout configuration
- Verify wait strategies respect cancellation token
- Review browser context lifecycle settings

### Coordinator doesn't retry

**Symptom**: Client gets TERMINATING without retry

**Causes**:
1. No healthy workers available
2. Remaining time < 10s
3. Max attempts (3) exhausted

**Solutions**:
- Check health cache logs: "No healthy workers available for retry"
- Increase worker replicas
- Check K8s Service endpoints
- Verify readiness probe configuration

## Implementation Files

### Core Implementation
- `crates/tokio-cancellation-ext/` - Cancellation utilities
- `crates/worker/src/service.rs` - Worker graceful shutdown
- `crates/worker/src/lib.rs` - Worker signal handler
- `crates/coordinator/src/service.rs` - Coordinator with retry logic
- `crates/coordinator/src/main.rs` - Coordinator signal handler
- `crates/common/src/wait_strategy.rs` - Cancellation in wait loops

### Proto Definitions
- `crates/proto/proto/worker.proto` - ERROR_CODE_TERMINATING = 5006
- `crates/proto/proto/coordinator.proto` - ERROR_CODE_TERMINATING = 5006

### Documentation
- `ERROR_HANDLING.md` - Error code documentation
- `K8S_DEPLOYMENT.md` - Kubernetes deployment guide
- `LOCAL_DEV.md` - Local development setup

## Future Improvements

1. **Health check caching improvements**
   - Exponential backoff for failed health checks
   - Separate cache for each scope

2. **Retry strategy enhancements**
   - Configurable retry attempts
   - Custom retry timeout threshold
   - Circuit breaker for problematic workers

3. **Observability**
   - Metrics for TERMINATING responses
   - Metrics for retry attempts
   - Distributed tracing for retry flow

4. **Testing**
   - Integration tests for graceful shutdown
   - Chaos testing with random pod terminations
   - Load testing during rolling updates
