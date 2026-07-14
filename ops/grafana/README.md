# Grafana Dashboards

## `browser-hive-dashboard.json` - Load & Capacity per Scope

Per-scope view of concurrent load, capacity utilization, throughput, failure rate,
and latency. Its main purpose is **sizing `maxReplicaCount` per scope** from the
observed daily peak.

### Import

Grafana → Dashboards → New → Import → upload the JSON (or paste it), then pick your
Prometheus datasource.

### Variables

- **Scope** - multi-select, populated from `label_values(browser_hive_worker_total_slots, scope)`. Defaults to `All`.
- **WORKER_MAX_CONTEXTS** - the per-pod context count for the selected scope (default `3`).
  Used only by the "Recommended maxReplicas" stat. If scopes run different values, filter
  to one scope at a time and set this to match.

### Reading it

- **Concurrent busy contexts** - the load curve. Its peak over the range drives sizing.
- **Recommended maxReplicas** - `ceil(peak_busy / WORKER_MAX_CONTEXTS * 1.25)` over the
  selected time range.
- **Utilization %** - sustained ~100% means capacity is capped and the busy-contexts curve
  is clipped; true demand is higher than shown. Add pods or check the rejection signal.
- **Avg in-flight (Little's Law)** - sampling-immune concurrency from the duration
  histogram. Cross-check against the busy-contexts peak; if it is consistently higher,
  shorten the worker scrape interval.

See [../../METRICS.md](../../METRICS.md) for metric semantics, sizing math, and KEDA guidance.
