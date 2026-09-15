# TODO

Open items that need investigation or a decision. Remove an item once it is resolved.

## Tune the block quarantine cooldown from real block durations

**Status**: open, waiting for production numbers (raised 2026-09-02)

`WORKER_BLOCK_QUARANTINE_SECS` defaults to 300 s in `reusable` (SESSION_MODES.md). That number is a
compromise with no measurement behind it: long enough for a rate limit computed over a short window
to expire, short enough not to hold a context out of a small pool for a whole shift.

To replace the guess: after a quarantine expires, the next request to that origin either succeeds
(the block was shorter than the cooldown) or is refused again and re-arms it. Count the re-arms per
context+origin — a high rate means the cooldown is too short for this origin, a cooldown that never
re-arms means it can come down. Only then consider a backoff, which was deliberately left out
because it needs failure history the pool has no other use for.

## A claimed-slot-seconds counter for `dedicated` autoscaling

**Status**: open, low priority (raised 2026-09-02)

Pool gauges are recomputed inside the Prometheus scrape handler (`refresh_pool_gauges`), so they
report occupancy **at that instant**, not over the interval. `always_new`/`reusable` have a
rate-based alternative — `sum(rate(browser_hive_worker_request_duration_seconds_sum[1m]))` is mean
requests in flight, in the same unit a slot threshold already uses, and immune to scrape sampling.
`dedicated` autoscales on `claimed_contexts`, which has no such equivalent: a slot stays claimed
between a session's requests.

The sampling error there is small (slots are held far longer than a scrape interval), which is why
this is not urgent. If one signal that is correct in every mode is ever wanted, it would be a
monotonic counter of claimed-slot-seconds alongside the gauge — computable exactly at scrape time
from the contexts' own timestamps plus an accumulator for the ones already removed.

## Align the coordinator's session TTL with `dedicated`'s idle removal

**Status**: open, deliberately unresolved (raised 2026-08-03, still open after `dedicated` shipped)

`SessionManager` defaults to a 30-minute TTL (`common/src/session.rs`) while a `dedicated` context
is removed after a minute idle. A client returning from a two-minute pause therefore presents a
session id the coordinator still accepts and the worker no longer has → `SESSION_NOT_FOUND`. The
answer is correct, but it makes that code a routine outcome rather than an exception.

Not trivial to fix: the coordinator has one `SessionManager` for all scopes and knows neither a
scope's session mode nor its idle timeout. Decide it with real numbers on how often clients
actually pause — not up front. See SESSION_MODES.md.

## Measure the gateway provider's sticky TTL from context deaths

**Status**: open — the log line it needs now exists (raised 2026-08-03)

A gateway provider's sticky session has a TTL it does not publish. When it expires the exit IP
changes underneath a live context, the origin answers the now-mismatched cookies with a 403, and
the client drops the session — the system self-corrects at a cost of one wasted page load, which
is why `max_lifetime` is deliberately left at its default rather than guessed (SESSION_MODES.md).

To replace the guess with a number: collect the **context age** from the `Releasing dedicated
context … after HTTP 403` lines of any `dedicated` scope — `WORKER_DESTROY_SESSION_ON_BLOCK`
defaults to `true` there, so the lines are already being emitted. If those ages cluster, the cluster is the provider's sticky TTL and `max_lifetime` can be
set just below it. Note this is a per-provider fact — a dedicated IP pool
has no sticky TTL and needs no ceiling.

## WebRTC can reveal the pod's real IP — flag not set, effect on blocking unknown

**Status**: open, needs a decision backed by a measurement (raised 2026-07-31)

