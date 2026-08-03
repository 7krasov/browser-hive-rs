# Proxy Exit IPs and Browser Connection Reuse

Why a single page load can be served by **several different proxy exit IPs**, why that surfaces
as a CORS error that looks like a site problem, and what actually pins an IP.

Investigated 2026-07-27 against a listing page that returned HTML with unreplaced `{{ }}`
template expressions. Hosts below are anonymised: `app.example.com` is the page's own origin,
`static.example.net` the CDN host serving its bundles.

## The symptom

A scrape returns a complete, valid-looking HTML document, but the client-side framework never ran:
mustache placeholders are unrendered, the listing container is still in its `Loading` state, and
`wait_selector` times out after the full budget. `skip_selector` is not found either, because the
"no results" element is rendered by the very JavaScript that never executed.

Browser diagnostics showed four failed script loads, all with `net::ERR_FAILED`, and matching
console entries:

```
Access to script at 'https://static.example.net/.../bundle.min.js'
from origin 'https://app.example.com' has been blocked by CORS policy:
No 'Access-Control-Allow-Origin' header is present on the requested resource.
```

Everything downstream (`$ is not defined`, `SomeApi.load is not a function`, …) is a consequence.

## Why CORS applies to a `<script>` at all

It normally does not. A plain `<script src>` loads cross-origin without any CORS headers. CORS
applies only when the tag carries `crossorigin` (or `integrity`, which forces it). On this page
exactly four tags do:

```html
<script type="text/javascript" crossorigin="anonymous"
        src="https://static.example.net/.../bundle.min.js?v=..."></script>
```

Those four are exactly the four that failed. Another library from the **same host** without
`crossorigin` loaded and executed fine. Sites add the attribute to get full stack traces in
`window.onerror` (without it errors are opaque) and because SRI requires it.

## The mechanism

`crossorigin="anonymous"` does **two independent** things:

1. **Enables CORS rules** — the response must carry `Access-Control-Allow-Origin`, or it is blocked.
2. **Omits credentials** (`credentials: omit` — the literal meaning of *anonymous*): no cookies, no
   HTTP auth. Chromium marks such a request **privacy mode enabled**.

Point 2 is the one that matters here. Chromium's socket pool reuses connections by a group key
that includes the destination, **the proxy chain**, the NetworkAnonymizationKey, and **privacy
mode**. A privacy-mode request therefore *cannot* reuse the connection already open for the
ordinary requests — it opens a new one:

```
crossorigin="anonymous"
  → credentials omitted
  → privacy mode → different socket-pool group
  → new TCP connection to the proxy
  → new CONNECT tunnel
  → provider assigns a new exit IP (unless a session ID pins it)
```

This is entirely browser-side. The application cannot force or forbid connection reuse. The only
lever it has is the **proxy username**, which decides which exit IP a new tunnel receives.

### Why the error message is misleading

If the second exit IP is one the origin's CDN blocks, the response is a block/challenge page —
HTML where JavaScript was expected — and such pages carry no `Access-Control-Allow-Origin`. The
browser reports the *CORS* failure, not the 403. The log therefore accuses the site or our headers
when the real cause is the network path.

Corollary: without the attribute the same bad IP would still break the page, just differently
(block-page HTML executed as JS → `SyntaxError`). The attribute shapes the error message **and**
forces the extra connection.

## Measurements

Same page, same browser build, one variable at a time. `cf-ray` suffixes identify the Cloudflare
edge, which reveals which exit IP served each resource.

| Run | Edges observed | Note |
|---|---|---|
| Direct, no proxy | VIE only | baseline (test host is in Vienna) |
| Via proxy, **no** session ID | **SEA** + CDG | 4 `crossorigin` scripts → SEA; everything else → CDG |
| Via proxy, **with** session ID | LIS only | all 13 resources on one exit |

Two edges for thirteen resources — so the split is per **connection group**, not per request.
Neither proxy run touched VIE, which rules out the theory that privacy-mode requests bypass the
proxy: `--proxy-server` is browser-wide and the proxy chain is itself part of the group key.

Also ruled out experimentally (Chrome and Brave, with and without `Fetch`, direct and via proxy):
the `Fetch` interception domain, `Network.enable`, and Brave itself do **not** break CORS.

