# Response Observers

**Response observers** capture wire-level facts about the **main navigation** — the HTTP
exchange behind the URL we open the tab with — that the rendered DOM and page JavaScript
cannot see. Today the observer captures two fields from the main-document response:
**HTTP status** and **response headers**. This document describes how it works and the
trait-based direction for adding more.

## Why this exists

The worker's `status_code` comes from the Performance API
(`performance.getEntriesByType('navigation')[0].responseStatus`, in
`worker/src/service.rs`). That JS API is convenient but deliberately limited: **it does not
expose response headers**, and there is no in-page JavaScript API that can read the raw
response headers of the navigation request (same-origin or not). The only source is the
Chrome DevTools Protocol (CDP).

So capturing headers (and, in the future, other network-level signals) requires observing
CDP network events — a fundamentally different data path from the DOM/JS one the rest of the
scraper uses. "Response observers" is the name for that data path.

## Current implementation (status + headers)

Location: `worker/src/service.rs`, in `scrape_page_internal`, set up immediately before
navigation and read just before building the response. The captured fields live in a small
`MainDocumentResponse { status, headers }` struct (deliberately a struct, not a trait — see
"Planned direction" below).

1. **Enable the CDP `Network` domain** on the tab (`Network.enable`).
2. **Register an event listener** (`tab.add_event_listener`) for `Network.responseReceived`.
3. **Filter to the main document**: keep only events where
   - `Type == ResourceType::Document`, and
   - `frame_id == tab.get_target_id()` — the top-level frame id equals the target id, so
     this excludes sub-resources (images, CSS, XHR…) **and** iframe documents.
4. **Last matching response wins**: on redirects the intermediate 3xx responses are delivered
   via `requestWillBeSent.redirectResponse` (which this observer does **not** listen to), so
   `responseReceived` only fires for the **final** document — no ordering ambiguity, its
   `status` and `headers` are the final page's.
5. **Store** `{ status, headers }` into a shared `Arc<Mutex<Option<MainDocumentResponse>>>`.
6. **After navigation + wait**, drain the holder into `ScrapePageResponse.status_code` and
   `ScrapePageResponse.response_headers`.
7. **Clean up** via `EventListenerGuard` (RAII): the listener is removed on drop on **every**
   exit path (success, navigation timeout, cancel, panic). This matters because tabs are
   **reused** across requests in `reusable`/`reusable_preinit` modes — without removal,
   listeners would accumulate on the shared tab.

**Status source and fallback**: `status_code` is taken from the observer when it captured a
response (`status > 0`). When it captured nothing — the `Network` domain could not be enabled,
or the navigation produced no response (e.g. a `chrome-error://` page after a connection/DNS
failure) — it **falls back** to the Performance API
(`performance.getEntriesByType('navigation')[0].responseStatus`), returning 0 if still
unknown. This keeps behaviour identical to the pre-observer code in every no-response case.

Failure to enable the domain or add the listener is **non-fatal**: it is logged at DEBUG and
the request proceeds with the Performance-API status and empty `response_headers`.

> Note: the 403 early-exit inside the wait strategy (`common/src/wait_strategy.rs`) still
> reads status via its own Performance-API eval per poll — it was intentionally **not** moved
> to the observer (that is change "B": it alters production anti-bot timing and couples
> `common` to the observer; deferred).

### End-to-end path

`worker` populates `response_headers` → the coordinator forwards it unchanged on the success
path (`coordinator/src/service.rs`, `response_headers: worker_response.response_headers`) →
the client receives it in `ScrapePageResponse.response_headers` (field 6 in both
`worker.proto` and `coordinator.proto`). Error paths return empty headers.

## Design decision: `Network` domain, not `Fetch` (recorded 2026-07-23)

**Decision**: use the `Network` domain with a client-side filter, **not** the `Fetch` domain
with a browser-side pattern.

