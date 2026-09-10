# Error Handling Guide

This document describes Browser Hive's error handling philosophy and how to work with errors in your client applications.

## Philosophy

Browser Hive follows a **dual-layer error handling approach**:

1. **gRPC Status Layer** - Infrastructure-level failures only
2. **ErrorCode Layer** - Operational errors in business logic

### Key Principle

> **gRPC Status is always 0 (OK) if the system can return a response to the client.**

This means:
- ✅ Navigation failures → gRPC OK + `ErrorCode::NetworkError`
- ✅ Selector not found → gRPC OK + `ErrorCode::SelectorNotFound`
- ✅ Browser crashes → gRPC OK + `ErrorCode::BrowserError`
- ❌ Network unreachable → gRPC Error (can't communicate at all)

### Why This Design?

**Traditional approach** (what we DON'T do):
```
Client → Request → Server dies → gRPC Status = INTERNAL
         ↓
    No response body
```

**Browser Hive approach**:
```
Client → Request → Navigation fails → gRPC Status = OK
         ↓
    Full response with:
    - success: false
    - error_code: NETWORK_ERROR
    - error_message: "Failed to navigate..."
    - execution_time_ms: 1523
    - content: "" (or partial content)
```

This allows clients to:
- Always parse responses uniformly
- Get detailed error context (execution time, context metadata)
- Distinguish between infrastructure failures and operational errors
- Receive partial results when available

## ErrorCode Reference

Error codes live in **two** proto files, and the split matters to a client.

`crates/proto/proto/worker.proto` defines the codes a worker can produce. The client-facing
`crates/proto/proto/coordinator.proto` repeats most of them with the same numeric values and adds
the ones the coordinator alone can return, because they describe failures that happen *before* any
worker is involved:

| Code | Value | Defined in |
|------|-------|------------|
| `ERROR_CODE_TIMEOUT_GRPC` | 1 | `coordinator.proto` only (reserved, not currently emitted) |
| `ERROR_CODE_NO_WORKERS_AVAILABLE` | 5001 | `coordinator.proto` only |
| `ERROR_CODE_WORKER_UNREACHABLE` | 5002 | `coordinator.proto` only |
| `ERROR_CODE_CAPACITY_EXHAUSTED` | 5008 | `worker.proto` only — **never reaches a client** |

A client talks to the coordinator, so it should generate from `coordinator.proto`.

### Capacity is not a malfunction: 5008 → 5001

`ERROR_CODE_CAPACITY_EXHAUSTED` (5008) is the one code that travels only on the internal hop. A
worker returns it when every slot is taken, i.e. when `acquire_context` found nothing to hand out —
**no context was even attempted**. It exists because that answer used to be
`ERROR_CODE_CONTEXT_CREATION_FAILED` (5005), which says the opposite: that creating a context was
tried and the browser refused. Three things were wrong with sharing one code:

- **The worker's failure metric lied.** `browser_hive_worker_requests_failed` counts the whole 5xxx
  range, so a scope that was merely busy looked like a scope that was breaking — and `success_rate`
  in `GetStats` sank with it. 5008 is explicitly exempt (`counts_as_failure` in
  `worker/src/service.rs`).
- **The autoscaling signal was missing.** `requests_rejected_total{reason="no_slots"}` only counted
  refusals the *coordinator* decided. A refusal the worker discovered — which is real demand that
  got nothing — was recorded nowhere at all.
- **The client could not act.** The same situation arrived as 5001 when the coordinator saw it and
  5005 when the worker did.

The coordinator now either retries it on another pod (see below) or maps it to
`ERROR_CODE_NO_WORKERS_AVAILABLE` (5001) and counts `reason="no_slots"`. The worker's own wording is
kept in `error_message`, because it names the limit that has to change (`max_contexts`, and in
`dedicated` the idle timeout).

Why a client-facing code was *not* added: 5001 already means "the fleet has no room for this
request right now", and a client that handles 5001 needs no second code for the same decision. The
extra detail an operator needs is in the `reason` label, not in the enum.

### Retrying on another worker

Two worker answers are worth re-sending to a different pod, and only two (`classify_retry` in
`coordinator/src/service.rs`):

| Answer | Max attempts | Why |
|---|---|---|
| `ERROR_CODE_TERMINATING` (5006) | 3 | a rolling restart can hit several pods of a scope in turn |
| `ERROR_CODE_CAPACITY_EXHAUSTED` (5008) | 2 (one retry) | covers the few seconds of staleness in the coordinator's slot cache, where another pod really does have room |

Capacity deliberately gets **fewer** attempts. A 5008 answer means the scope is at its limit, and
that is the worst moment to multiply RPCs across it — three attempts per client would turn a
saturated scope into a self-amplifying load generator.

A request carrying a `session_id` is **never** retried for capacity: the session lives in one
context on one pod, so "somewhere else" is a different session. (TERMINATING is the exception — that
pod is going away and the session with it.)

Every retry increments `browser_hive_coordinator_requests_retried_total{scope, reason}`. Without it
a successful retry is invisible: the client got a normal response, nothing was rejected, and the
first worker's refusal is not a failure either — so a scope that only stays healthy because half
its requests are retried would look comfortable. Watch it next to
`requests_rejected_total{reason="no_slots"}`: retries rising is the early warning, rejections are
the same problem arriving too late.

Common errors are not retried on another worker (a dead browser, a missing selector, a 403): every
pod in the scope would answer them the same way, so a retry only spends the client's deadline. Those
are the client's own retries to make, on its own schedule.

### Success

| Code | Value | Description |
|------|-------|-------------|
| `ERROR_CODE_NONE` | 0 | No error - operation succeeded |

### Client Errors (4xxx)

These indicate issues with the request itself.

| Code | Value | Description | Retry? |
|------|-------|-------------|--------|
| `ERROR_CODE_INVALID_URL` | 4001 | Invalid URL format provided | ❌ No - fix URL |
| `ERROR_CODE_SESSION_NOT_FOUND` | 4002 | Session/context ID not found or expired | ⚠️ Retry without session |
| `ERROR_CODE_SCOPE_NOT_FOUND` | 4003 | No worker pod in the cluster carries this scope name | ❌ No - check scope name |
| `ERROR_CODE_SESSION_BUSY` | 4004 | The session's own context is still serving a previous request (`dedicated` only) | ⚠️ Retry the **same** session after a short pause |
| `ERROR_CODE_TIMEOUT_BROWSER` | 4041 | Browser timeout during page load | ✅ Yes - increase timeout |
| `ERROR_CODE_SELECTOR_NOT_FOUND` | 4042 | Wait selector not found within timeout | ⚠️ Maybe - check selector |
| `ERROR_CODE_SKIP_SELECTOR_FOUND` | 4043 | Skip selector found (content should be ignored) | ❌ No - expected behavior |
| `ERROR_CODE_REDIRECT_TO_ANOTHER_DOMAIN` | 4050 | Navigation was redirected to a different registrable domain (eTLD+1); `wait_selector`/`skip_selector` are not applied to the foreign page. `status_code` is the landing page's status; `response_headers` are the landing page's | ❌ No - target site redirected off-domain |

### Server Errors (5xxx)

These indicate issues with the worker/browser infrastructure.

| Code | Value | Description | Retry? |
|------|-------|-------------|--------|
| `ERROR_CODE_NO_WORKERS_AVAILABLE` | 5001 | No workers available, all slots busy (as the coordinator saw it, or as the chosen worker reported via 5008), **or the scope's pods are all restarting** | ✅ Yes - retry after delay or scale workers |
| `ERROR_CODE_WORKER_UNREACHABLE` | 5002 | Routing picked a worker, but the coordinator could not connect to it, or the RPC broke mid-request (the pod died). Returned in the response body, with `worker_id` naming the pod | ✅ Yes - retry after delay |
| `ERROR_CODE_BROWSER_ERROR` | 5003 | Browser process crashed or internal failure | ✅ Yes - worker auto-recovers |
| `ERROR_CODE_NETWORK_ERROR` | 5004 | Network error during page navigation | ✅ Yes |
| `ERROR_CODE_CONTEXT_CREATION_FAILED` | 5005 | CDP refused to create a browser context — a **real malfunction**, not a full pool (that is 5008, mapped to 5001) | ✅ Yes |
| `ERROR_CODE_TERMINATING` | 5006 | Worker/Coordinator is shutting down gracefully | ✅ Yes - retry immediately or route to another instance |
| `ERROR_CODE_PROXY_ERROR` | 5007 | The proxy path failed: a refused/failed `CONNECT`, an unreachable proxy, or a proxy auth problem. Says nothing about the target site | ✅ Yes - a retry draws a different exit IP |
| `ERROR_CODE_CAPACITY_EXHAUSTED` | 5008 | Every slot of the chosen worker was taken. **Internal only** — the coordinator retries it on another pod or maps it to 5001; a client never sees it | — |

#### HTTP 403 and 429 are reported through `status_code`, not an error code

Neither gets an `ErrorCode` of its own. The response carries `success = true`, `error_code = 0`
and the origin's `status_code`, and the client decides what that means — the same treatment 403
has always had. Duplicating an HTTP status into a bespoke enum would create a second source of
truth for a fact the response already states, and the list would have no natural end (451? 503?).

`content` is the origin's block or refusal page, so a client that keys "this is content" on
`success` alone will store it. Key on `status_code` instead.

What the worker does do is stop waiting: with the `network_idle` strategy the request returns as
soon as the status is seen, instead of spending the rest of its budget polling for a selector
that cannot appear — measured at 38.8 s of a 40 s budget before the change. The `timeout`
strategy still burns its full budget by design.

#### When 5007 is returned

Detection is keyed on Chromium's error taxonomy (`ERR_TUNNEL_CONNECTION_FAILED`,
`ERR_PROXY_CONNECTION_FAILED`, …), not on any provider's status code — for HTTPS the proxy's
reply to `CONNECT` is consumed by the network stack, so a provider's own status (an unavailable
peer is typically answered with 502) never surfaces as `status_code`. The classification is
therefore identical for every proxy provider. See PROXY_NETWORKING.md.

