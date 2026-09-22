# Grafana Dashboards

Import: Grafana → Dashboards → New → Import → upload the JSON (or paste it), then pick
your Prometheus datasource.

## `browser-hive-dashboard.json` - Load & Capacity per Scope

Per-scope view of concurrent load, capacity utilization, throughput, failure rate,
and latency. Its main purpose is **sizing `maxReplicaCount` per scope** from the
observed daily peak.

### Variables

- **Scope** - multi-select, populated from `label_values(browser_hive_worker_total_slots, scope)`. Defaults to `All`.
- **WORKER_MAX_CONTEXTS** - the per-worker context count for the selected scope (default `2`).
  Used only by the "Recommended maxReplicas" stat. If scopes run different values, filter
  to one scope at a time and set this to match.

### Reading it

- **Concurrent busy contexts** - the load curve. Its peak over the range drives sizing.
- **Recommended maxReplicas** - `ceil(peak_busy / WORKER_MAX_CONTEXTS * 1.25)` over the
  selected time range.
- **Utilization %** - sustained ~100% means capacity is capped and the busy-contexts curve
  is clipped; true demand is higher than shown. Add workers or check the rejection signal.
- **Avg in-flight (Little's Law)** - sampling-immune concurrency from the duration
  histogram. Cross-check against the busy-contexts peak; if it is consistently higher,
  shorten the worker scrape interval.

### Refused demand row (coordinator metrics)

The four panels in this row come from `browser_hive_coordinator_*` and require the
coordinator to be scraped as well - see METRICS.md. They answer the one question the worker
metrics structurally cannot: **how much demand was turned away**. There is no queue in
Browser Hive, so a request refused with `NO_WORKERS_AVAILABLE` (5001) never reaches a worker
and leaves no trace in any `browser_hive_worker_*` series. A fleet at 100% utilization and a
fleet rejecting half its traffic look identical from the worker side.

- **Rejected requests/sec by reason** - split by cause. `no_slots`/`no_workers` mean
  capacity; `scope_not_found`/`session_not_found` mean a client or configuration problem and
  no amount of scaling will change them.
- **Capacity rejection rate %** - the actionable ratio. Sustained above zero = the scope is
  under-provisioned.
- **Requests lost to capacity** - the same thing in absolute requests over the range: the
  cost of the current replica settings.
- **Coordinator view: free slots and healthy workers** - the discovery cache routing
  actually decides on. Free slots pinned at zero with rejections rising is genuine
  saturation; healthy workers collapsing while pods run is a discovery/health problem
  wearing a capacity costume.

The fix for capacity rejections is `minReplicaCount`/`maxReplicaCount`, **not** a lower KEDA
threshold: the threshold sets steady-state utilization, and no threshold can conjure a pod
faster than it boots. See the KEDA section of METRICS.md.

## `browser-hive-workers-dashboard.json` - Workers per Scope

Per-scope view of **how many worker instances run over time** (the autoscaling picture -
in Kubernetes a worker instance is a pod, but the metric works the same for any
deployment): current worker count, peak, and average over the selected range, plus a row
aggregated by browser mode (headless `hl` / headful `hf`).

### How workers are counted

There is no dedicated worker-count metric: each worker instance exports exactly **one**
`browser_hive_worker_total_slots{scope}` series (Prometheus adds the `pod`/`instance`
label), so

```promql
count by (scope) (browser_hive_worker_total_slots)
```

equals the number of live, scraped workers of that scope. Caveats:

- A worker is counted only while Prometheus successfully scrapes it: workers still
  starting (metrics server not up yet) are not counted; terminating workers stay counted
  until their scrape fails. In Kubernetes, cross-check with kube-state-metrics
  (`kube_deployment_status_replicas`) for an authoritative replica count if available.
- When a scope has **zero** workers, `count()` returns no data - the graph shows a
  **gap**, not a zero line, and range averages are computed only over moments when
  workers existed.
- **Avg workers x range hours = worker-hours** - a quick cost/capacity estimate per
  scope.

### Headless / Headful row

The `hl`/`hf` mode is extracted from the scope name with
`label_replace(..., "browser_mode", "$1", "scope", ".*_(hl|hf)_.*")` - it relies on the
scope naming convention `{provider}_{hl|hf}_{session_mode}`. Scopes that do not match the
pattern fall into an unnamed series. Headful workers consume significantly more RAM/CPU,
so the per-mode worker counts (and the capacity/slots panel) drive node pool sizing. Note
that hl and hf run different `WORKER_MAX_CONTEXTS`, so worker counts and slot capacity
differ per mode - the dashboard shows both.

## `browser-hive-browser-resources-dashboard.json` - Browser Resources

Per-pod view of what the browser is made of: memory and process count per Chromium process
type, CDP targets and browser contexts, and the worker process's own RSS and threads. Its
purpose is diagnosing OOMKills without `kubectl exec`. Metric semantics are in the "Browser
resources" section of METRICS.md.

### Variables

- **Scope** - as in the other dashboards.
- **Pod** - read from the `pod` label that Kubernetes scraping adds. The OOM limit is per
  container, so **select a single pod** when reading the by-type panels, which otherwise sum
  every selected pod. Outside Kubernetes the list is empty and `All` still matches everything.

### Reading it

- **Browser memory by process type** - where a pod's memory goes. PSS, so the stack adds up.
- **Container restarts and OOMKills per pod** - when a pod died and whether memory killed it.
- **Memory per pod vs. container limit** - browser PSS plus worker RSS, the container working
  set, and the memory limit as a dashed line. A line climbing to the dashed one is the next
  OOMKill.
- **Largest single process** together with **Browser processes by type** - one bloated
  renderer (high max, few processes) vs. many small ones (low max, many processes).
- **Out-of-process iframes per page** - high values mean the memory comes from what pages
  embed (a renderer per framed third-party site), not from tab age.
- **Browser main processes per pod** - counts launcher wrappers too (5 per browser for Brave in
  production); a step up by one browser's worth is a browser process that was never reaped.
- **Browser contexts: not in the pool** - contexts the browser still holds after the pool
  let them go; meaningful for isolated scopes only.
- **Worker RSS / threads** - growth with uptime is a leak in the worker process itself.
- **CPU per pod vs. container request and limit** - usage climbing on an almost idle pod is the
  browser burning CPU on its own; usage pinned at the limit is throttling.
- **Third-party hosts row** - top 100 tables over the dashboard range, for choosing block list
  patterns: iframes, loads that went out, and loads the list blocked. Each comes by host (summed
  over sites) and by site and host. A host still high in the loads table is not matched by any
  pattern; a row with `page_site="other"` means a worker reached its cap on label combinations.
  Loads inside a cross-site iframe are not counted, and the list does not block iframes. The last
  two panels show loads dropped by resource type (`WORKER_BLOCKED_RESOURCE_TYPES`), over time and
  by site; they stay empty on a scope that blocks no type. See the "Third-party hosts and iframes"
  section of METRICS.md.

Gaps mean a source could not be read: a worker version without these gauges, a non-Linux
host (process gauges), or a target probe that failed or ran past its 3 s budget.

### Kubernetes metrics (limit, working set, OOMKills)

Three panels also read standard Kubernetes metrics that Browser Hive does not export. They must
be scraped by the **same Prometheus** as the workers; kube-prometheus-stack does both by
default.

| Series | Source |
|---|---|
| `container_memory_working_set_bytes` | cAdvisor (kubelet) |
| `container_cpu_usage_seconds_total` | cAdvisor (kubelet) |
| `kube_pod_container_resource_limits{resource="memory"}`, `{resource="cpu"}` | kube-state-metrics v2 |
| `kube_pod_container_resource_requests{resource="cpu"}` | kube-state-metrics v2 |
| `kube_pod_container_status_restarts_total` | kube-state-metrics |
| `kube_pod_container_status_last_terminated_reason` | kube-state-metrics |

The limit comes from kube-state-metrics rather than cAdvisor's `container_spec_memory_limit_bytes`,
because kube-prometheus-stack drops `container_spec_*` by default. Every such query is
restricted to worker pods with `and on (pod)` against `browser_hive_worker_total_slots`, so the
selection follows the Scope and Pod variables and never pulls in the rest of the cluster.

After a container restart cAdvisor exports the dead container next to the new one for a few
minutes, under the same `pod` and `container`. The working-set query therefore takes the
`max` per container before summing per pod — a plain sum drew spikes far above the limit.

If those series are missing, the panels show only the Browser Hive lines, or nothing. To
check, run each metric name above in Grafana Explore. Cloud-provider system metrics (for
example GKE's `kubernetes.io/container/memory/limit_bytes` in Cloud Monitoring) live in a
different backend and cannot be combined with these PromQL queries.

See [../../METRICS.md](../../METRICS.md) for metric semantics, sizing math, and KEDA guidance.
