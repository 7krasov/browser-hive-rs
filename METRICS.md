# Metrics & Monitoring

Browser Hive workers expose Prometheus metrics over HTTP on port `9090` at `/metrics`.

```bash
# Local quick check
curl http://localhost:9090/metrics
```

Implementation: `crates/worker/src/metrics.rs`.

## Worker Metrics

All metrics carry a `scope` label (e.g. `{scope="local_dev"}`).

| Metric | Type | Description |
|--------|------|-------------|
| `browser_hive_worker_total_slots` | Gauge | Configured capacity (`WORKER_MAX_CONTEXTS`) - the maximum number of concurrent requests the worker can handle |
| `browser_hive_worker_total_contexts` | Gauge | Browser contexts currently in the pool. Contexts are created on demand, so this can be lower than `total_slots` |
| `browser_hive_worker_active_contexts` | Gauge | Busy contexts (= requests currently being processed) |
| `browser_hive_worker_claimed_contexts` | Gauge | Slots that cannot be handed to a new client. Identical to `active_contexts` in `always_new`/`reusable`; in `dedicated` it also counts contexts owned by sessions that are idle between requests |
| `browser_hive_worker_available_slots` | Gauge | Free capacity: `total_slots - claimed_contexts` |
| `browser_hive_worker_requests_total` | Counter | Total scraping requests received |
| `browser_hive_worker_requests_failed` | Counter | Failed requests: any response with a 5xxx `error_code` (browser error, network error, context creation failed, terminating) plus gRPC-level infrastructure errors. 4xxx codes (invalid URL, session not found, selector not found, skip selector) are client-side conditions and are NOT counted - see [ERROR_HANDLING.md](ERROR_HANDLING.md) |
| `browser_hive_worker_request_duration_seconds` | Histogram | End-to-end `scrape_page` duration in seconds (observed on every return path, including early returns). Buckets: 0.1, 0.25, 0.5, 1, 2, 3, 5, 8, 13, 21, 34, 60. Exposes `_bucket`, `_sum`, `_count` |

**Freshness**: pool gauges (`total_slots`, `total_contexts`, `active_contexts`, `claimed_contexts`, `available_slots`) are refreshed from live browser pool state on every Prometheus scrape, so they always reflect the current pool regardless of which code path changed it (requests, lifecycle recycling, pool recreation). Counters are incremented in the request path.

**Capacity model**: each worker runs `WORKER_MAX_CONTEXTS` CDP browser contexts (default: 3), one tab per context, and each context processes exactly one request at a time. So the concurrency unit is a **context**, not a worker pod:

```
cluster capacity = sum(total_slots)   = pods × WORKER_MAX_CONTEXTS
cluster load     = sum(claimed_contexts)
utilization      = sum(claimed_contexts) / sum(total_slots)
```

⚠️ **Autoscale on `claimed_contexts`, not `active_contexts`.** They are the same number in
`always_new` and `reusable`, so nothing changes for those scopes. In `dedicated`
(`WORKER_SESSION_MODE=dedicated`) a slot is held by a session for as long as the client keeps
coming back, and between two of its requests that context is *not busy* — a trigger reading
`active/total_slots` would see an idle worker whose every slot is already spoken for and would
scale down under a full pool. The examples below use `active_contexts` where they are measuring
request concurrency (a latency/throughput question) and `claimed_contexts` where they are
measuring capacity pressure.

## Sizing `maxReplicaCount` per scope

To pick the maximum replicas a scope needs, look at the **peak concurrent load over
time**, not a single aggregate number. The concurrency unit is a context, so:

```
maxReplicas(scope) = ceil( peak_busy_contexts / WORKER_MAX_CONTEXTS x (1 + headroom) )
```

Two ways to measure `peak_busy_contexts`, with different robustness:

1. **Gauge (simple, sampling-sensitive)** - the instantaneous busy-context count.
   `active_contexts` is only computed at scrape time, so requests shorter than the
   scrape interval can be missed and the peak under-counted:
   ```promql
   max_over_time( sum by (scope) (browser_hive_worker_active_contexts) [1d:] )
   ```
   Keep the worker scrape interval small (~10-15s) if you rely on this.

