# TODO

Open items that need investigation or a decision. Remove an item once it is resolved.

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

Both worker binaries — the base `crates/worker/src/main.rs` and the downstream
`src/bin/worker.rs` — set `ContextLifecycleConfig::rotation_strategy` to `RotationStrategy::Hybrid`
as a literal. It is the only lifecycle field not read from the environment, and no manifest sets
it. No prior note explains whether that is a decision or simply never finished, which is why this
item exists.

`should_recycle_context` (`worker/src/browser_pool.rs`) makes the strategy decide *which thresholds
are consulted at all*:

| Strategy | Honoured | Silently ignored |
|---|---|---|
| `TimeBasedOnly` | `max_lifetime` | `max_requests`, `max_idle_time`, `max_cache_size_mb` |
| `RequestBasedOnly` | `max_requests` | `max_lifetime`, `max_idle_time`, `max_cache_size_mb` |
| `Hybrid` | all four, OR-ed | — |

**Argument for leaving it hardcoded**: `Hybrid` is the only value under which the other four knobs
mean anything. Exposing it as `WORKER_ROTATION_STRATEGY` creates a knob that can silently disable
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
   assert the next request succeeds instead of returning `CONTEXT_CREATION_FAILED`.
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
