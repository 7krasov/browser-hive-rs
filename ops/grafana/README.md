# Grafana Dashboards

Import: Grafana → Dashboards → New → Import → upload the JSON (or paste it), then pick
your Prometheus datasource.

## `browser-hive-dashboard.json` - Load & Capacity per Scope

Per-scope view of concurrent load, capacity utilization, throughput, failure rate,
and latency. Its main purpose is **sizing `maxReplicaCount` per scope** from the
observed daily peak.

### Variables

- **Scope** - multi-select, populated from `label_values(browser_hive_worker_total_slots, scope)`. Defaults to `All`.
- **WORKER_MAX_CONTEXTS** - the per-worker context count for the selected scope (default `3`).
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

See [../../METRICS.md](../../METRICS.md) for metric semantics, sizing math, and KEDA guidance.