Two situations produce it:

1. **The main navigation failed** with a proxy-class error. Previously reported as
   `ERROR_CODE_NETWORK_ERROR` (5004), which conflated a dead tunnel with a site-side problem.
2. **Sub-resources failed** with a proxy-class error *and* the request was already failing with
   `ERROR_CODE_SELECTOR_NOT_FOUND` (4042) or `ERROR_CODE_TIMEOUT_BROWSER` (4041). A page whose
   scripts were lost to a dead tunnel renders as an unfilled template, so it otherwise fails as
   a plain "selector not found" that blames the site. `error_message` keeps the original text
   and appends the failure summary.

Deliberately **not** overridden: a successful request (the client got what it asked for — the
failures appear only in the log), `ERROR_CODE_SKIP_SELECTOR_FOUND` (that element really was
present, which a network failure cannot invalidate), and
`ERROR_CODE_REDIRECT_TO_ANOTHER_DOMAIN` (a definitive observation). Hard-timeout and
cancellation paths return before the check and are not covered.

`status_code` is unaffected: it stays whatever the main document actually returned.

Proxy failures are logged at WARN on every request that has any (`N resource load(s) failed
with a proxy/tunnel error: …`), independent of the outcome and of
`WORKER_ENABLE_BROWSER_DIAGNOSTICS` — including on requests that succeed, where they are the
only trace that a page was served through two different exit IPs.

