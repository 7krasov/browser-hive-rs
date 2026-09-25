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
| `browser_hive_worker_requests_failed` | Counter | Failed requests: any response with a 5xxx `error_code` (browser error, network error, context creation failed, terminating, proxy error, page load incomplete) plus gRPC-level infrastructure errors. 4xxx codes (invalid URL, session not found, session busy, selector not found, skip selector) are client-side conditions and are NOT counted. **`CAPACITY_EXHAUSTED` (5008) is the one 5xxx code that is also not counted** — a full pool is the pool working as configured under load, and counting it would make a busy scope indistinguishable from a breaking one (it would also drag `success_rate` down). That refused demand is counted on the coordinator as `requests_rejected_total{reason="no_slots"}` - see [ERROR_HANDLING.md](ERROR_HANDLING.md) |
| `browser_hive_worker_request_duration_seconds` | Histogram | End-to-end `scrape_page` duration in seconds (observed on every return path, including early returns). Buckets: 0.1, 0.25, 0.5, 1, 2, 3, 5, 8, 13, 21, 34, 60, 90, 120, 180, 320 (the last four since 0.38.0, see below). Exposes `_bucket`, `_sum`, `_count` |
| `browser_hive_worker_browser_processes{scope, type}` | Gauge | Browser processes by Chromium process type (see [Browser resources](#browser-resources)) |
| `browser_hive_worker_browser_process_pss_bytes{scope, type}` | Gauge | Proportional set size of those processes, summed per type |
| `browser_hive_worker_browser_process_max_pss_bytes{scope, type}` | Gauge | PSS of the largest single process of each type |
| `browser_hive_worker_browser_targets{scope, type}` | Gauge | CDP targets the browser reports, by target type |
| `browser_hive_worker_browser_contexts` | Gauge | CDP browser contexts that exist in the browser, the default context excluded |
| `browser_hive_worker_process_resident_memory_bytes` | Gauge | RSS of the worker process itself, the browser excluded |
| `browser_hive_worker_process_threads` | Gauge | Threads of the worker process |
| `browser_hive_worker_iframes_total{scope, page_site, iframe_host}` | Counter | Cross-site iframes loaded by scraped pages, each frame counted once (see [Third-party hosts](#third-party-hosts-and-iframes)) |
| `browser_hive_worker_third_party_requests_total{scope, page_site, request_host}` | Counter | Cross-site sub-resource loads that were not blocked |
| `browser_hive_worker_third_party_requests_blocked_total{scope, page_site, request_host}` | Counter | Loads dropped by the scope's blocked-URL list, any host |

**Freshness**: pool gauges (`total_slots`, `total_contexts`, `active_contexts`, `claimed_contexts`, `available_slots`) are refreshed from live browser pool state on every Prometheus scrape, so they always reflect the current pool regardless of which code path changed it (requests, lifecycle recycling, pool recreation). The browser resource gauges are collected on every scrape too. Counters are incremented in the request path.

### Browser resources

A worker's memory limit is spent by the browser's processes, not by the worker, and the container's memory figure is only their sum. It cannot tell one bloated renderer from many small ones (out-of-process iframes), or from something that outlived the page it served. These gauges answer that without `kubectl exec`. Implementation: `crates/worker/src/browser_resources.rs`.

- **Processes** are the worker's descendants in `/proc`, grouped by Chromium's `--type=` flag. The label set is closed:
  - `browser`, `renderer`, `extension` (a renderer with `--extension-process`), `gpu`, `zygote`;
  - `network` and `storage` (utility processes, told apart by `--utility-sub-type=`);
  - `utility`, `other`.
- **`browser` counts every process with no `--type=`**, so launcher wrappers land there next to the real main process: production Brave shows **5 per browser**, of which one holds memory ("Largest single process" tells them apart). A never-reaped browser — for example, the one a pool recreation replaced — shows as a **step of one browser's worth** (+5 there), not as "above 1".
- **Blind spot:** a process reparented away from the worker is not seen, which can happen only when the worker is not the container's PID 1.
- **Memory is PSS** (`smaps_rollup`), not RSS. Renderers are forked from a zygote and share most of their pages, so summed RSS overstates the total several times over. PSS splits each shared page between the processes that map it, so the per-type sums add up.
- **Targets** come from `Target.getTargets` over a browser-level CDP connection the metrics endpoint keeps for itself. It is separate from the one context creation uses, so a slow scrape never blocks a request. The label set is closed:
  - `page`, `iframe`, `worker`, `shared_worker`, `service_worker`;
  - `browser_ui`, the browser's own internal pages;
  - `other`.
- **A cross-site iframe appears as an `iframe` target.** This was checked against a real Chrome. `page` includes the browser's initial blank tab.
- **An unreadable source leaves its gauges absent, not zero.** That covers:
  - no `/proc` (any non-Linux host);
  - a probe error;
  - a probe over its 3 s budget, after which the next scrape skips the probe while the stuck one is still running.

  Within a readable source every type is written, zero included.
- **Cost per scrape:**
  - one `smaps_rollup` read per browser process, which walks that process's page tables;
  - two CDP round-trips.

```promql
# Where the memory goes, by process type
sum by (type) (browser_hive_worker_browser_process_pss_bytes{scope="<scope>"})

# Out-of-process iframes per page: high values point at site isolation, not at tab age
browser_hive_worker_browser_targets{type="iframe"} / browser_hive_worker_browser_targets{type="page"}

# Contexts the browser still holds after the pool let them go (isolated scopes only;
# a `shared` scope creates no CDP contexts of its own)
browser_hive_worker_browser_contexts - browser_hive_worker_total_contexts
```

### Third-party hosts and iframes

Which foreign hosts the scraped pages load, ranked, to choose what `BlockedUrlsMiddleware` should cut. Implementation: `crates/worker/src/third_party.rs`.

**Two sources, because neither sees everything.** With site isolation on (the headless default) a cross-site iframe runs in its own renderer. The page's CDP session neither reports nor blocks anything inside it, and never blocks the frame's own document.

| Metric | Source | Sees | Misses |
|---|---|---|---|
| `iframes_total` | a background sampler calls `Target.getTargets` every second over its own browser-level CDP connection | every cross-site frame, nested ones included | frames that live under a second (a frame lives only while its page is loaded); same-site frames |
| `third_party_requests_total` | the page session's `Network` events (the response observer's listener): `requestWillBeSent`, then `loadingFinished` or a `loadingFailed` not caused by the list | the main page and frames running in its process | everything inside a cross-site iframe; `Document` loads (the page itself, and frame documents, which `iframes_total` counts); loads still pending when the request ends |
| `third_party_requests_blocked_total` | same listener, `loadingFailed` with `blockedReason: inspector` | loads dropped by the list **or** by a blocked resource type, any host, same-site included | same as above |
| `requests_blocked_by_type_total{page_site, resource_type}` | same listener: a blocked load whose type is in the scope's `BlockedResourceTypesMiddleware` list | loads dropped by type, per site | same as above. A load matched by both the URL list and a blocked type is attributed to the type — Chrome reports both kinds of block identically (`blockedReason: inspector`) |

**Labels.**
- **Hosts are full hosts**, not registrable domains: a block pattern is often written for one subdomain.
- **Cross-site means the registrable domains differ** (public suffix list; an IP or `localhost` is its own site).
- **`page_site` is the host the client requested**, even after a redirect to another site, so a site stays joinable with the client's own source list.
- **An iframe's `page_site`** comes from its target's `browserContextId`: each pool context remembers the host of the last request it served. The tab keeps that page loaded afterwards, so a frame appearing between requests still belongs to it. It is `unknown` when no pool context matches, which includes every frame of a `shared` scope, since all its tabs live in the default context.
- **A frame is keyed by target id and host**, so a frame that navigates to another host is counted again.

**Cardinality.** Client input feeds both `page_site` and the hosts, so the `(page_site, host)` combinations are capped **per metric, per worker process** by `WORKER_THIRD_PARTY_METRICS_MAX_SERIES` (default 2000). Past the cap a combination is counted as `page_site="other"`, host `"other"`: the total stays correct and only the breakdown is lost. Nothing is evicted, since a counter that disappears and comes back breaks `increase()`.
- ⚠️ **Worst case is `4 × cap × pods` series** (for `requests_blocked_by_type_total` the host is replaced by `resource_type`, so its real ceiling is `sites × configured types`, far below the cap), and every rollout creates the set again under new pod names. A container restart (an OOMKill included) keeps the pod name and adds no series. Series that only exist in the worst case are never created: the real number is the combinations actually seen.
- **To see what these metrics actually cost**, use Prometheus' **Status → TSDB Status** page: it breaks head series down by metric and label. `prometheus_tsdb_head_series` is only a total and cannot attribute growth.
- **An `other` row in the dashboard tables** means a worker reached the cap. Nothing is logged.
- Keeping `page_site` over dropping it was a deliberate choice for 300–800 scraped sites, accepting this cost (2026-09-15). Watch `prometheus_tsdb_head_series` after a deploy.

`increase()` misses the first increments of a series that appeared inside the range, so rare hosts are undercounted. The ranking of frequent hosts is unaffected.

```promql
# Hosts to consider blocking: sub-resource loads over the last day, across all sites
topk(50, sum by (request_host) (increase(browser_hive_worker_third_party_requests_total{scope="<scope>"}[1d])))

# Iframe hosts, same idea (the block list cannot reach these yet)
topk(50, sum by (iframe_host) (increase(browser_hive_worker_iframes_total{scope="<scope>"}[1d])))

# Is type blocking in force, and what it cuts (only types listed in WORKER_BLOCKED_RESOURCE_TYPES appear)
sum by (resource_type) (increase(browser_hive_worker_requests_blocked_by_type_total{scope="<scope>"}[1d]))

# How many sites load a host: a host used by one site is cheaper to reason about
count by (request_host) (sum by (page_site, request_host) (increase(browser_hive_worker_third_party_requests_total{scope="<scope>"}[1d])) > 0)
```

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

**Buckets above 60 s (since 0.38.0).** Both request-duration histograms, worker and
coordinator, used to end at 60, so every slower request fell into `+Inf` and
`histogram_quantile` reported it as exactly 60. Dashboards read that as a 60 s cap that
does not exist: the only hard bound is the 320 s gRPC server timeout. The buckets 90, 120,
180 and 320 show where that tail goes. Existing queries keep working: `le="60"` and every
lower bound are unchanged, `_sum`/`_count` are unaffected, and quantiles above 60 s become
more precise instead of being clamped. One caveat: a range that spans the upgrade has no
data for the new `le` series before it, so `increase(...{le="90"}[7d])` understates until
the window is past the rollout. To count the tail with no dependency on the new buckets,
use `_bucket{le="+Inf"} − _bucket{le="60"}`.

## Coordinator Metrics

The coordinator exposes Prometheus metrics on port `9090` at `/metrics` too (implementation: `crates/coordinator/src/metrics.rs`). Controlled by `COORDINATOR_ENABLE_METRICS` (default `true`) and `COORDINATOR_METRICS_PORT` (default `9090`).

**Why these exist at all:** there is no queue anywhere in Browser Hive. When no worker has a free slot, the coordinator answers `ERROR_CODE_NO_WORKERS_AVAILABLE` (5001) immediately and the request is gone. That rejection never touches a worker, so it leaves **no trace in any `browser_hive_worker_*` series** — a fleet at 100% utilization and a fleet turning away half its traffic are indistinguishable from the worker side. Worker metrics measure supply; these measure the demand that supply failed to meet.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `browser_hive_coordinator_requests_total` | Counter | `scope` | Scrape requests received, counted on completion (including futures dropped mid-request) |
| `browser_hive_coordinator_requests_rejected_total` | Counter | `scope`, `reason` | Requests refused **without reaching a worker**. See the reason table below |
| `browser_hive_coordinator_request_duration_seconds` | Histogram | `scope` | End-to-end coordinator duration: worker time plus routing, retries and the fresh-stats round trip. Buckets: 0.005 … 60, 90, 120, 180, 320 (the last four since 0.38.0, see below) |
| `browser_hive_coordinator_scope_workers_total` | Gauge | `scope` | Worker pods discovered per scope, as the coordinator sees them |
| `browser_hive_coordinator_scope_workers_healthy` | Gauge | `scope` | Of those, the pods that passed the last health check |
| `browser_hive_coordinator_scope_available_slots` | Gauge | `scope` | Free slots per scope **as routing sees them**: the discovery cache corrected by the requests this coordinator has dispatched since (`coordinator/src/in_flight.rs`) |

**`reason` values**, split by what they mean for action:

| Reason | Capacity? | Meaning |
|---|---|---|
| `no_slots` | **yes** | Workers exist, every slot is taken (confirmed against fresh worker stats, so not a stale cache). **The "add replicas" signal.** |
| `no_workers` | **yes** | The scope has no pods, or none that routing could select (all unhealthy / scaled to zero) |
| `scope_unavailable` | no | The scope's pods exist but **none is currently routable** — a pod restart, not a capacity problem. Adding replicas does not shorten it; it clears itself within ~10-20 s. A **sustained** rate here means pods are crash-looping |
| `scope_not_found` | no | No pod in the cluster carries this scope name — label or configuration mismatch |
| `session_not_found` | no | Client sent an expired `session_id`, or its worker is gone |
| `terminating` | no | The coordinator itself is shutting down |
| `worker_unreachable` | no | Routing picked a worker but the gRPC connection failed, or the RPC broke mid-request because the pod died. A **sustained** rate means pods are dying under load, not a client problem |

Notes:

- **Gauges are refreshed on every scrape** (same rule as the worker's pool gauges), so they cannot drift when discovery or the health monitor changes state. They deliberately show the **coordinator's own view** — that is what routing decides on. For `scope_available_slots` that view is the discovery cache corrected by in-flight requests, so it tracks the workers' own `available_slots` closely; a gap that **outlasts one discovery round (10 s)** means the in-flight count has leaked and routing believes pods are busier than they are — the same symptom (`no_slots` at low utilisation) the correction exists to remove. Because both are snapshot gauges, compare them over a window, not sample by sample.
- **Counters are recorded by an RAII guard**, so a request whose future is dropped (client disconnect, gRPC deadline) is still counted.
- `scope_unavailable` keeps the real scope name: it is only reached after the name matched a scope discovery found in the cluster, so its cardinality is bounded by the deployments rather than by client input. The `scope` label of a `scope_not_found` rejection is reported as `unknown`. It is the one label value fed from client input, and an unknown scope is by definition not from the configured set — collapsing it keeps a buggy client from minting unbounded time series. The real name is in the logs (`span_scope`).
- Every `reason` series is pre-created per discovered scope, so `rate()` over a rejection that has not happened yet returns 0 rather than no data.
- Worker errors (selector not found, timeouts, browser errors) are **not** rejections: those requests did reach a worker and are already counted by `browser_hive_worker_requests_failed`.
- `no_slots` is recorded in **two** places: when the coordinator's own check found every worker full, and when the chosen worker answered `CAPACITY_EXHAUSTED` (5008) and no other pod had room. The second case is the stale-cache race, and before 5008 existed it was recorded nowhere at all — it left the worker as a "failed request" and the coordinator as a normal one.

### `browser_hive_coordinator_requests_retried_total{scope, reason}`

Attempts re-sent to a **different** worker (not requests — one request can be retried more than
once). `reason` is `terminating`, `no_slots` or `worker_unreachable`.

This counter exists because a **successful** retry is otherwise invisible: the client got a normal
response, nothing was rejected, and the first worker's refusal is not a failure either. A scope that
only stays healthy because half its requests are retried looks identical to a comfortable one.

- `reason="no_slots"` rising is the **early warning** for capacity. `requests_rejected_total{reason="no_slots"}` is the same problem after it became visible to clients — by then requests are already being lost.
- `reason="terminating"` should stay near zero now that workers drain on SIGTERM (GRACEFUL_SHUTDOWN.md): it counts requests that arrived after the signal, or outlived the drain timeout.
- `reason="worker_unreachable"` is a pod that died with requests in flight — killed mid-drain (spot preemption), OOM-killed or crashed. Rising outside a deploy window, it is the request-side trace of OOM kills.

```promql
# Retries as a share of traffic, per scope — watch this before rejections appear
sum by (scope) (rate(browser_hive_coordinator_requests_retried_total{reason="no_slots"}[5m]))
/ clamp_min(sum by (scope) (rate(browser_hive_coordinator_requests_total[5m])), 0.0001)
```

⚠️ Capacity and an unreachable worker get **one** retry and TERMINATING gets up to three, on purpose: retrying hard against a
scope that is already at its limit turns it into a self-amplifying load generator. See
[ERROR_HANDLING.md](ERROR_HANDLING.md#retrying-on-another-worker).

Useful queries:

```promql
# Demand refused for lack of capacity, per scope (the actionable signal)
sum by (scope) (rate(browser_hive_coordinator_requests_rejected_total{reason=~"no_slots|no_workers"}[5m]))

# As a share of all traffic
sum by (scope) (rate(browser_hive_coordinator_requests_rejected_total{reason=~"no_slots|no_workers"}[5m]))
/ clamp_min(sum by (scope) (rate(browser_hive_coordinator_requests_total[5m])), 0.0001)

# Requests actually lost over the last hour
sum by (scope) (increase(browser_hive_coordinator_requests_rejected_total{reason=~"no_slots|no_workers"}[1h]))
```

The Grafana "Refused demand (coordinator)" row in `ops/grafana/browser-hive-dashboard.json` renders all of this.

⚠️ **Prometheus must scrape the coordinator pod as well.** The worker `PodMonitor` selects `app: browser-hive-worker` and will not match it; the coordinator needs its own scrape target on a container port named `metrics`.

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

### Choosing the threshold, and what it cannot do

`threshold` is **per-pod capacity, in contexts** — it must be recomputed whenever `WORKER_MAX_CONTEXTS` changes. It is not a portable number, so **never copy a threshold between environments that run different `WORKER_MAX_CONTEXTS`**: carrying `0.7` (correct for a 1-context dev pod) over to a 10-context production pod silently turns a 70% utilization target into 7%, and a single busy pod then asks for 15 replicas.

| `WORKER_MAX_CONTEXTS` | target utilization | `threshold` |
|---|---|---|
| 1 | 0.7 | `"0.7"` |
| 5 | 0.7 | `"3.5"` |
| 10 | 0.7 | `"7"` |

Fractional thresholds are fine (KEDA converts them to milli-quantities).

**Lower threshold = earlier scale-up = more idle headroom.** The free slots the target buys are `(1 − utilization) × WORKER_MAX_CONTEXTS × replicas`. That is the whole effect; in particular the threshold does **not** control scale-down damping (`scaleDown.stabilizationWindowSeconds` does) and does **not** create a queue.

⚠️ **A threshold cannot absorb a burst.** By Little's Law free slots absorb *concurrency*, not rate: `spare_slots / mean_request_duration` is the extra arrival rate they cover, and only until a new pod is serving — Chrome launch, readiness probe and the coordinator's 10s discovery poll put that at tens of seconds. Buying meaningful burst protection by lowering the threshold means running permanently over-provisioned pods through a knob that expresses it indirectly. Use `minReplicaCount` for that instead, and treat the threshold as the steady-state growth target.

So when capacity rejections appear (`browser_hive_coordinator_requests_rejected_total{reason=~"no_slots|no_workers"}`, see above), the fix is **`minReplicaCount` / `maxReplicaCount`, not a lower threshold**. Without the coordinator metrics scraped there is no signal at all: rejected requests never reach a worker.

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