`--proxy-server` and `Target.createBrowserContext { proxyServer }` govern the TCP/HTTP stack only.
WebRTC gathers ICE candidates over **UDP, outside the proxy**: a page that opens an
`RTCPeerConnection` against a STUN server receives a host candidate (the pod's IP) and a
server-reflexive candidate (the cluster's public egress IP). Anti-bot vendors do probe this. It is
the one real-IP disclosure path left after the startup guard (see PROXY_NETWORKING.md, "One proxy
for the whole browser is a mode"), because it does not go through the network stack the proxy sits
in.

Nothing in the current launch arguments addresses it: neither `DefaultBinaryParamsMiddleware` nor
`BraveBinaryParamsMiddleware` (`common/src/browser_middleware.rs`) nor `headless_chrome`'s own
`DEFAULT_ARGS` sets a WebRTC policy, and `--disable-brave-extension` additionally turns off the
browser's built-in protection.

**The decision is not obvious, which is why this is a TODO and not a patch.**
`--force-webrtc-ip-handling-policy=disable_non_proxied_udp` closes the leak, but a browser that
refuses non-proxied UDP is itself a fingerprintable deviation from a normal consumer browser — it
may raise the block rate, i.e. make the anti-bot situation worse in exchange for hiding an IP that
most scraped origins never probe. Neither direction should be assumed.

**How to decide**:

1. Measure exposure first — does a page actually get a usable srflx candidate from inside the
   pod? (UDP egress may already be blocked by the cluster.) If not, there is nothing to fix.
2. If it is exposed, ship the flag as an **opt-in env var**, enable it on one scope, and compare
   block rates against an identical scope without it.
3. Consider the alternative that costs no fingerprint at all: a NetworkPolicy allowing egress
   only to the proxy hosts over TCP. That closes this *and* every future variant of the same
   problem, and it fails closed. It belongs to DevOps (`ops/deployment-chart` has no
   NetworkPolicy today).

## Confirm sticky proxy sessions are not metered separately

**Status**: open, non-technical (raised 2026-07-27; nothing further is needed in code)

A billing question for whoever owns the proxy contract: whether the plan meters sessions
separately from plain requests. Sticky sessions are on in every session mode for the deployed
datacenter provider (reasoning and evidence in PROXY_NETWORKING.md, "Consequences for session
modes"), and `AlwaysNew` opens one session per request — so if sessions are metered, the request
count and the session count are now the same number. Billed per session **and** per request, this
doubles the line items without changing the traffic.

Also worth passing on if the residential provider is ever deployed: it is still mode-driven, and
turning it sticky has the same argument but a different cost profile (session idle expiry on the
order of minutes, see PROXY_NETWORKING.md).

## `rotation_strategy` is hardcoded to `Hybrid` — decision or oversight?

**Status**: open question, left as-is deliberately for now (raised 2026-07-29)

Both worker binaries — the base `crates/worker/src/main.rs` in this repo, and the custom binary
in the downstream repo that builds its own `ScopeConfig` — set
`ContextLifecycleConfig::rotation_strategy` to `RotationStrategy::Hybrid` as a literal. It is the only lifecycle field not read from the environment, and no manifest sets
it. No prior note explains whether that is a decision or simply never finished, which is why this
item exists.

`should_recycle_context` (`worker/src/browser_pool.rs`) makes the strategy decide *which thresholds
are consulted at all*:

| Strategy | Honoured | Silently ignored |
|---|---|---|
| `TimeBasedOnly` | `max_lifetime` | `max_requests`, `max_idle_time`, `max_cache_size_mb` |
| `RequestBasedOnly` | `max_requests` | `max_lifetime`, `max_idle_time`, `max_cache_size_mb` |
| `Hybrid` | `max_lifetime`, `max_requests`, `max_idle_time`, OR-ed | `max_cache_size_mb` — inert for another reason, see the next item |

**Argument for leaving it hardcoded**: `Hybrid` is the only value under which the other lifecycle
knobs mean anything. Exposing it as `WORKER_ROTATION_STRATEGY` creates a knob that can silently disable
three other knobs — `time_based_only` would turn `WORKER_MAX_IDLE_TIME_SECS` and
`WORKER_MAX_CACHE_SIZE_MB` into no-ops with nothing in the logs to say so. The three restrictive
modes have no known use case; the base enum offers them, nobody asked for them.

**Trigger to act**: a concrete scope that must *not* recycle on idle or cache size (e.g. a
long-lived logged-in session that should survive idle periods). Then expose it, and log the
resolved strategy at startup next to the thresholds it disables.

**Alternative worth considering instead**: drop the enum and let each threshold be disabled by
setting it to zero/unset. That expresses the same intent without one variable overriding others.

**Update 2026-08-03**: `dedicated` depends on `max_idle_time` being honoured — it is the only
mechanism that releases a claimed slot — so `ScopeConfig::validate()` now **rejects** `dedicated`
combined with any non-`Hybrid` strategy, and **warns** for `reusable` (SESSION_MODES.md). The
strategies are therefore no longer silent, but the underlying oddity stands: an enum where two of
three values disable other configuration, still hardcoded, still unexposed. The argument above is
unchanged.

## `max_cache_size_mb` is a threshold nothing can ever cross

**Status**: open, confirmed by reading the code (raised 2026-08-11)

`BrowserContextMetadata::cache_size_mb` (`common/src/types.rs`) is initialised to `0` and is
**never written** anywhere in the workspace — the only readers are
`should_recycle_context` (`worker/src/browser_pool.rs`) and one diagnostic log line in
`worker/src/service.rs`. So `cache_too_large` is permanently `false`: `Hybrid` effectively ORs
**three** predicates (age, requests, idle), not four, and `WORKER_MAX_CACHE_SIZE_MB` /
`max_cache_size_mb` (default 500) is inert configuration in every session mode.

This is not currently harmful — no scope relies on cache-driven rotation, and `max_idle_time`
recycles a `reusable` context long before its disk cache could matter. It is on this list because
the setting *reads* as working: CLAUDE.md, SESSION_MODES.md and the strategy table above all list
it next to the three thresholds that do fire, so anyone tuning it would get no rotation and no
warning.

**Three ways out**, in increasing cost:

1. **Delete it.** Drop the field from `ContextLifecycleConfig`, the predicate from
   `should_recycle_context`, and the mentions from the docs. Honest, and the strategy table
   collapses to three knobs.
2. **Populate it** from CDP — per-context disk/memory usage is not directly exposed;
   `Network.getResponseBodyForInterception` accounting or `Browser.getBrowserCommandLine`-era
   cache-size APIs do not give it either, so this needs research before it is promised.
3. **Leave it and mark it inert** in `ScopeConfig::validate()`, the way non-`Hybrid` strategies
   are already warned about — cheapest, and consistent with the existing "never inert in silence"
   rule.

The **documentation half is done** (2026-09-09): CLAUDE.md, SESSION_MODES.md, the
`rotation_strategy` table above and the warning text in `ScopeConfig::validate()`
(`common/src/config.rs`) all now say the threshold is inert and that `Hybrid` effectively ORs
three predicates. What is left here is the code decision — one of the three options above.

## `headless_chrome` is pinned to a fork commit

**Status**: open until upstream PR rust-headless-chrome#568 is released (raised 2026-09-15)

The workspace `Cargo.toml` takes `headless_chrome` from `tungs0ul/rust-headless-chrome` at rev
`a321dae1`, which is upstream **1.0.22 plus exactly the three commits of #568** (verified with the
GitHub compare API: ahead 3, behind 0). A rev rather than a branch, so the code cannot change under us.

**Why.** In every published version up to 1.0.22:
- a tab's session listener is never removed from the transport, so after the tab is closed its event
  thread blocks on `recv()` for the browser's lifetime and keeps all of the tab's state reachable —
  one thread and one tab's state per tab ever opened, which in `always_new` is one per request;
- the tab's `received_event_params` map keeps the params of every `Network.responseReceived` —
  every sub-resource, headers included — and is never pruned. The response observer enables
  `Network` on every request, so a `reusable` tab accumulates this for its whole life;
- a late response to a call that already timed out, or a send to a listener whose thread has ended,
  breaks the transport's message loop — which the pool can only see as a dead browser.

#568 fixes all three (`Drop for Tab` unregisters the listener, the map is pruned on
`loadingFinished`/`loadingFailed`, timed-out calls are unregistered).

**Measured before the switch** (downstream production, 1.0.18): the `worker` process grew ~20 MiB/h
on one `reusable` pod and 84 → 175 MiB between minute 14 and minute 31 on a busy one. Not what
triggers the OOMKills — renderer processes are — but unbounded. Re-measure after deploying.

**Measured after the switch** (2026-09-15, same busy scope): 12 → 22–33 MiB after ~1 h, against
83 → 171 MiB at minutes 14–31 before. Still rising in small steps; the busy scope's pods do not live
long enough (OOMKills) to show whether it levels off, so read a quieter scope over 12 h+.

**What else came with 1.0.18 → 1.0.22**, for tracing a regression: a regenerated CDP protocol (new
optional fields in `Network.enable` and `Target.createTarget`, set to `None` here, so the requests on
the wire are unchanged); `wait_for_initial_tab` waits 20 s instead of 10 s (not called here);
`tungstenite` 0.29, which our own pin followed.

**Costs of the pin**: the fork can disappear — clean builds would then fail loudly; mirror the same
rev under our own account if that happens. A crate with a git dependency cannot be published to
crates.io.

**To close**: once a release contains #568, switch back to the version from crates.io, keep
`tungstenite` on the version it resolves (see the comment in `Cargo.toml`), and remove this item.

## Worker OOMKills are driven by renderer processes

**Status**: investigating (raised 2026-09-15)

A busy downstream `reusable` scope (2 GiB limit, `max_contexts = 2`, Brave headless, isolated
contexts) is OOMKilled repeatedly — one container lived 6 m 44 s. PSS snapshots of one pod:

| container age | renderers | renderer PSS | `worker` RSS | `worker` threads |
|---|---:|---:|---:|---:|
| ~3.5 min | 21 | ~1.47 GiB (the two tab renderers: 511 + 418 MiB) | — | 11 |
| 14 min | 15 | 898 MiB | 83 MiB | 12 |
| 31 min | 28 | 1.18 GiB | 171 MiB | 14 |

A tab's renderer reaches 400–500 MiB within 2–3 minutes, so the memory is not a slow growth with
tab age, and neither `max_lifetime` nor a per-tab rotation can be the main fix. A lower
`WORKER_MAX_LIFETIME` did not prevent the kills.

**Leading hypothesis, unconfirmed**: out-of-process iframes. Headless keeps site isolation on
(`--disable-features=IsolateOrigins,site-per-process` is added only in headful), so every
third-party site framed by a page gets its own renderer; `renderer-client-id` reached ~143 within
3.5 minutes of browser start. Manual confirmation failed: the image has no `curl`, and a raw
`/json/list` request over `/dev/tcp` returned 0 bytes.

Plan, in order:
1. **Deploy the browser resource gauges and read them** (implemented, not yet deployed; see
   METRICS.md, "Browser resources"): processes and PSS per Chromium process type, CDP targets per
   type, browser contexts, the worker's own RSS and threads. Also confirm there that the first
   production deploy exposes them: `/proc` readable by the worker, and the target probe connecting.
   The `iframe` target type has only been verified against a local Chrome, not Brave.
   **Deployed 2026-09-15**: `/proc` and the target probe work in production, but every process was
   classified as `browser` (Chromium rewrites its process title, so `cmdline` is space-joined, not
   NUL-separated). Fixed in `classify_process`; the per-type process gauges need a redeploy.
   First target readings (busy `reusable` scope, 8 pods): ~37 processes, ~3.4 `page`, ~4 `iframe`,
   ~3 `worker` targets per pod on average; browser contexts go 2 → 4 when lifecycle recycling runs.
   An `always_new` scope showed 0 `iframe` targets, which is **not** evidence of no iframes: the
   gauge is read once per scrape, and a frame lives only while its request runs (locally ~1.5 s).
   `browser_hive_worker_iframes_total` (the item below) counts them.
2. Confirm or reject the iframe hypothesis from those numbers. If confirmed, the options are:
   blocking third-party hosts (`BlockedUrlsMiddleware`), turning site isolation off (a
   memory-vs-stealth trade-off — measure the block rate), fewer contexts per pod, or a larger limit.
3. Dispose of empty CDP contexts (the item below). It is suspected of growing NetworkService, not
   confirmed; the browser-context gauge from step 1 decides it.
4. Per-tab rotation (close and reopen the tab inside the same CDP context every N requests) is
   deprioritised by the data above; revisit only if renderer PSS still grows with tab age once the
   fast part is explained.

Also seen, unexplained: an almost idle `always_new` pod held 0.3 → 1.4 CPU cores for hours, which
dropped on restart. On the busy pod, gpu-process (swiftshader software rendering) showed 21 % CPU.
The target gauges from step 1 are the first thing to check there.

## Third-party request and iframe metrics

**Status**: implemented 2026-09-15, not deployed. What the metrics count, the label rules and the
decisions (full hosts, requested-host attribution, keeping `page_site` under a cap) are in the
"Third-party hosts and iframes" section of METRICS.md.

Goal: find the hosts worth adding to `BlockedUrlsMiddleware` and rank them.

**Verified locally** (Chrome, macOS, headless, HTTP, no proxy, base worker):
- A cross-site sub-resource of the main page was counted. A same-site one and one inside a
  cross-site iframe were not.
- Iframes, nested ones included, were attributed to the requested site. After an off-domain
  redirect, both loads and frames stayed on the requested host.
- Redirect side effect: the redirect target's own resources, and its `favicon.ico`, count as third
  party. Accepted by the user (2026-09-15): the host still appears under the requested site, so the
  noise is readable.
- **The agreed 5 s sampling interval counted 0 of 8 frames**, in `always_new` and in `reusable`: a
  frame lives only while its page is loaded, about 1.5 s here. At 1 s it counted 8 of 8. The
  interval is now 1 s (confirmed by the user, 2026-09-15).

**Open.**
- **After the deploy**, compare `prometheus_tsdb_head_series` with the reading before it. On
  2026-09-15 it was 1.26 M, with a 2.6–4.7 M sawtooth earlier that day before Prometheus moved
  pods. Lower `WORKER_THIRD_PARTY_METRICS_MAX_SERIES` (default 2000) if the growth is too much.
- **Not verified end to end**: the blocked counter (the base worker has no list; unit-tested only),
  Linux, Brave, HTTPS, a proxy.
- **Frames shorter than 1 s are still missed.** The exact alternative is event-driven:
  `Target.setDiscoverTargets` on a dedicated socket, reading `targetCreated` /
  `targetInfoChanged`. It needs an event-reading client, which `browser_cdp.rs` is not (invariant
  5). Not done.
- **Deferred**: memory per requested site. No way was found to map a tab to its renderer process.
  When revisited, the user wants metrics, with the same cardinality problem at 800 sites.

## `BlockedUrlsMiddleware` cannot block iframes

**Status**: open, decision pending (raised 2026-09-15)

Measured locally against Chrome and Brave on macOS, headless, over plain HTTP with no proxy. The
page loaded one cross-site iframe that nested two more, and the list blocked one host. Server hits
were checked, not only the CDP events.

| | site isolation on (headless default) | site isolation off |
|---|---|---|
| sub-resource of the main page on a blocked host | blocked | blocked |
| iframe **document** on a blocked host, at any depth | **loaded** | **loaded** |
| sub-resource inside a cross-site iframe | **loaded**, invisible to the page session | blocked, visible |
| `Network.requestWillBeSent` on the page session | only the main page's direct child frames | every frame, nested included |
| `iframe` targets from `Target.getTargets` | every cross-site frame, nested included | none (frames run in-process) |

Consequences:
- The list cannot reduce renderer processes, which is what the OOMKills are made of.
- An iframe-host metric fed from page-session events misses every frame nested inside a
  cross-site frame.

Candidates, all unverified:
- Turn site isolation off in headless (headful already does).
- `Fetch` interception of `Document` requests, which must coexist with the `Fetch` handler
  `tab.authenticate` installs.
- `Target.setAutoAttach` to reach the child frames' sessions.

Not yet checked on Linux, over HTTPS, or through a proxy.

## Shutdown does not drain in-flight requests

**Status**: open, agreed, not started (raised 2026-09-15)

- `shutdown_signal` (`worker/src/lib.rs`) cancels the token **before** waiting, so SIGTERM aborts
  in-flight requests with `TERMINATING`. The coordinator retries those only with ≥ 10 s of
  deadline left, from scratch.
- During a `preStop` sleep the worker still reports itself healthy (`health_check` is
  `is_ready && !cancelled`, and SIGTERM arrives only after preStop), so the coordinator keeps
  routing new requests to a pod that is about to cancel them.
- Between the gRPC server closing and the next health round, connects fail as
  `WORKER_UNREACHABLE`, which is not retried.

Proposed order on SIGTERM: report unhealthy → wait at least one coordinator health round → wait
for in-flight requests with a bound → cancel what remains → exit. The bound must be configurable
and fit inside `terminationGracePeriodSeconds`; `GRPC_REQUEST_TIMEOUT` is 320 s, so it cannot
simply wait for the longest possible request. Today this costs requests on every rollout, KEDA
scale-down and spot preemption. A separate change from the memory work.

## Empty CDP BrowserContexts are never disposed

**Status**: residue of the tab-leak fix; needs an upstream change (raised 2026-07-27)

Tabs are now closed at every context-removal site (`close_tab_detached` in
`worker/src/browser_pool.rs`), so the per-request tab leak in `AlwaysNew` is gone. What remains is
the empty CDP BrowserContext behind each closed tab: `Target.disposeBrowserContext` is rejected
over a page session (`Not allowed`), and headless_chrome exposes no browser-level method call
(`Transport::call_method_on_browser` exists but is unreachable from the public API).

Low priority — an empty context holds no renderer, no sockets and no proxy tunnel, which is what
the memory and connection pressure actually came from.

**The blocker is gone as of 2026-07-31**: `worker/src/browser_cdp.rs` is exactly the "raw CDP call
over the browser WebSocket" this item was waiting for, added for per-context proxy hosts. Disposing
a context is now a matter of calling `Target.disposeBrowserContext` through that client at the
removal sites — with the caveat that the client only exists for providers that route per context,
so it would have to be created unconditionally first.

## Per-context proxy hosts are verified on macOS Brave only

**Status**: open (raised 2026-07-31, when the feature was added)

`assigns_proxy_host_per_context()` was measured against **Brave 150.1.92.144 on macOS**: three
`AlwaysNew` contexts driven through `BrowserPool` reported three distinct exit IPs, and one shared
IP with the flag off. Production runs Brave from `apt stable` inside the worker image, which is the
same Chromium line but not the same build, and the image does not pin a version.

`Target.createBrowserContext { proxyServer }` is Chromium behaviour, so the risk is low — but the
failure mode is quiet in the wrong direction: a build that ignores the parameter would route every
context through the launch proxy while the logs report per-context hosts. Run the same check inside
the image before the first scope opts in, and treat `span_proxy_host` in production as the standing
verification (a scope that rotates should show many distinct values).

No provider in this repository sets the flag; the first user is a downstream dedicated-IP pool.

## `country_code` is silently ignored by providers that cannot geo-target

**Status**: open (raised 2026-07-31)

`ProxyParams::requires_dedicated_context()` (`common/src/request_context.rs`) is derived from the
request alone: any `country_code` forces a new dedicated context in `reusable`,
because country affects connection identity. For a provider that cannot act on it at all — a
dedicated IP pool, where geography is a property of the purchased addresses — this is pure cost:
the scope loses a warm idle context and pays for a new one, and the client's parameter still does
nothing.

Two halves, both open:

- **Cost**: gate the decision on provider capability (e.g. `supports_country_targeting()`,
  default `true` so gateway providers are unaffected) instead of on the request alone.
- **Honesty**: a client sending `country_code` to a provider that ignores it gets no signal. At
  minimum a warning naming the provider; an error is arguably more correct but needs an error code
  and a client change, so it is not obviously worth it.

## Generalize the response observer into a `ResponseObserver` trait

**Status**: deferred by design; inline struct is fine for now (raised 2026-07-23)

The response observer in `worker/src/service.rs::scrape_page_internal` (CDP `Network` domain →
main-document `responseReceived`) currently captures two fields into a `MainDocumentResponse`
struct: HTTP `status` and `response_headers`. This is intentionally **not** abstracted into a
trait yet — two fixed fields do not justify one (YAGNI). See RESPONSE_OBSERVERS.md.

**Trigger to act**: when signals become **pluggable per-scope** or numerous. Candidates:
proxy exit IP (`response.remote_ip_address`, replacing the ~1s JS `check_proxy_exit_ip`
probe), redirect chain (see next item), negotiated protocol (h2/h3), response cookies,
cache-provenance flags, anti-bot headers (`cf-ray`, `server`), response bodies for non-HTML
endpoints.

**Then**: extract a `ResponseObserver` trait (parallel to `WaitStrategy` and the middleware
vectors), hold `Vec<Box<dyn ResponseObserver>>` on `ScopeConfig`, and fan the single
`Network.enable` + listener out to all observers. Sketch in RESPONSE_OBSERVERS.md.

## Redirect follow-ups

**Status**: both optional (raised 2026-07-23, when off-domain detection shipped)

Off-domain redirect detection itself is implemented and documented in RESPONSE_OBSERVERS.md.
What is left:

1. **Avoid wasting the wait budget on the foreign page.** The check currently runs *after*
   the wait strategy, so a request with a `wait_selector` that redirects off-domain still
   polls for the selector on the foreign page until timeout before being overridden. Moving
   the check *before* the selector phase (inside `NetworkIdleStrategy` Phase 1→2) avoids the
   waste but couples `common` to the observer / requested URL (change of the `WaitStrategy`
   signature). Only worth it if this waste shows up in practice.
2. **Full redirect chain.** Capture every hop (`Location` + per-hop status) via a
   `requestWillBeSent.redirectResponse` observer — a superset of the off-domain check, useful
   for the recurring "why was the selector not found?" debugging (answer is often: redirected
   off-site) and to optionally return the 3xx status instead of the landing status.

## Possible false-positive selector match on the previous page (wait_strategy)

**Status**: needs verification, deferred (raised 2026-07-20)

`NetworkIdleStrategy::wait` runs its first `check_selector_exists` call immediately at the
top of the Phase 1 loop, while `wait_until_navigated()` is still running in a background
thread. Where a context outlives its request (`WORKER_SESSION_MODE=reusable` / `dedicated`) the tab
is carried over from the previous request, so if the document has not been swapped to the
new page yet, `skip_selector` (or the 403 early-exit probe) could match content from the
*previous* page and abort the request with `ERROR_CODE_SKIP_SELECTOR_FOUND`.

Unverified: it is not confirmed whether `headless_chrome`'s `navigate_to()` guarantees the
new navigation is committed before it returns. If it does, this cannot happen and the item
can be dropped.

**How to check**: in a reusable context, scrape page A containing the skip element, then
page B without it, using a `skip_selector` that only matches A. A false
`SKIP_SELECTOR_FOUND` on B reproduces the bug.

**Possible fix if confirmed**: compare `document.location.href` (or a navigation loader id)
against the requested URL before trusting the first selector probe, or delay selector
checks by one poll interval.

## AlwaysNew context leak: end-to-end coverage is missing

**Status**: unit-tested at the logic level, integration coverage deferred (raised 2026-07-20)

The leak fix (`AlwaysNewContextGuard` in `worker/src/service.rs`,
`reclaim_leaked_always_new_contexts` in `worker/src/browser_pool.rs`) is unit-tested only for
the pure parts: reclamation keeps busy contexts and drops idle ones, and `ContextBusyGuard`
adopts a pre-marked flag and clears it on drop.

**Not covered**: that dropping the `scrape_page` future mid-request actually removes the
context from a live pool. This is the exact scenario that caused the production incident
(`No available slots - max contexts limit (1) reached` with no request in flight), and it is
the one path unit tests cannot reach — `BrowserPool::new` launches a real Chrome process, so
neither the pool nor the guard can be constructed in a test.

**How to cover it**, cheapest first:

1. *Integration test with a real browser* (`#[ignore]`d, run manually / in a browser-enabled
   CI job): start a worker with `WORKER_SESSION_MODE=always_new`, `WORKER_MAX_CONTEXTS=1`,
   issue a `scrape_page` call against a slow URL, drop the gRPC client mid-request, then
   assert the next request succeeds instead of returning `CAPACITY_EXHAUSTED` (5008, which the
   coordinator surfaces to a client as `NO_WORKERS_AVAILABLE`).
2. *Refactor for testability*: extract context bookkeeping (the `Vec<Arc<BrowserContext>>`
   plus capacity/reclaim rules) from `BrowserPool` into a separate struct that owns no
   `Browser`. Then the guard can be tested against it with no Chrome at all, and
   `BrowserPool` keeps only browser/tab concerns. Larger change, better long-term.

**Production signal in the meantime**: the log lines `Purged N leaked idle context(s)` and
`Removed N leaked idle context(s)` mean reclamation fired — the slot was recovered, but a
leak still *occurred*, which means `AlwaysNewContextGuard` did not run. If those appear,
investigate what aborts the request (client vs. coordinator timeouts against the worker's
`DEFAULT_WAIT_TIMEOUT_MS`).

## A busy-stuck AlwaysNew context is invisible to leak reclamation

**Status**: known gap, no production evidence yet, deferred (raised 2026-07-20)

Both reclamation paths (`reclaim_leaked_always_new_contexts`, called from
`create_always_new_context` and from the lifecycle monitor) only collect contexts whose
`is_busy` is false. A context stuck with `is_busy = true` and no request behind it is
therefore never reclaimed and permanently consumes a slot.

Known window: `create_always_new_context` pre-marks the context busy and inserts it into the
pool, but `AlwaysNewContextGuard` (which removes it) is only constructed after
`acquire_context_with_recovery` returns to `scrape_page`. A future dropped inside that window
leaves a busy context with no owner. The window is very small, and no production occurrence
has been confirmed — the 2026-07-20 incident logs were produced by a pre-fix binary
(the deploy had built from a branch without the version bump, so Docker reused the cached
`COPY Cargo.toml Cargo.lock` layer and shipped the old worker).

**Diagnostic that would confirm it**: `at max capacity (N/N)` in an `always_new` pod with no
preceding `Purged N leaked idle context(s)` / `Removed N leaked idle context(s)` line, while
`browser_hive_worker_active_contexts` stays at N with no request in flight.

**Proposed fix if confirmed**: in the lifecycle monitor, also remove contexts that have been
busy longer than any possible request (`last_used_at.elapsed()` beyond a hard ceiling, e.g.
5 minutes). Removing a context from the pool does not abort its request — the `Arc` keeps it
alive — so a false positive only over-subscribes slots temporarily instead of locking the
pod out permanently.