⚠️ **Metrics impact**: `browser_hive_worker_requests_failed` counts 5xxx codes, so cases that
used to be silent 4042s now count as failures. An increase after deploying this is the metric
becoming honest, not a regression.

### Unknown Errors (9xxx)

| Code | Value | Description | Retry? |
|------|-------|-------------|--------|
| `ERROR_CODE_UNKNOWN` | 9999 | Unexpected/unhandled error | ⚠️ Maybe |

## Response Structure

The client-facing response (`scraper.coordinator.ScrapePageResponse`) contains:

```protobuf
message ScrapePageResponse {
  bool success = 1;               // false on error
  uint32 status_code = 2;         // HTTP status or 0
  string content = 3;             // Empty or partial content
  string error_message = 4;       // Detailed human-readable error
  ErrorCode error_code = 5;       // Machine-readable error code
  map<string, string> response_headers = 6;  // Main-document headers, CDP format:
                                  // repeated headers joined with "\n", server-case names
                                  // (see RESPONSE_OBSERVERS.md)
  string session_id = 7;          // Session ID for reuse ("{worker_id}:{context_id}", may be empty)
  string worker_id = 8;           // Worker pod ID (component of session_id, for debugging)
  string context_id = 9;          // Browser context ID (component of session_id, for debugging)
  uint64 execution_time_ms = 10;  // Always present
  string ray_id = 11;             // Tracing ID (same as in request, or auto-generated)
}
```

