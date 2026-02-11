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

All error codes are defined in `crates/proto/proto/worker.proto`.

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
| `ERROR_CODE_SCOPE_NOT_FOUND` | 4003 | Specified scope does not exist | ❌ No - check scope name |
| `ERROR_CODE_TIMEOUT_BROWSER` | 4041 | Browser timeout during page load | ✅ Yes - increase timeout |
| `ERROR_CODE_SELECTOR_NOT_FOUND` | 4042 | Wait selector not found within timeout | ⚠️ Maybe - check selector |
| `ERROR_CODE_SKIP_SELECTOR_FOUND` | 4043 | Skip selector found (content should be ignored) | ❌ No - expected behavior |

### Server Errors (5xxx)

These indicate issues with the worker/browser infrastructure.

| Code | Value | Description | Retry? |
|------|-------|-------------|--------|
| `ERROR_CODE_NO_WORKERS_AVAILABLE` | 5001 | No workers available or all slots busy | ✅ Yes - retry after delay or scale workers |
| `ERROR_CODE_BROWSER_ERROR` | 5003 | Browser process crashed or internal failure | ✅ Yes - worker auto-recovers |
| `ERROR_CODE_NETWORK_ERROR` | 5004 | Network error during page navigation | ✅ Yes |
| `ERROR_CODE_CONTEXT_CREATION_FAILED` | 5005 | Failed to create browser context | ✅ Yes |
| `ERROR_CODE_TERMINATING` | 5006 | Worker/Coordinator is shutting down gracefully | ✅ Yes - retry immediately or route to another instance |

### Unknown Errors (9xxx)

| Code | Value | Description | Retry? |
|------|-------|-------------|--------|
| `ERROR_CODE_UNKNOWN` | 9999 | Unexpected/unhandled error | ⚠️ Maybe |

## Response Structure

When an error occurs, `ScrapePageResponse` contains:

```protobuf
message ScrapePageResponse {
  bool success = 1;              // false on error
  uint32 status_code = 2;        // HTTP status or 0
  string content = 3;            // Empty or partial content
  string error_message = 4;      // Detailed human-readable error
  ErrorCode error_code = 5;      // Machine-readable error code
  map<string, string> response_headers = 6;
  uint64 execution_time_ms = 7;  // Always present
  string context_id = 8;         // Session ID (may be empty)
}
```

### Error Message Format

Error messages follow these patterns:

**INVALID_URL**:
```
Invalid URL: <parse error details>
```

**SESSION_NOT_FOUND**:
```
Context not found or expired: <context_id>
```

**BROWSER_ERROR** (context busy):
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
  "context_id": "ctx-expired-123"
}
```

**Response**:
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Context not found or expired: ctx-expired-123",
  "error_code": 4002,
  "execution_time_ms": 15,
  "context_id": ""
}
```

**Client Action**: Retry without `context_id` to get a new session.

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
3. Context remains in pool (in Reusable/ReusablePreinit mode) with cookies/storage preserved
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

### Scenario 7: Context Busy

**Request**: Two concurrent requests to same session.

**Response** (second request):
```json
{
  "success": false,
  "status_code": 0,
  "content": "",
  "error_message": "Context ctx-123 is already busy (created: 2024-01-15T10:00:00Z, last_used: 2024-01-15T10:05:30Z, total_requests: 42, cache_size_mb: 128) (after 2ms)",
  "error_code": 5003,
  "execution_time_ms": 2,
  "context_id": "ctx-123"
}
```

**Client Action**: Wait and retry, or use a different session.

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

## Client Implementation Guidelines

### Retry Strategy

**Retryable errors** (with exponential backoff):
- `ERROR_CODE_NO_WORKERS_AVAILABLE` (5001) - all workers busy or no workers for scope
- `ERROR_CODE_BROWSER_ERROR` (5003)
- `ERROR_CODE_NETWORK_ERROR` (5004)
- `ERROR_CODE_CONTEXT_CREATION_FAILED` (5005)
- `ERROR_CODE_TERMINATING` (5006) - retry immediately, service is shutting down gracefully
- `ERROR_CODE_TIMEOUT_BROWSER` (4041) - but consider increasing timeout first

**Non-retryable errors**:
- `ERROR_CODE_INVALID_URL` (4001) - fix the URL first
- `ERROR_CODE_SCOPE_NOT_FOUND` (4003) - check scope configuration
- `ERROR_CODE_SKIP_SELECTOR_FOUND` (4043) - expected behavior

**Special handling**:
- `ERROR_CODE_SESSION_NOT_FOUND` (4002) - retry without `context_id` to start new session
- `ERROR_CODE_SELECTOR_NOT_FOUND` (4042) - check selector validity, content may still be useful

### Session Management

When receiving `ERROR_CODE_SESSION_NOT_FOUND`:
1. Clear the stored `context_id`
2. Retry the request without `context_id`
3. Store the new `context_id` from the response

### Partial Content

Some errors still return content:
- `NETWORK_ERROR`: Chrome error page HTML
- `SELECTOR_NOT_FOUND`: Full page HTML (selector just wasn't found)
- `SKIP_SELECTOR_FOUND`: Full page HTML (for analysis)

Always check the `content` field even when `success: false`.

## gRPC Status Codes

Browser Hive only uses gRPC error statuses for true infrastructure failures:

| gRPC Status | When Used | Example |
|-------------|-----------|---------|
| `OK (0)` | Always, when response can be returned | All operational errors |
| `UNAVAILABLE (14)` | Coordinator/worker unreachable | Network partition |
| `DEADLINE_EXCEEDED (4)` | gRPC timeout (not browser timeout) | Request timeout |
| `INTERNAL (13)` | Should never happen | Report as bug |

**Important**: If you see `INTERNAL` status in production, this is a bug. Browser Hive should always return `OK` with appropriate `ErrorCode`.

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

Workers expose Prometheus metrics:

```bash
curl http://localhost:9090/metrics | grep browser_hive_worker_requests_failed
```

Monitor `browser_hive_worker_requests_failed` by `error_code` label.

## Best Practices

1. **Always check `success` field first** - don't rely only on `error_code`
2. **Parse `error_message` for debugging** - contains context like context IDs, domains, timestamps
3. **Use `execution_time_ms`** - helps identify slow requests vs fast failures
4. **Implement exponential backoff** - for 5xxx errors
5. **Don't retry 4xxx errors** - except `SESSION_NOT_FOUND` and `TIMEOUT_BROWSER`
6. **Handle `SKIP_SELECTOR_FOUND` gracefully** - this is expected behavior, not a failure
7. **Log `context_id`** - helps trace session-specific issues
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