Ten consecutive runs without a session ID all succeeded — the IP scatter is normal and usually
harmless. It needs a bad IP to become a failure, which makes the bug load- and pool-dependent
rather than deterministic.

## What actually pins an exit IP

**Only the session ID inside the proxy username.** Not the connection, not the browser context.

Two provider-side limits mean a session ID makes one exit IP *likely*, not guaranteed. Both come
from a datacenter provider's support answers (whose own documentation contradicted itself on the
numbers), so verify them against whichever provider a scope uses:

- **Peer fallback.** A plain sticky session lets the provider silently hand out a *different* peer
  when the pinned one becomes unavailable. Some providers offer a "no fallback" modifier on the
  session, which fails the request with **HTTP 502** instead of substituting. So even a sticky
  page load can end up split across two exit IPs — less likely than without a session, but not
  impossible.
- **Idle expiry.** A session survives on the order of 1–5 minutes of idle time; assume the lower
  bound. This matters for `reusable` scopes: our default `WORKER_MAX_IDLE_TIME_SECS` is 300 s, so
  a context can sit idle, stay in the pool, keep the same session ID — and be served by a *new* IP
  on its next request. The pin expires silently; nothing in our logs marks the moment.

`tab.enable_fetch()` + `tab.authenticate()` answer the proxy's 407 challenge and nothing more —
they neither establish nor hold a connection. For a **gateway** provider, exit identity travels
exclusively through the username:

- `worker/src/browser_pool.rs` — `get_context_proxy_with_params(&metadata.id.to_string(), …)`
  builds the username, session ID = context UUID
- `worker/src/service.rs` — reads `assigned_proxy_config`, then `tab.authenticate(user, pass)`

For those providers the `--proxy-server` host:port is identical for every context in the browser,
and that is correct: one gateway fronts every exit.

Relying on connection reuse for IP stability would never have worked: Chromium opens parallel
connections, closes idle ones, and partitions pools by privacy mode.

## One proxy for the whole browser is a mode; no proxy is an accident

Two routing modes are legitimate, and the difference is not visible in a response:

| Mode | How | Who uses it |
|---|---|---|
| One proxy per **browser process** | `--proxy-server` at launch; every CDP context inherits it | every provider that does not override `assigns_proxy_host_per_context()` — including every gateway provider, where the exit is selected by the *username*, so contexts still differ |
| One proxy per **context** | `Target.createBrowserContext { proxyServer }` | providers whose pool is a list of distinct hosts (opt-in, see below) |

The first is not a degraded version of the second. For a gateway provider it is exactly right: one
host fronts every exit, and rotation happens through the credentials. As of 2026-07-31 **no
provider in either repository sets the routing flag**, so every deployed scope runs the first mode.

What is never legitimate is *neither*: a browser launched with no proxy at all. That is the one
remaining path by which a scrape can leave through the pod's own public IP, because every
in-request fallback — a tab created in the browser's default context, a slot whose recycling
failed, a tab recreated after a dead CDP session — lands on the launch proxy. If the launch proxy
exists, no request can escape it; if it does not, they all go out directly, every page loads
normally, and the only signal is the origin's blocklist learning the cluster's egress IP.

**Therefore `BrowserPool::new` refuses to start when `build_config()` yields no proxy server**,
unless the provider overrides `ProxyProvider::allows_direct_connection()` (only the base worker's
`NoProxyProvider` does, for local development). An empty or whitespace-only proxy string counts as
none: Chrome reads `--proxy-server=` as "connect directly". This also enforces what the per-context
flag documents but could not check on its own — a provider that routes per context still has to
return a usable proxy from `build_config()`, since that one carries the default context.

The misconfigurations this catches all collapse into the same empty value before the pool sees
them: an unparseable `PROXY_PORT` (previously swallowed by `.parse().ok()`, now an error naming
the variable), `PROXY_ADDRESS` without `PROXY_PORT`, an empty `PROXY_URL`, an empty pool.

Which mode is in force is logged once at startup, next to the host, so it never has to be inferred
by correlating `span_proxy_host` across requests.

## Pools of distinct hosts: per-context proxy