To reuse a browser session, pass `session_id` in the next request. The internal worker-to-coordinator proto (`scraper.worker.ScrapePageResponse`) differs slightly (it returns only `context_id`); the examples below show the client-facing view.

**The scenario payloads below are abridged**: every response really carries all eleven fields
above, and the examples show only the ones the scenario is about. Absent fields are at their
protobuf defaults (`""` for strings, `0` for numbers) — in particular `ray_id` is always
populated in a real response, and `session_id`/`worker_id` are empty strings on the paths that
never reached a worker or that ran in a non-`dedicated` scope.

### Error Message Format

Error messages follow these patterns:

**INVALID_URL**:
```
Invalid URL: <parse error details>
```

**SESSION_NOT_FOUND** — three distinct messages, from two layers. Match on the
`error_code` (4002), never on the text:
```
Session not found or expired
```
  the coordinator has no such `session_id` in its `SessionManager` (`coordinator/src/service.rs`)
```
Session expired or worker unavailable. Please retry without session_id
```
  the session existed, but the worker that owned it is gone from discovery
  (`coordinator/src/service.rs`)
```
Context not found or expired: <context_id>
```
  the worker was reached and no longer has that context (`worker/src/service.rs`). A client
  talking to the coordinator normally sees one of the first two

**SESSION_BUSY**:
```
Context <id> is already busy (created: <timestamp>, last_used: <timestamp>,
total_requests: <count>, cache_size_mb: <size>) (after <ms>ms)
```

**BROWSER_ERROR** (tab creation failure):
```
Failed to create tab for context <id> (domain: <domain>): <error> (after <ms>ms)
```

**CONTEXT_CREATION_FAILED**:
```
Failed to create new browser context: <error> (after <ms>ms)
Failed to create context after pool recreation: <error>
```

**CAPACITY_EXHAUSTED** (internal; reaches the client as 5001 prefixed with the scope name). The
wording follows the session mode, because what has to change differs:
```
No available contexts - all <N> contexts are busy                        (reusable)
No available slots - max contexts limit (<N>) reached                    (always_new)
No available session slots - all <N> contexts are claimed by sessions …  (dedicated)
```

**NETWORK_ERROR**:
```
Failed to navigate to URL: <error>
```

**SELECTOR_NOT_FOUND**:
```
Wait selector '<selector>' was not found within timeout
```

**SKIP_SELECTOR_FOUND**:
```
Skip selector '<selector>' was found
```

**TIMEOUT_BROWSER**:
```
Wait strategy '<strategy>' failed: <timeout details>
```

**TERMINATING**:
```
Worker is shutting down, please retry with another instance
```
or
```
Coordinator is shutting down, please retry
```

## Common Error Scenarios

### Scenario 1: Invalid URL

**Request**:
```json
{
  "scope_name": "my_scope",
  "url": "not-a-valid-url"
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Invalid URL: relative URL without a base",
  "error_code": 4001,
  "execution_time_ms": 1,
  "context_id": ""
}
```

**Client Action**: Fix the URL and retry.

---

### Scenario 2: Session Expired

**Request**:
```json
{
  "scope_name": "my_scope",
  "url": "https://example.com",
  "session_id": "worker-1:ctx-expired-123"
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Session expired or worker unavailable. Please retry without session_id",
  "error_code": 4002,
  "execution_time_ms": 15,
  "session_id": ""
}
```

**Client Action**: Retry without `session_id` to get a new session.

---

### Scenario 3: Network Error (Site Down)