**Context**: `Network.enable` is all-or-nothing — the browser emits events for *every*
resource (`requestWillBeSent`, `responseReceived`, `dataReceived`, `loadingFinished`), not
just the main document. Our listener discards all but the main-document response, but the
full stream still crosses the CDP websocket and is deserialized in the worker.

The only way to filter **in the browser** (so only the document response produces an event)
is the `Fetch` domain with a `{resourceType: Document, requestStage: Response}` pattern.

**Why we did not use `Fetch`**:

| | `Network` (chosen) | `Fetch` with Document/Response pattern |
|---|---|---|
| Browser-side filter | no (filter in worker) | yes, only the document event |
| Wire overhead | all network events (~a few ms CPU/request) | document only |
| Code complexity | low, isolated | high |
| Risk | minimal | must pause **and resume** the document request — a mistake hangs the page |
| Proxy-auth interaction | none (different domain) | conflicts: single per-tab interceptor slot + existing `enable_fetch(None, true)` for auth must be merged |
| Tab reuse | fine | interceptor persists on the tab and must target the *current* request's storage |

The measured `Network` overhead is modest: Chrome emits only additive event messages, each a
few microseconds to parse and dispatch to our single listener — on the order of a few
milliseconds of CPU per request, against page loads that take hundreds of ms to seconds
(< 1%). The `Fetch` approach saves that at the cost of real complexity and a page-hang risk
near the delicate proxy-auth / tab-reuse machinery. Not worth it for one signal.

**Revisit if**: profiling shows the network-event volume actually hurts throughput, *or* we
start capturing many signals and the per-event cost compounds.

## Planned direction: a `ResponseObserver` trait

Today the logic is inline because there is exactly one signal (YAGNI — we deliberately did
not build an abstraction for a single observer). When a **second** signal is added, extract a
trait that parallels the existing `WaitStrategy` trait and the middleware vectors:

```rust
// Sketch — not yet implemented.
trait ResponseObserver: Send + Sync {
    /// Called for each main-frame Document response during the request.
    fn observe(&self, event: &ResponseReceivedEvent);
    /// Fold the collected signal into the outgoing response.
    fn into_response(self: Box<Self>, resp: &mut ScrapePageResponse);
}
```

Observers would be held in a `Vec<Box<dyn ResponseObserver>>` on `ScopeConfig` (like
`binary_params_middlewares` / `tab_init_middlewares`), configured in the base worker, and the
single `Network.enable` + listener would fan events out to all of them.

### Candidate future signals

- **Authoritative HTTP status** — ✅ done (see above); the final `status_code` now comes from
  the observer with a Performance-API fallback. The wait-strategy 403 early-exit still uses
  its own JS eval (change "B", deferred).
- **Proxy exit IP** — `response.remote_ip_address` is already in the event; this could
  replace the JS-based `check_proxy_exit_ip` probe (`worker/src/service.rs`), which adds ~1s.
- **Off-domain redirect** — ✅ done (see section below).
- **Full redirect chain** — the intermediate 3xx hops (each `Location` + status) arrive via
  `requestWillBeSent.redirectResponse`, a different event not observed today. A superset of
  the off-domain check, useful for the recurring "why was the selector not found?" debugging.
  Still future — see TODO.md.

## Off-domain redirect detection (`ERROR_CODE_REDIRECT_TO_ANOTHER_DOMAIN`, 4050)

When the main navigation lands on a **different registrable domain (eTLD+1)** than requested,
the worker returns `ERROR_CODE_REDIRECT_TO_ANOTHER_DOMAIN` instead of a `wait_selector` /
`skip_selector` outcome — those selectors are meaningful only on the target site, so running
them against a foreign page would produce misleading `SELECTOR_NOT_FOUND` / `SKIP_SELECTOR_FOUND`
results. Same-site redirects (same eTLD+1, e.g. `www.example.com` → `shop.example.com`) are
allowed and processed normally.