A provider whose pool is a list of **hosts** (dedicated IP pools) does not fit the model above,
and until 2026-07-31 it was silently broken by it. `supports_per_context_proxy()` delivers
credentials per context; the host of the returned config was discarded, so one entry was drawn at
browser launch for `--proxy-server` and carried every request until the process restarted. The
pool was advertised as rotating and used exactly one address per browser.

Two consequences, both invisible in logs at the time:

- **Credentials and host came from different pool entries.** The launch host was one random draw,
  the credentials `tab.authenticate` sent were another. Harmless only while every entry shares one
  username and password — the day they differ it is a fleet-wide 407 with nothing pointing at why.
- **One bad address took down a whole pod** until the browser died of idle timeout, instead of
  costing 1/N of requests.

`ProxyProvider::assigns_proxy_host_per_context()` (default `false`) opts a provider into real
routing: each CDP BrowserContext is created with its own `proxyServer`, so the pool is rotated per
context. Gateway providers do not set it and take the unchanged path.

### Measured before implementing (Brave 150, real pool, `api.ipify.org`)

| Launch argument | context with own `proxyServer` | plain `new_context()` |
|---|---|---|
| none | its own IP | **the machine's own IP** — this is the leak case |
| `--proxy-server=per-context` | its own IP | `ERR_PROXY_CONNECTION_FAILED` |
| `--proxy-server=<real proxy>` | its own IP | the launch proxy, unchanged |

**Decided 2026-07-31: keep launching with a real proxy** (the third row). The Puppeteer-style
`per-context` sentinel is unnecessary here and actively harmful — it leaves the default context
without a usable proxy, which breaks `shared` isolation. Launching with a real proxy keeps the
default context working, so nothing outside the opted-in path changes.

Verified end to end through `BrowserPool` itself: three `AlwaysNew` contexts reported three
distinct exit IPs with the flag on, and one shared IP with it off.

### Constraints that come with the flag

- **`ContextIsolation::Isolated` is required.** Shared isolation serves every request from the
  browser's default context, which no CDP call can re-point. The worker **refuses to start** on
  that combination rather than quietly serving one host.
- **`supports_per_context_proxy()` must be true as well.** The host comes from the config
  `get_context_proxy()` returns, and that call only happens when the other flag is set — the
  routing flag alone would leave nothing to route by and every context would fall back to the
  launch proxy, under a startup line announcing rotation. Also a **refusal to start**.
- **`build_config()` must still return a usable proxy**, because it launches the browser and
  therefore carries the default context. Enforced for every provider, not just this one — see the
  startup refusal in the previous section.
- **No fallback on failure.** If `Target.createBrowserContext` is rejected, context creation
  fails. Falling back to the plain call would put the request on the launch proxy while the logs
  claimed rotation — the original bug, restored.
- **Recycling follows the same path.** A recycled context is created with its own proxy too;
  otherwise a `reusable` pool would stop rotating after the first lifecycle tick.

### A context is only ever served from its own CDP context

Both refusals above exist because a request served from the browser's **default** context leaves
through the launch proxy while its span still names the assigned host — the failure is invisible
in exactly the way the flag was added to prevent. The same rule is enforced at the one remaining
place a tab can be created outside the normal path: a slot left without a tab by a failed
recycling.

- If the CDP BrowserContext was created and only the tab failed, the recycler **keeps the context
  id**, and the next request creates its tab back inside it.
- If the CDP context itself could not be created, the slot is **removed from the pool** on the
  next request (which fails with `BROWSER_ERROR` and can be retried); a fresh, correctly routed
  context is built for the request after it. Serving it from the default context would have
  worked, silently and wrongly, and — since `last_used_at` is refreshed before that point — the
  broken slot would never have gone idle long enough to be recycled away.

The **browser process dying** mid-request used to be the exception: the pool is replaced, the old
CDP context is gone with the process, and the request carried on in the new browser's default
context — losing isolation and, for concurrent requests recovering from the same death, sharing
that one default context between them. It is now handled the same way as everything else: the
attempt ends, and the **whole request is retried once** against a context from the new pool. The
signal is a `pool_generation` counter on `WorkerService`, bumped under the write lock in
`recreate_browser_pool` and compared across the attempt in the `scrape_page` handler.