**Request**:
```json
{
  "scope_name": "my_scope",
  "url": "https://nonexistent-site-xyz.com"
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "<html><body>ERR_NAME_NOT_RESOLVED</body></html>",
  "error_message": "Failed to navigate to URL: net::ERR_NAME_NOT_RESOLVED",
  "error_code": 5004,
  "execution_time_ms": 5234,
  "context_id": "ctx-abc123"
}
```

**Client Action**: Retry with exponential backoff or mark URL as unavailable.

**Note**: `content` contains Chrome's error page HTML.

---

### Scenario 4: Wait Selector Not Found

**Request**:
```json
{
  "scope_name": "my_scope",
  "url": "https://example.com",
  "wait_selector": "#login-button",
  "wait_timeout_ms": 10000
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 200,
  "content": "<html>...</html>",
  "error_message": "Wait selector '#login-button' was not found within timeout",
  "error_code": 4042,
  "execution_time_ms": 10234,
  "context_id": "ctx-abc123"
}
```

**Client Action**: Check if selector is correct or increase timeout. Content is still available.

---

### Scenario 5: Skip Selector Found

**Request**:
```json
{
  "scope_name": "my_scope",
  "url": "https://example.com",
  "skip_selector": ".captcha-challenge"
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 200,
  "content": "<html>...captcha...</html>",
  "error_message": "Skip selector '.captcha-challenge' was found",
  "error_code": 4043,
  "execution_time_ms": 3421,
  "context_id": "ctx-abc123"
}
```

**Client Action**: Handle as expected behavior - content should be skipped. Content is still provided for analysis.

---

### Scenario 6: Browser Crashed

**Request**:
```json
{
  "scope_name": "my_scope",
  "url": "https://example.com"
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Failed to create tab for context ctx-123 (domain: example.com): connection is closed (after 234ms)",
  "error_code": 5003,
  "execution_time_ms": 234,
  "context_id": "ctx-123"
}
```

**Client Action**: Retry immediately - worker automatically recreates browser pool.

---

### Scenario 6a: Tab Died After Hard Timeout (Automatic Recovery)

This scenario happens internally when a previous request hit a hard timeout and the tab was closed to abort a stuck CDP call. The system automatically recovers without any client action needed.

**What happens internally**:
1. Previous request hits hard timeout (e.g., navigation stuck for 20s)
2. Worker closes the tab via `tab.close(false)` to abort the CDP call
3. Context remains in pool (in Reusable/Dedicated mode) with cookies/storage preserved
4. Next request to this context gets "No session with given id" error
5. Worker automatically creates new tab in the same CDP BrowserContext
6. Request proceeds normally with session state preserved

**When client sees this**: Never - the recovery is transparent. Client receives normal response.

**Internal log message** (for debugging):
```
WARN Tab CDP session dead ('No session with given id') - recreating tab for context ctx-123 (cdp_context_id: Some("ABC123"))
INFO Successfully recreated tab after dead session for context ctx-123 (cdp_context_id: Some("ABC123"))
```

**Key benefit**: In Reusable mode, even after hard timeouts kill the tab, cookies and storage are preserved because the CDP BrowserContext survives. Only the tab (CDP Target) is recreated.

---

### Scenario 7: Session Busy

**Request**: Two concurrent requests on the same `session_id` (`dedicated` scopes only — no other
mode addresses a context by id, so no other mode can produce this).

**Response** (second request):
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Context ctx-123 is already busy (created: 2024-01-15T10:00:00Z, last_used: 2024-01-15T10:05:30Z, total_requests: 42, cache_size_mb: 128) (after 2ms)",
  "error_code": 4004,
  "execution_time_ms": 2,
  "context_id": "ctx-123"
}
```

**Client Action**: pause briefly and retry **the same** `session_id`. After 2–3 failures, drop the
session id and start a new session: the previous request may have hung and will hold the context
until the idle timeout.

**Why 4xxx.** This used to be `ERROR_CODE_BROWSER_ERROR` (5003), which blamed the browser, counted
the request as a worker failure and pointed an operator at logs holding nothing. Nothing is broken:
the context is busy with *this client's own* previous request. It is also not a capacity problem —
adding replicas cannot help, and the coordinator must **not** retry it on another pod, because the
session exists only in that context on that pod.

---

### Scenario 8: Graceful Shutdown (Terminating)

**Request**: Normal request while worker/coordinator is shutting down.

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Worker is shutting down, please retry with another instance",
  "error_code": 5006,
  "execution_time_ms": 5,
  "context_id": ""
}
```

