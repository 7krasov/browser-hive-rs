# Graceful Shutdown Implementation

This document describes the graceful shutdown implementation for Browser Hive's distributed web scraping system.

## Overview

Browser Hive implements graceful shutdown to handle POSIX signals (SIGTERM, SIGINT) during pod termination, ensuring:
- Requests already in flight are allowed to **finish**, up to a configurable bound
- A request that arrives after the signal is answered `ERROR_CODE_TERMINATING` without being started,
  and the coordinator re-sends it to a healthy worker
- A worker killed before it finished (SIGKILL, e.g. a spot preemption) is retried once on another
  worker by the coordinator

## Architecture

### Worker Graceful Shutdown

Implemented in `crates/worker/src/shutdown.rs`. When a Worker receives SIGTERM:

1. **Stop taking work** (t=0)
   - Sets `is_ready = false` → `HealthCheck` answers `healthy = false`; the coordinator's health
     monitor (1 s poll) stops routing here
   - A request that still arrives is answered `ERROR_CODE_TERMINATING` at once — nothing was done,
     so the coordinator's retry costs the client nothing

2. **Drain** (t=0 … `WORKER_SHUTDOWN_DRAIN_TIMEOUT_SECS`, default 25 s)
   - Requests in flight run to completion; the remaining count is logged when it changes
   - At least 3 s pass even with nothing in flight, so the coordinator has seen `healthy = false`
     before the gRPC server closes (otherwise it would connect to a closed port for up to a health
     round)
   - A second SIGTERM / Ctrl+C ends the drain at once

3. **Cancel what is left** (drain timeout reached, or interrupted)
   - `cancellation_token.cancel()`: blocking operations are wrapped in `spawn_blocking` +
     `tokio::select!`, so each remaining request answers `TERMINATING` immediately; its tab is
     closed **detached** on the blocking pool (`close_tab_detached`) and the context stays in the
     pool. Never inline: `Tab::close` can wait up to an hour on an unresponsive tab
   - The blocking thread may stay parked inside headless_chrome until SIGKILL (acceptable)
   - The worker waits for those handlers to return, then the gRPC server closes

4. **K8s Integration**
   - Set `terminationGracePeriodSeconds` ≥ 3 s + drain timeout + a few seconds of margin. A drain
     timeout ≥ the longest request a scope serves means rollouts and scale-downs abort nothing
   - ⚠️ **The grace period is not always what the pod spec says.** On a GKE Spot preemption a
     non-system pod gets at most **15 s**, whatever `terminationGracePeriodSeconds` is (the node
     pool's `shutdownGracePeriodSeconds` can raise it to 120 s). The drain is then cut by SIGKILL;
     the requests still running lose their connection and the coordinator retries them as an
     unreachable worker (below), so a short real grace period costs no more than cancelling early
   - **No `preStop` sleep is needed for the worker.** Routing stops because `HealthCheck` turns
     unhealthy at SIGTERM, not because of Service endpoints (the coordinator discovers pods via the
     K8s API and connects to pod IPs). A `preStop` sleep only delays that, and on a spot preemption
     it spends part of the 15 s
   - `terminationGracePeriodSeconds` counts from the start of `preStop`, if there is one
   - `WORKER_SHUTDOWN_DRAIN_TIMEOUT_SECS=0` restores the old behaviour: cancel right after the 3 s
     pause

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
   - No internal timeout: waits until all complete; K8s sends SIGKILL after `terminationGracePeriodSeconds`

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

### ERROR_CODE_WORKER_UNREACHABLE (5002)

Returned by the coordinator when it could not connect to the chosen worker, or the connection broke
mid-request — most often a worker killed before its drain ended.

**Coordinator automatically** retries it **once** on another healthy worker (2 attempts in total),
under the same ≥ 10 s deadline guard, and never for a request carrying a `session_id` (the session
lived on the pod that is gone). Only one retry because a worker can also die *because of* the
request (a page that pushes the container over its memory limit), and every further attempt would
take down another pod.

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
  terminationGracePeriodSeconds: 360   # >= 3 s + drain timeout + margin
  containers:
  - name: worker
    env:
    - name: WORKER_SHUTDOWN_DRAIN_TIMEOUT_SECS
      value: "330"                      # >= the longest request (320 s)
    readinessProbe:
      tcpSocket:            # not grpc: - no grpc.health.v1 service is implemented
        port: 50052
      initialDelaySeconds: 5
      periodSeconds: 2
    # no preStop: routing stops on the HealthCheck answer, see above
```

## Monitoring

### Worker Logs

During graceful shutdown, worker logs:
```
WARN  Received SIGTERM signal
INFO  Worker marked unhealthy; taking no new requests, draining in-flight ones for up to 330s
INFO  Draining: 2 request(s) in flight, 329.9s left
INFO  Draining: 1 request(s) in flight, 322.4s left
INFO  All in-flight requests completed
INFO  Starting gRPC server shutdown
INFO  gRPC server shutdown complete
INFO  Worker shutdown complete
```

When the bound is reached: `WARN Drain timeout (…) reached with N request(s) still in flight;
cancelling them`. The drain bound in force is logged at startup (`Shutdown drain timeout: …`).

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
  another pod after a TERMINATING answer; with the drain in place this should be close to zero
  outside spot preemptions
- `browser_hive_coordinator_requests_retried_total{scope, reason="worker_unreachable"}` - attempts
  re-sent after a connect failure or a broken RPC — a pod killed mid-drain, or crashed
- `browser_hive_coordinator_requests_rejected_total{scope, reason="terminating"}` /
  `{reason="worker_unreachable"}` - requests that still ended that way (no retry possible)

### Following one request across the retry

Both services put `ray_id` on the request span, and the coordinator re-records `worker_id` on a
TERMINATING retry, so `| json | span_ray_id="..."` in Loki shows every attempt of one request.

## Timing Breakdown

### Worker Shutdown Timeline

```
t=0s:     Pod deleted → SIGTERM → is_ready=false; new requests answer TERMINATING
t=0-2s:   Coordinator's health monitor (1s poll) sees healthy=false, stops routing here;
          readiness probe fails (cosmetic - nothing routes via Service)
t=0s+:    In-flight requests keep running
t=3s+:    Server closes once nothing is in flight (never before 3s)
t=drain:  Whatever is still running is cancelled → TERMINATING → coordinator retries elsewhere
t=grace:  SIGKILL if still running (15s on a GKE Spot preemption) → connections break →
          coordinator retries once as WORKER_UNREACHABLE
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

### Worker takes long to shut down

**Expected** while requests are in flight: the worker waits for them up to
`WORKER_SHUTDOWN_DRAIN_TIMEOUT_SECS`. If pods are SIGKILLed before the drain ends
(`Worker shutdown complete` missing from the log), the grace period is shorter than 3 s + drain
timeout — or it was a spot preemption (15 s).

### Coordinator doesn't retry

**Symptom**: Client gets TERMINATING or WORKER_UNREACHABLE without retry

**Causes**:
1. No healthy workers available
2. Remaining time < 10s
3. Max attempts exhausted (3 for TERMINATING, 2 for WORKER_UNREACHABLE)
4. The request carried a `session_id` (never retried as WORKER_UNREACHABLE)

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
- `crates/worker/src/shutdown.rs` - Worker signal handler and drain
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
