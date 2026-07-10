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
| `browser_hive_worker_available_slots` | Gauge | Free capacity: `total_slots - active_contexts` |
| `browser_hive_worker_requests_total` | Counter | Total scraping requests received |
| `browser_hive_worker_requests_failed` | Counter | Failed requests: any response with a 5xxx `error_code` (browser error, network error, context creation failed, terminating) plus gRPC-level infrastructure errors. 4xxx codes (invalid URL, session not found, selector not found, skip selector) are client-side conditions and are NOT counted - see [ERROR_HANDLING.md](ERROR_HANDLING.md) |

**Freshness**: pool gauges (`total_slots`, `total_contexts`, `active_contexts`, `available_slots`) are refreshed from live browser pool state on every Prometheus scrape, so they always reflect the current pool regardless of which code path changed it (requests, lifecycle recycling, pool recreation). Counters are incremented in the request path.

**Capacity model**: each worker runs `WORKER_MAX_CONTEXTS` CDP browser contexts (default: 3), one tab per context, and each context processes exactly one request at a time. So the concurrency unit is a **context**, not a worker pod:

```
cluster capacity = sum(total_slots)   = pods × WORKER_MAX_CONTEXTS
cluster load     = sum(active_contexts)
utilization      = sum(active_contexts) / sum(total_slots)
```

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
  cooldownPeriod: 300           # conservative scale-down (see caveats below)
  triggers:
  - type: prometheus
    metadata:
      serverAddress: http://prometheus.monitoring:9090
      # Busy contexts for this scope across all pods
      query: sum(browser_hive_worker_active_contexts{scope="my_scope"})
      # Target busy contexts per pod = WORKER_MAX_CONTEXTS × target utilization
      # e.g. 3 contexts/pod × 0.8 = 2.4
      threshold: "2.4"
```

With `WORKER_MAX_CONTEXTS=3` and `threshold: "2.4"`, KEDA keeps average utilization around 80%: 7 busy contexts → `ceil(7 / 2.4) = 3` pods.

For dashboards and alerting, the utilization ratio is more readable:

```promql
sum(browser_hive_worker_active_contexts{scope="my_scope"})
/
sum(browser_hive_worker_total_slots{scope="my_scope"})
```

(Avoid dividing by `total_contexts` - it is the current pool size, not capacity, and equals the busy count when contexts are created on demand.)

### Scale-down caveats

- **Sessions are lost on pod termination.** Session IDs have the form `{worker_id}:{context_id}` - when KEDA removes a pod, every session living on it dies. Clients get `ERROR_CODE_SESSION_NOT_FOUND` (4002) on the next request and must start a new session. If clients rely on long-lived sessions (login flows), scale down conservatively: increase `cooldownPeriod` and/or set an HPA `scaleDown` stabilization window via `spec.advanced.horizontalPodAutoscalerConfig.behavior`.
- **In-flight requests are safe.** Graceful shutdown (SIGTERM → wait for active requests, coordinator retries on healthy workers) handles pod removal cleanly - see [GRACEFUL_SHUTDOWN.md](GRACEFUL_SHUTDOWN.md). Ensure `terminationGracePeriodSeconds` covers your longest request timeout.
- **Prometheus must scrape the workers.** The metrics port (9090) must be reachable by Prometheus (ServiceMonitor / scrape annotations) - see [K8S_DEPLOYMENT.md](K8S_DEPLOYMENT.md) for the Service definition.