**Client Action**: Retry immediately with the same or different worker. This error indicates graceful shutdown - the request was not processed to allow existing requests to complete. The service will route your retry to a healthy instance.

---

### Scenario 9: Scope Not Found

**Request**: Request with non-existent scope name.

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Scope not found: invalid_scope",
  "error_code": 4003,
  "execution_time_ms": 2,
  "context_id": ""
}
```

**Client Action**: Check scope name configuration. This is a client configuration error - verify that the scope exists in your deployment. Do not retry with the same scope name.

---

### Scenario 10: No Workers Available

**Request**: Request when all workers are busy or no workers exist for scope.

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "No available slots in scope: my_scope. All workers are busy.",
  "error_code": 5001,
  "execution_time_ms": 15,
  "context_id": ""
}
```

**Client Action**: Retry with exponential backoff (start with 1-5 seconds). This indicates temporary resource exhaustion. If the error persists:
- Check worker health and availability
- Consider scaling up worker replicas
- Review workload patterns (might need more capacity)

**Coordinator Behavior**: The coordinator returns this error when:
1. No workers are discovered for the specified scope
2. All discovered workers have `available_slots = 0` (all busy)
3. Health check indicates all workers are unhealthy
4. The cluster has pods labelled with this scope, but **none of them is currently reachable** — they are booting, terminating, or otherwise not answering `GetStats`
5. A worker was sent the request and answered `CAPACITY_EXHAUSTED` (5008) — its last free slot was taken in the few seconds between the coordinator's stats and the request arriving, and no other pod had room either. The `error_message` then carries the worker's own wording, e.g. `No available slots in scope my_scope: No available contexts - all 3 contexts are busy`

### 5001 vs 4003: "come back later" vs "fix the name"

Case 4 above is the one worth understanding, because it used to be reported as `SCOPE_NOT_FOUND`.

A scope enters the map the coordinator routes on only once one of its pods answers `GetStats`, and a worker's gRPC server starts only **after** its browser pool is up. So during a pod restart — a spot node reclaimed, a rollout, an OOM kill — every pod of the scope is unreachable and the whole scope disappears from routing for roughly the browser launch time (~9 s) plus up to the 10 s discovery interval. Requests arriving in that window were told the scope did not exist, which reads as a permanent client-side configuration error and stops a well-behaved client from retrying.

The coordinator now separates the two questions using the pod list it already fetches:

| Question | Source | Answer |
|---|---|---|
| Does this scope exist at all? | the `scope` **label** on pods, present from the moment the pod object is created | no → `SCOPE_NOT_FOUND` (4003) |
| Can any of its pods serve right now? | `GetStats` succeeded | no → `NO_WORKERS_AVAILABLE` (5001) |

`error_message` carries the pod breakdown so the reason is visible without cluster access:

```
Scope brightdata_dc_shared_hl_reusable_ego is temporarily unavailable
(3 pod(s), 0 reachable (3 starting up or unreachable)); retry shortly
```

⚠️ **Client requirement**: 5001 is retryable and 4003 is not. A client that treats both as fatal loses every request during a routine pod restart; a client that retries 4003 forever hides a real typo. The distinction is only useful if the client acts on it.

⚠️ **5001 covers five distinct situations** (no pods, none healthy, no free slots as seen by the coordinator, **a worker that ran out of slots between the check and the request** — see the 5008 mapping above — and a scope whose pods are restarting). All five are retryable, so a client needs no further detail — but they call for different **backoff lengths**, and an operator separating them should use the `reason` label on `browser_hive_coordinator_requests_rejected_total` (`no_workers`, `no_slots`, `scope_unavailable`), not the error code. See METRICS.md.

## Client Implementation Guidelines

### Retry Strategy