The two conditions for a retry — generation moved **and** the response is a failure — are precise
rather than heuristic: every context in a pool lives in the same browser process, so whoever
replaced the pool, a replacement plus a failure means this request's own browser died and nothing
was scraped. A successful response is never retried, so no page is loaded twice. Exactly one
retry: a second death during it is a browser that cannot stay alive. This mirrors what
`acquire_context_with_recovery` already did when the browser died *before* a context was bound —
the correct behaviour was in the codebase, it just was not reachable once the context was held.

### Why a second CDP client

`headless_chrome::Browser::new_context()` hardcodes `proxy_server: None` and `Browser::call_method`
is private, so the parameter is unreachable through the crate. `worker/src/browser_cdp.rs` attaches
a second CDP client to `Browser::get_ws_url()` for that one call; the returned context id is handed
to the crate's own `Context`, so tabs, Fetch auth and navigation stay on the crate's transport.
Chosen over forking the crate — no fork to maintain, and the browser-level client is also the only
route to `Target.disposeBrowserContext`, which is rejected over a page session (see CLAUDE.md on
contexts that are removed from the pool but never disposed).

Two CDP clients against one browser do not race — CDP delivers a response to the connection that
sent the request, so the two id spaces are independent — but that safety rests on rules the module
documents as invariants: one call at a time on that socket (the mutex spans send *and* read, since
the read loop skips frames that are not its own), no target creation there (tab lifetime keeps a
single owner), `disposeOnDetach` left unset so contexts outlive the socket, and no CDP domain
enabled on it. Read `worker/src/browser_cdp.rs` before changing anything in it.

### Which host actually carried a request

`span_proxy_host` on the worker's `scrape_page` span, credentials stripped. It records the host
that carried the traffic, not the one the provider nominated — with the flag off those differ, and
that difference is what hid the bug. A provider that hands out non-routing hosts also logs a
one-time warning naming both.

## Consequences for session modes

Deriving `use_sticky` from the session mode — the obvious reading, and what downstream providers
did originally — is **wrong**. It treats the session ID as a property of the *context*, but what it
actually pins is one *page load*: without it, the extra privacy-mode tunnel described above draws a
fresh IP even though the context, the request and the mode never changed. `AlwaysNew` is the mode
that suffers most, and it is exactly the mode that reading gives no session to.

Enabling sticky sessions for `AlwaysNew` costs nothing in IP diversity: the session ID is the
context UUID and an `AlwaysNew` context lives for exactly one request, so each request still gets
its own IP. It only stops the scatter *within* one page load.

**Decided 2026-07-30, applied downstream** for the datacenter provider (the one deployed): sticky
in every session mode. See the production reading below for the evidence. A provider whose IPs are
fixed (dedicated pools) has no session mechanism and is unaffected; for the residential provider
the same argument applies but its per-session idle expiry and billing differ, so it was left
mode-driven pending the same billing check.

What it does and does not buy:

| | Without session ID | With session ID |
|---|---|---|
| Exit IPs per page load | 2+ | 1, unless the pinned peer drops (see the fallback note above) |
| Chance of drawing a bad IP | higher (several draws) | lower (one draw) |
| Failure shape | **partial** — document loads, scripts do not; 40 s selector timeout, plausible but useless HTML | **complete** — the document itself fails, with a status code that existing error handling surfaces and a client can retry |

It does not improve the pool's quality. A systematically blocked pool is a proxy-provider matter
(different zone, different proxy type, country settings), not a code matter.

## Context isolation is not affected

Verified separately: five isolated CDP BrowserContexts, each authenticated with its own session
ID, probed sequentially against the same host with **nothing closed in between**, so later
contexts ran while every earlier tunnel was still open and warm. Two independent runs landed in
PH/US/BD/US/US and US/AE/US/US/US. Contexts do not share each other's tunnels, so per-context
sticky sessions and `country_code` geo-targeting behave as intended, and raising
`WORKER_MAX_CONTEXTS` is safe in this respect.

Both runs also show **no concurrency ceiling** at five simultaneously open tunnels. Some exits
report the same city, which the geo probe cannot distinguish from the same IP — unremarkable in a
small datacenter pool, and neither run shows cross-context reuse.

## A proxy failure never reaches us as an HTTP status

