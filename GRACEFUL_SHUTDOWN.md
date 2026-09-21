# Graceful Shutdown Implementation

This document describes the graceful shutdown implementation for Browser Hive's distributed web scraping system.

## Overview

Browser Hive implements graceful shutdown to handle POSIX signals (SIGTERM, SIGINT) during pod termination, ensuring:
- Every in-flight request gets an answer (`ERROR_CODE_TERMINATING`) rather than a dropped connection
- Automatic retry on healthy workers, by the coordinator

⚠️ **It does not drain.** SIGTERM cancels in-flight requests at once instead of letting them
finish, so a rollout, a KEDA scale-down or a spot preemption costs every request in flight on the
pod; the coordinator retries them from scratch only while ≥ 10 s of the client's deadline remain.
The fix is an open item in TODO.md ("Shutdown does not drain in-flight requests").

## Architecture

### Worker Graceful Shutdown

When a Worker receives SIGTERM:

1. **Immediate Response** (t=0ms)
   - Sets `is_ready = false` → K8s readiness probe fails
   - Calls `cancellation_token.cancel()`
   - New requests immediately return `ERROR_CODE_TERMINATING`

2. **Active Request Handling**
   - Blocking operations wrapped in `spawn_blocking` + `tokio::select!`
   - When the token is cancelled, the in-flight request answers TERMINATING immediately — it is
     aborted, not finished
   - Its tab is closed **detached** on the blocking pool (`close_tab_detached`); the context stays
     in the pool. Never inline: `Tab::close` can wait up to an hour on an unresponsive tab
   - The blocking thread may stay parked inside headless_chrome until SIGKILL (acceptable)

3. **Graceful Wait**
   - Waits for the request handlers to return — which, since they were cancelled in step 1, is
     normally immediate
   - Polls every 500ms and logs remaining count
   - There is no internal timeout; the hard limit is K8s `terminationGracePeriodSeconds` (SIGKILL)

4. **K8s Integration**
   - The `preStop` hook runs **before** SIGTERM is sent. During its sleep the worker still reports
     itself healthy and keeps receiving requests, which step 1 then cancels
   - Readiness probe removes pod from Service endpoints. This is **not** what stops routing: the
     coordinator discovers pods via the K8s API and connects to pod IPs, so it never consults the
     Service. Routing stops because `HealthCheck` returns `healthy = false` from SIGTERM on, which
     the coordinator's health monitor sees within ~1s
   - `terminationGracePeriodSeconds: 60` counts from the start of `preStop`

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
   - No internal timeout: waits until all complete; K8s sends SIGKILL after `terminationGracePeriodSeconds` (60s)

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
- Readiness/liveness probes (TCP)
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
      tcpSocket:            # not grpc: - no grpc.health.v1 service is implemented
        port: 50052
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

Workers expose Prometheus metrics on port 9090 (see [METRICS.md](METRICS.md)). During shutdown:
- `browser_hive_worker_active_contexts{scope}` - Should decrease to 0 before shutdown
- `browser_hive_worker_requests_total{scope}` - Total processed

The coordinator counts the shutdown side (port 9090 of the coordinator, see METRICS.md):
- `browser_hive_coordinator_requests_retried_total{scope, reason="terminating"}` - attempts re-sent to
  another pod after a TERMINATING answer
- `browser_hive_coordinator_requests_rejected_total{scope, reason="terminating"}` - requests that
  still ended in TERMINATING (no retry possible)

### Following one request across the retry

Both services put `ray_id` on the request span, and the coordinator re-records `worker_id` on a
TERMINATING retry, so `| json | span_ray_id="..."` in Loki shows every attempt of one request.

## Timing Breakdown

### Worker Shutdown Timeline

```
t=0s:     Pod deleted → preStop hook starts (sleep 5); worker still healthy, still routed to
t=5s:     preStop done → SIGTERM → is_ready=false, cancel token;
          in-flight requests answer TERMINATING, the coordinator retries them elsewhere
t=5-7s:   Coordinator's health monitor (1s poll) sees healthy=false, stops routing here;
          readiness probe fails (cosmetic - nothing routes via Service)
t=5s+:    Wait for the cancelled handlers to return (check every 500ms), then exit
t=60s:    SIGKILL if still running
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
- Check worker discovery logs: pods must be labelled `app=browser-hive-worker` **and**
  `scope=<name>`, be in phase `Running`, and answer `GetStats` on `pod_ip:50052`
- Check the workers' own `HealthCheck` responses (the readiness probe is not what routing
  reads)

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

3. **Testing**
   - Integration tests for graceful shutdown
   - Chaos testing with random pod terminations
   - Load testing during rolling updates