The whole decision in one rule: **every 5xxx code is retryable, no exceptions; in the 4xxx range
only three are, and each in its own way.** The invariant is worth relying on — it is what the 5008
split above exists to restore.

**5xxx — retry all of them** (with exponential backoff unless noted):

| Code | Backoff | Notes |
|---|---|---|
| `NO_WORKERS_AVAILABLE` (5001) | **≥ 10–20 s** | no pods / none healthy / no free slots / the scope's pods are restarting. A pod needs ~9 s of browser launch plus up to the 10 s discovery interval to become routable |
| `WORKER_UNREACHABLE` (5002) | short | the next attempt routes to a different pod by itself |
| `BROWSER_ERROR` (5003) | short (1–2 s) | the worker recreates its pool on its own |
| `NETWORK_ERROR` (5004) | short | `content` holds the Chrome error page, which names the cause |
| `CONTEXT_CREATION_FAILED` (5005) | short | a real CDP refusal; if it repeats on one scope, look at the worker logs |
| `TERMINATING` (5006) | **none, retry at once** | the coordinator already tried up to 3 pods; reaching the client means < 10 s of the deadline was left |
| `PROXY_ERROR` (5007) | none on the first retry, then back off | a retry draws a different exit IP; repeated failures mean a whole provider zone is down |

**4xxx — retryable only here**:

| Code | What to do |
|---|---|
| `SESSION_NOT_FOUND` (4002) | retry **without** `session_id`; store the new one from the response |
| `SESSION_BUSY` (4004) | retry **with the same** `session_id` after 0.5–2 s; after 2–3 failures drop the session and start a new one |
| `TIMEOUT_BROWSER` (4041) | retry is possible, but review `wait_timeout_ms` first — the same budget will time out again |

**4xxx — never retry**: `INVALID_URL` (4001), `SCOPE_NOT_FOUND` (4003 — no pod in the cluster
carries that name; a restart is 5001 instead), `SELECTOR_NOT_FOUND` (4042 — check the selector, and
note `content` is still there), `SKIP_SELECTOR_FOUND` (4043 — this is the check working),
`REDIRECT_TO_ANOTHER_DOMAIN` (4050).

**HTTP 403 / 429** arrive as `success = true`, `error_code = 0` and the origin's `status_code`. The
system sees no error, so the decision is the client's: a retry is reasonable (it gets a different
context, and in `reusable` the one that was blocked is already quarantined for that origin), but back
off. ⚠️ `success` does **not** mean "this is content" — key on `status_code`, or a block page gets
stored as data.

### Session Management

When receiving `ERROR_CODE_SESSION_NOT_FOUND`:
1. Clear the stored `session_id`
2. Retry the request without `session_id`
3. Store the new `session_id` from the response

### Partial Content