For HTTPS the browser reaches the origin through a `CONNECT` tunnel, and the proxy's answer to
`CONNECT` is consumed by the network stack — it is not a page response. Chromium maps any non-200
`CONNECT` reply (407 aside, which triggers auth) to a **network error**, so the provider's status
code never appears in `status_code`, in `response_headers`, or anywhere JavaScript can see it.
The 502 a provider returns for an unavailable pinned peer included: we would observe
`net::ERR_TUNNEL_CONNECTION_FAILED`, never `502`.

This is convenient rather than limiting: it means proxy failure detection does **not** have to be
written per provider. The signal is Chromium's error taxonomy, which is identical whichever proxy
is configured:

| Error text | Meaning |
|---|---|
| `net::ERR_TUNNEL_CONNECTION_FAILED` | the proxy refused or failed the `CONNECT` — a dead peer, an exhausted zone, a rejected destination |
| `net::ERR_PROXY_CONNECTION_FAILED` | the proxy endpoint itself was unreachable |
| `net::ERR_PROXY_AUTH_UNSUPPORTED`, 407 loops | credentials or auth-scheme problem |
| `net::ERR_PROXY_CERTIFICATE_INVALID`, `net::ERR_SOCKS_CONNECTION_FAILED` | transport-level proxy problems |

Plain-HTTP sub-resources are the exception: there is no tunnel, so a proxy error page arrives as
an ordinary response with the provider's own status. Rare on modern sites, and not something to
build detection on.

Because these errors reach us as `Network.loadingFailed` events, a failure on a *sub-resource*
leaves the request looking successful: the document loaded, `status_code` is 200, and only the
page content is wrong. That is exactly the failure this document was written about.

**What the worker does with it** (`is_proxy_error` / `ProxyFailures` in `worker/src/service.rs`,
recorded by the response observer's existing listener):

- every request with any proxy failure logs one WARN line, whatever its outcome and regardless
  of `WORKER_ENABLE_BROWSER_DIAGNOSTICS` — on a *successful* request that line is the only trace
  that the page was served through more than one exit IP;
- the request is reported as `ERROR_CODE_PROXY_ERROR` (5007) when the main navigation failed this
  way, or when sub-resources failed this way and the request was already failing with
  `SELECTOR_NOT_FOUND` / `TIMEOUT_BROWSER`.

No retry happens inside the worker: that is the client's call. Without `-const` the provider
already substitutes a peer by itself, and a worker-side retry would double the latency while
hiding a systematically bad pool. See ERROR_HANDLING.md for the full precedence rules.

## Telling the two failure modes apart

Browser diagnostics log `corsErrorStatus` from `Network.loadingFailed`
(`worker/src/diagnostics.rs`). A CORS block otherwise surfaces as a bare `net::ERR_FAILED`, which
is indistinguishable from a connection failure. The field separates them:

| Value | Meaning |
|---|---|
| `MissingAllowOriginHeader` | a response really arrived without the header — something on the path returned a different body than the origin serves (block/challenge page) |
| `PreflightMissingAllowOriginHeader` | same, on the preflight `OPTIONS` — the response arrived, so the tunnel was alive |
| `InvalidResponse` | the fetch never completed — the tunnel itself failed, no block page was involved |

**First production reading (2026-07-30)**: `PreflightMissingAllowOriginHeader` on the XHR that
carries a listing page's data, on an `AlwaysNew` scope. The document itself loaded normally over
the page's own tunnel and the `wait_selector` was found in ~2.5 s — only the data XHR, on its own
tunnel and therefore its own exit IP, was rejected. The returned HTML was a plausible page at a
third of its normal size with the data region empty. This settled the open question in TODO.md:
the second tunnel is not failing, its exit IP is being blocked.

## Debugging checklist

When a page returns unrendered markup and only *some* sub-resources fail:

1. Check whether the failing ones carry `crossorigin` or `integrity`. If the split follows that
   attribute, this document is the explanation — not the site and not our headers.
2. Verify the origin's headers directly (`curl -H 'Origin: …' -D -`). If the CDN serves
   `Access-Control-Allow-Origin` unconditionally, the response the browser saw was not the CDN's.
3. Read `corsErrorStatus` from the diagnostics line to pick between block page and failed tunnel.

The returned `content` cannot answer any of this: a page whose bundle was blocked and a page whose
JavaScript threw are byte-identical templates.