2. **Little's Law (robust, from the histogram)** - average in-flight requests derived
   from throughput x latency, immune to scrape sampling because it integrates the `_sum`
   counter:
   ```promql
   sum by (scope) (rate(browser_hive_worker_request_duration_seconds_sum[5m]))
   ```
   This equals the average number of concurrently-running requests over the window. Use
   its `max_over_time(...[1d:])` for the busy peak. Pair with p95 latency for SLOs:
   ```promql
   histogram_quantile(0.95,
     sum by (scope, le) (rate(browser_hive_worker_request_duration_seconds_bucket[5m])))
   ```

The gauge and Little's Law numbers should agree; if the gauge peak is consistently lower,
your scrape interval is too coarse to catch the true peak.

## Coordinator Stats (gRPC)

The coordinator can query per-worker stats via the gRPC `GetStats` endpoint (`WorkerStatsResponse`): total/available slots, active requests, contexts created/recycled, success rate. This is used internally for load balancing and is independent of the Prometheus endpoint.

## Autoscaling with KEDA

CPU/RAM-based autoscaling does not work well here: a worker can have plenty of CPU/RAM headroom while all its browser contexts are busy and unable to accept new requests. Scale on **slot utilization** instead.

### Recommended trigger

KEDA's Prometheus scaler computes `desiredReplicas = ceil(metricValue / threshold)`, so use the **absolute number of busy contexts** as the query and set the threshold to the per-pod capacity you want to target:

```yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: browser-hive-worker
  namespace: browser-hive
spec:
  scaleTargetRef:
    name: browser-hive-worker   # worker Deployment
  minReplicaCount: 1
  maxReplicaCount: 10
  # cooldownPeriod only applies to scale-to-zero (minReplicaCount: 0) - it is
  # inert while min replicas >= 1. Anti-flapping for N->M scale-down is the
  # HPA stabilization window below (default 300s).
  advanced:
    horizontalPodAutoscalerConfig:
      behavior:
        # scaleUp is intentionally left at HPA defaults (immediate, 0s window):
        # a delayed scale-up means all contexts are busy and clients get errors
        # while new pods start. Only scale-down needs damping.
        scaleDown:
          # Scale down only to the max replica count needed during the last
          # window. Use 600+ when workers hold sessions (see caveats below).
          stabilizationWindowSeconds: 600
  triggers:
  - type: prometheus
    metadata:
      serverAddress: http://prometheus.monitoring:9090
      # Claimed slots for this scope across all pods. Equals busy contexts in
      # always_new/reusable, and additionally counts idle-but-owned session
      # contexts in dedicated - which is the only correct signal there.
      query: sum(browser_hive_worker_claimed_contexts{scope="my_scope"})
      # Target claimed contexts per pod = WORKER_MAX_CONTEXTS × target utilization
      # e.g. 3 contexts/pod × 0.8 = 2.4
      threshold: "2.4"
```

With `WORKER_MAX_CONTEXTS=3` and `threshold: "2.4"`, KEDA keeps average utilization around 80%: 7 claimed contexts → `ceil(7 / 2.4) = 3` pods.

For dashboards and alerting, the utilization ratio is more readable:

```promql
sum(browser_hive_worker_claimed_contexts{scope="my_scope"})
/
sum(browser_hive_worker_total_slots{scope="my_scope"})
```

(Avoid dividing by `total_contexts` - it is the current pool size, not capacity, and equals the busy count when contexts are created on demand.)

### Scale-down caveats

- **Sessions are lost on pod termination.** Session IDs have the form `{worker_id}:{context_id}` - when KEDA removes a pod, every session living on it dies. Clients get `ERROR_CODE_SESSION_NOT_FOUND` (4002) on the next request and must start a new session. Only `WORKER_SESSION_MODE=dedicated` has sessions at all; in the other modes nothing is lost. For a `dedicated` scope, scale down conservatively: increase `scaleDown.stabilizationWindowSeconds` (via `spec.advanced.horizontalPodAutoscalerConfig.behavior`, HPA default 300s). KEDA's `cooldownPeriod` does not help here - it only applies to scale-to-zero.
- **In-flight requests are safe.** Graceful shutdown (SIGTERM → wait for active requests, coordinator retries on healthy workers) handles pod removal cleanly - see [GRACEFUL_SHUTDOWN.md](GRACEFUL_SHUTDOWN.md). Ensure `terminationGracePeriodSeconds` covers your longest request timeout.
- **Prometheus must scrape the workers.** The metrics port (9090) must be reachable by Prometheus (ServiceMonitor / scrape annotations) - see [K8S_DEPLOYMENT.md](K8S_DEPLOYMENT.md) for the Service definition.