Implementation (`worker/src/service.rs`):

- **Landing URL**: taken from the observer's captured `url` (the final main-document
  response URL from the network layer), falling back to `tab.get_url()` if the observer
  captured nothing. This covers both HTTP and JS (`location.href`) redirects, since each
  triggers a new top-level document load.
- **Same-site test**: `cross_site_redirect_target()` compares the registrable domain of the
  requested vs landing host using the `psl` crate (compile-time Public Suffix List). This is
  correct for multi-label suffixes (`example.co.uk`), unlike a naive host or last-two-labels
  comparison. It errs toward *not* flagging when a URL/host/eTLD+1 cannot be determined.
- **Decision point**: after the wait strategy runs (`navigate_to` returns before redirects
  are followed, so the final URL is only known once the page has settled). The check
  **overrides** the wait result — priority is `navigation error > off-domain redirect > wait
  result` — so no selector error surfaces. A DEBUG line
  `Off-domain redirect detected: <from> -> <to>` is logged.
- **Returned fields**: `status_code` and `response_headers` are the **landing** page's
  (final status, e.g. 200). The redirect (3xx) status is not returned — it would require the
  full-redirect-chain observer (future).

**Known trade-off**: because the check runs *after* the wait strategy, a request with a
`wait_selector` that redirects off-domain still spends the wait budget polling for the
selector on the foreign page before being overridden. Moving the check *before* the selector
phase (inside the wait strategy) would avoid that but couples `common` to the observer — see
TODO.md.
- **Negotiated protocol** — `response.protocol` (h2/h3), useful for fingerprint/debugging.
- **Response cookies** — `Set-Cookie` handling for session diagnostics.
- **Cache provenance** — `from_disk_cache` / `from_service_worker` flags.
- **Anti-bot signals** — surface `cf-ray`, `server`, challenge headers for detection metrics.
- **Response body for non-HTML** — API/JSON endpoints where the DOM is not the payload.

Some of these (status, exit IP, anti-bot signals) could also feed Prometheus metrics /
diagnostics, not only the gRPC response — see METRICS.md.

## Trade-offs and gotchas

- **Overhead is per request and unconditional.** Every scrape now enables the `Network`
  domain. See the design decision above; gate behind a config flag (mirroring
  `WORKER_ENABLE_BROWSER_DIAGNOSTICS`) only if this becomes a measured problem.
- **Header values are strings.** Multi-value headers that Chrome joins (e.g. `Set-Cookie`)
  arrive newline-joined in a single map entry; non-string JSON values fall back to their
  JSON string form.
- **Empty is a valid result.** Clients must treat missing `response_headers` as "not
  captured", not as an error.
- **Headers describe the wire response, not the returned `content`.** They are forwarded
  verbatim, exactly as the browser received them — deliberately, since the raw values are
  themselves a useful signal (anti-bot fingerprints, CDN provenance, encoding negotiation).
  This means `content-length` and `content-encoding` must **not** be used to reason about
  `content`:
  - `content-encoding: gzip` (or `br`, `deflate`, `zstd`) reports what the *server* sent.
    The body was already decompressed by Chrome long before we read it.
  - `content-length` is the length of the **compressed** body (per RFC 9110 it measures the
    body *after* Content-Encoding is applied), and Chrome does not rewrite it after
    decompressing. It is frequently **absent** altogether — chunked transfer-encoding, HTTP/2
    and HTTP/3 responses usually carry no `content-length` at all.
  - `content` is neither of those: it is `document.documentElement.outerHTML` read **after**
    JavaScript execution, so its size matches neither the compressed nor the original
    uncompressed body.

  A `content-length` that disagrees with `content.len()` is therefore expected and is **not**
  a sign of a truncated response. Clients needing the size of what they received must measure
  `content` themselves. (The actual transferred byte count is available in CDP as
  `encodedDataLength`, which this observer does not capture today.)