Some errors still return content:
- `NETWORK_ERROR`: Chrome error page HTML
- `SELECTOR_NOT_FOUND`: Full page HTML (selector just wasn't found)
- `SKIP_SELECTOR_FOUND`: Full page HTML (for analysis)

Always check the `content` field even when `success: false`.

## gRPC Status Codes

Browser Hive only uses gRPC error statuses for true infrastructure failures:

| gRPC Status | When Used | Retry? |
|-------------|-----------|--------|
| `OK (0)` | Always, when a response can be returned — every operational error, including an unreachable worker | see the code |
| `UNAVAILABLE (14)` | The **coordinator** is unreachable (network partition, no coordinator pod) | ✅ with backoff |
| `DEADLINE_EXCEEDED (4)` | gRPC deadline, not a browser timeout | ⚠️ carefully — the page may have been loading |
| `INVALID_ARGUMENT (3)` | The request itself is rejected: unknown `wait_strategy`, `wait_timeout_ms` over the maximum (`worker/src/service.rs`) | ❌ fix the request |
| `INTERNAL (13)` | Should never happen | Report as a bug |

**Important**: If you see `INTERNAL` in production, this is a bug. Browser Hive returns `OK` with an
`ErrorCode` whenever it can answer at all.

⚠️ **A worker the coordinator cannot reach is *not* a gRPC error.** Both paths — the connection
could not be established, and the RPC broke mid-request because the pod died — return `OK` with
`ERROR_CODE_WORKER_UNREACHABLE` (5002), a `worker_id` naming the pod, plus `ray_id` and
`execution_time_ms` like every other response. They used to return `Status::internal`, which broke
this document's own principle: the client had to parse free text and lost the tracing id with it.
The mid-request case also recorded no rejection metric at all, so a pod dying under load looked like
a batch of ordinary completed requests. Both now count
`requests_rejected_total{reason="worker_unreachable"}`.

`INVALID_ARGUMENT` is the one status deliberately passed through from the worker: it is a defect in
the call, and dressing it up as an infrastructure problem would have clients retrying it forever.

## Debugging

### Enable Verbose Error Logging

Worker logs include detailed error context:

```bash
docker-compose logs -f worker | grep ERROR
```

### Check Execution Time

All error responses include `execution_time_ms`. High values may indicate:
- Network issues (>5000ms for NETWORK_ERROR)
- Browser hang (>30000ms for BROWSER_ERROR)
- Context contention (>100ms for "context busy" errors)

### Error Rate Monitoring

Workers expose Prometheus metrics (see [METRICS.md](METRICS.md)):

```bash
curl http://localhost:9090/metrics | grep browser_hive_worker_requests_failed
```

Note: `browser_hive_worker_requests_failed` counts responses with 5xxx `error_code` values (server-side failures) and gRPC-level infrastructure errors. 4xxx codes (invalid URL, session not found, selector not found, skip selector) are client-side conditions and are not counted - track those on the client side if needed.

## Best Practices

1. **Always check `success` field first** - don't rely only on `error_code`
2. **Parse `error_message` for debugging** - contains context like context IDs, domains, timestamps
3. **Use `execution_time_ms`** - helps identify slow requests vs fast failures
4. **Implement exponential backoff** - for 5xxx errors
5. **Don't retry 4xxx errors** - except `SESSION_NOT_FOUND` and `TIMEOUT_BROWSER`
6. **Handle `SKIP_SELECTOR_FOUND` gracefully** - this is expected behavior, not a failure
7. **Log `session_id` and `ray_id`** - helps trace session-specific issues and correlate logs across coordinator/worker
8. **Monitor error rates** - set alerts on unusual spikes

## FAQ

**Q: Why is `success: false` but `status_code: 200`?**

A: HTTP status reflects the actual HTTP response from the target site. Browser Hive successfully loaded the page (HTTP 200), but your wait selector wasn't found (`SELECTOR_NOT_FOUND`). The `content` field contains the actual HTML.

---

**Q: Should I retry `SKIP_SELECTOR_FOUND` errors?**

A: No. This indicates the page contains an element you want to skip (e.g., CAPTCHA, login wall). This is expected behavior. Retrying won't help.

---

**Q: What's the difference between `TIMEOUT_BROWSER` and gRPC `DEADLINE_EXCEEDED`?**

A:
- `TIMEOUT_BROWSER` = Browser wait strategy timed out (e.g., network_idle not reached)
- `DEADLINE_EXCEEDED` = gRPC request timeout (infrastructure level)

You'll get a full response with `TIMEOUT_BROWSER`, but no response with `DEADLINE_EXCEEDED`.

---

**Q: When should I increase `timeout_seconds` vs `wait_timeout_ms`?**

A:
- `timeout_seconds`: Overall request timeout (use for very slow sites)
- `wait_timeout_ms`: Wait strategy timeout (use when `network_idle` takes too long)

If you get `TIMEOUT_BROWSER`, increase `wait_timeout_ms`. If gRPC times out, increase `timeout_seconds`.

---

**Q: Can I get partial content on errors?**

A: Yes! For these errors:
- `NETWORK_ERROR`: Chrome error page HTML
- `SELECTOR_NOT_FOUND`: Full page HTML (selector just wasn't found)
- `SKIP_SELECTOR_FOUND`: Full page HTML (for analysis)

For other errors, `content` will be empty.

---

**Q: Why do I get `BROWSER_ERROR` with "context busy"?**

A: You sent concurrent requests to the same `context_id`. Each browser context can only handle one request at a time. Wait for the first request to complete, or use different sessions.
