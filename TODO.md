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

**Over a full night** (2026-09-16, same scope): every pod shows the same shape — ~12 MiB at start
rising monotonically to 25–40 MiB, outliers at 47 and 55 MiB, i.e. ~7 MiB/h against ~20 MiB/h
before the fork. No plateau anywhere, because no pod survives long enough to reach one. At 55 MiB
against a 2 GiB limit this is 2.7 % and has nothing to do with the OOMKills, but it is still
unbounded, and it cannot be read properly until the OOMKills stop.

**A second upstream PR is worth tracking: rust-headless-chrome#567** (open, one line). When a
browser-event listener disconnects, the transport treats it as fatal, closes the websocket and
cancels every outstanding call — which this pool can only read as a dead browser, i.e. a spurious
`recreate_browser_pool` and the loss of every session on that pod. #568, which the pin carries,
fixed the neighbouring path (calls that already timed out), not this one. Not a memory fix. Before
adding it, check production logs for "connection is closed" to see whether the path is actually
being hit.

**What else came with 1.0.18 → 1.0.22**, for tracing a regression: a regenerated CDP protocol (new
optional fields in `Network.enable` and `Target.createTarget`, set to `None` here, so the requests on
the wire are unchanged); `wait_for_initial_tab` waits 20 s instead of 10 s (not called here);
`tungstenite` 0.29, which our own pin followed.

**Costs of the pin**: the fork can disappear — clean builds would then fail loudly; mirror the same
rev under our own account if that happens. A crate with a git dependency cannot be published to
crates.io.

**To close**: once a release contains #568, switch back to the version from crates.io, keep
`tungstenite` on the version it resolves (see the comment in `Cargo.toml`), and remove this item.

## Synchronous CDP calls on the request path: bounded, deployed, not yet verified

**Status**: implemented in v0.35.0 (2026-09-19); deployed downstream (worker pins v0.36.1 as of
2026-09-21). Nothing below has been read yet; remove this item once it has

Every headless_chrome call waits up to `idle_browser_timeout` (1 hour) for an answer. On
2026-09-18 a downstream `reusable` worker froze for exactly one hour: a wait-strategy hard timeout
closed the tab inline, the tab never answered, and the close held the runtime's only worker
thread. v0.34.0 fixed that instance (detached removal, recycling outside the lock, thread count).

This change bounds the rest (see CLAUDE.md, "Bounded CDP calls"): context creation reserves a
slot and builds outside the `contexts` write lock; every per-tab CDP call in
`scrape_page_internal` (tab creation, `Fetch.enable`, `Network.enable`, diagnostics `enable`s,
headless-marker and status-fallback `evaluate`s) and the lifecycle monitor's rebuild wait at most
30 s; a late result is disposed; a stall removes the context and answers 5003 (5005 for context
creation). (`authenticate` and `add_event_listener` make no CDP call — they only set local state.)

Side effect worth knowing: a tab created lazily or after a closed connection in `shared` isolation
now gets the tab init middlewares (UA override, blocked URLs); it used to be a bare
`browser.new_tab()`.

**Not covered**: `Browser::new` (startup / pool recreation only).

**To verify** (the hot path cannot be exercised by unit tests):
- startup line `Tokio runtime: 4 worker threads`;
- after a hard timeout or stall, `Removing context … after a hard timeout` / `… after a stalled
  CDP call` followed by uninterrupted log lines;
- `CDP call … took` WARNs: their durations are the data for tuning the 30 s `CALL_TIMEOUT`;
- `CDP call … did not answer within 30s` should be rare; if frequent, the timeout is too tight
  for a CPU-throttled browser.

## The same scope OOMKilled 10 minutes after a clean browser restart

**Status**: open, cause unknown. The renderer half of "Worker OOMKills" below — still reproduced
after disposal was deployed (2026-09-21: OOMKills ~20 min after a pod starts, single renderer up to
~770 MiB)

Same incident, 2026-09-18. After the hour-long freeze the pool was recreated and the old Chrome was
killed (`Killed browser process 8 of the dropped browser pool` — the v0.33.0 fix works). The new
browser then served ~70 ordinary successful requests (one site, ~7 s each, 2 contexts) and the
container was OOMKilled ~10 minutes later. Before the freeze the renderer PSS had also climbed to
~1.5–1.7 GiB (largest single renderer ~945 MiB) on the same traffic. Hypothesis, not verified:
each context reuses one tab, same-site navigations stay in one renderer process, and that
renderer's heap grows with every page of this site. Things to check: `renderer` max-PSS against
`total_requests` of the context; whether recycling after N requests (or a fresh tab per request
inside the same context) flattens it.

12-hour view of the largest single process of one pod (Kyiv time) supports the growth half of
the hypothesis: the largest renderer climbs **linearly** after every restart (~0 → ~930 MiB in
~25 min at 19:00, again to ~900 MiB 20:30–21:40), and in the earlier, lighter hours it is a
sawtooth whose drops match context recycling (the scope recycles at `WORKER_MAX_REQUESTS=100` /
`WORKER_MAX_LIFETIME=1h`). So one renderer keeps what every page it loaded left behind until
its tab is closed. *What* it keeps (V8 heap, DOM, back/forward cache, DevTools network buffers)
is not known. Both hour-long metric gaps (17:45–18:50 and 19:25–20:25) start at a renderer
peak (~930–960 MiB): plausibly a renderer under memory pressure stops answering, the request
hits its hard timeout and the old inline `tab.close` froze the worker — correlation only.
Mitigations, cheapest first: lower `WORKER_MAX_REQUESTS` for the scope (config only; costs
cookies and exit IP more often); rotate the **tab** inside the same CDP context every N requests
(keeps cookies and exit IP; whether a new tab gets a fresh renderer needs checking); find what
grows (`Runtime.getHeapUsage` / `Memory.getDOMCounters` per request).

## Worker OOMKills: renderers set the floor, undisposed contexts set the slope

**Status**: cause identified 2026-09-16; context disposal deployed and removes the slope (2026-09-21, see "Context disposal must be verified in production"); the renderer floor still OOMKills pods at 2 GiB

A busy downstream `reusable` scope (2 GiB limit, `max_contexts = 2`, Brave headless, isolated
contexts, 7-12 concurrent pods) is OOMKilled continuously. With the browser resource metrics of
v0.32.0 deployed, a 12-hour production night separates it into two independent phenomena. Figures
below are scope-wide sums divided by the count of `gpu`/`network`/`storage` processes, of which
Chromium runs exactly one per browser.

**The floor: renderers.**

| process type | per pod | PSS per pod |
|---|---:|---:|
| renderer | ~15 (peak ~35) | ~736 MiB (peak ~1.57 GiB) |
| network (NetworkService) | 1 | 173 MiB (peak 389 MiB) |
| browser | 5 | 123 MiB |
| gpu | 1 | 122 MiB |
| zygote, utility, storage | ~4 | ~38 MiB |

≈1.19 GiB the moment a pod is warm, of which renderers are 62 %. `browser_hive_worker_iframes_total`
counted ~69 K out-of-process iframes in 12 h on that scope, essentially all of them ad exchanges,
cookie-sync and RTB bidders — the hypothesis from the previous round, now measured. `container_memory_working_set_bytes` runs 5-10 % below our
PSS+RSS figure (PSS also counts shared file-backed pages); both agree the pods sit at the ceiling.

**The slope: NetworkService.** "Largest single process by type" over the same night shows
`network` as repeated **monotone ramps with a vertical drop** — 04:00 ~390 MiB → 06:30 **852 MiB**
(~185 MiB/h), then a drop when the pod dies, then another ramp. On the same panel the renderer
maximum is noisy with no trend, `browser` and `gpu` are flat. Renderers are large but bounded by
the request; only this one grows.

**Why it grows**: a non-default CDP BrowserContext is incognito-like, so its HTTP cache lives **in
memory inside NetworkService**, next to its cookie store and socket pools. Up to v0.32.0 nothing disposed it (see
"Context disposal must be verified in production"), and `reusable` recycling abandoned a context on
every rotation. `browser_hive_worker_browser_contexts` confirms the accumulation directly: healthy
pods sit at 2 (= `max_contexts`, nothing abandoned), long-lived ones climb a staircase to 16, of
which 14 are not in the pool — and the steepest climb, 05:00-06:30, is the same window as the
852 MiB ramp, with both dropping at the same instant. Roughly 50 MiB per abandoned context. (The
counts are always even because both slots of a pod expire together and the lifecycle monitor
recycles them in one tick.)

**The arithmetic closes.** Every pod carries exactly one `OOMKilled` and one restart, spread evenly
over the night, ~3 kills/hour across the scope → a pod lives ~3.3 h.

| | |
|---|---:|
| floor once warm | 1.2-1.4 GiB |
| headroom to the limit | 600-800 MiB |
| NetworkService slope | ~185 MiB/h |
| **predicted pod lifetime** | **3.2-4.3 h** |
| **measured pod lifetime** | **~3.3 h** |

This is a correlation of two independently collected signals, not a proof; the last link — that
Chromium actually returns the memory on `Target.disposeBrowserContext` — can only be established by
deploying the fix.

**Order of work**, highest value first (state as of 2026-09-21; the slope is gone with context
disposal, what remains is the floor):

1. **Block list of ad-tech script hosts** (downstream, `BlockedUrlsMiddleware`). Lowers the floor.
   Downstream now ships a list with the major ad exchanges in it, but its effect on renderers
   **has not been measured**: on 2026-09-21 the scope still showed ~13 renderers per pod against
   ~15 before, and whether the list was in force on those pods was not checked
   (`span_blocked_requests`). Next step: read the test below, then extend the list with the top
   hosts of `iframes_total`.
   Note the twist: the list provably cannot block an iframe *document*, but these iframes are
   injected by JS from tag and ad-server hosts, and those scripts are ordinary sub-resources of the
   main page, which the list does block. Test: `iframes_total` and `browser_processes{type="renderer"}`
   must fall together; if only `third_party_requests_blocked_total` moves, the reasoning is wrong.
   Never block the consent manager, `www.google.com` (reCAPTCHA lives there) or a generic JS CDN.
2. **Explain the renderers without targets** (open sub-question below).
3. A larger memory limit, as an anaesthetic while 1 is built — **applied** to that one scope
   downstream (2 → 2.5 GiB, 2026-09-21, request unchanged). **First reading, 2026-09-22** (12 h,
   ~10 h after the rollout): no OOMKill or restart on that scope (before: ~3/h). Headroom is thin
   — pods peak at ~2.2-2.4 GiB, the largest single renderer reached 720 MiB — so one heavy page on
   a warm pod can still tip it. No evictions either (`kube_pod_status_reason{reason="Evicted"}` all
   0), although the request is still 1.5 GiB and nodes are overcommitted by ~1 GiB per pod at peak
   — re-check if the scope grows.
4. Turning site isolation off in headless — only if 1 is not enough, and with a block-rate
   comparison, since it is a memory-vs-stealth trade.

**Open sub-question: ~3 renderer processes per live target.** Unchanged with the block list in
force (2026-09-22, 12 h: ~13.5 renderers per browser vs. ~3.8 live targets per browser —
`page` 29, `iframe` 3, `worker` 6 across ~10 browsers), so the list did not lower the renderer
count. At the same instants, the scope had
~154 renderer processes against ~55 targets that need one (`page` 23, `iframe` 24, `worker` 8) —
~15 renderers per pod against ~5.5 targets. Cause unknown; do not guess it. The cheap test is one
long-lived pod: if `browser_processes{type="renderer"}` rises while the target gauges stay flat,
it is a second leak, independent of NetworkService.

## Third-party request and iframe metrics

**Status**: implemented 2026-09-15, deployed (read in production from 2026-09-16). What the metrics count, the label rules and the
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
- The list cannot reduce renderer processes **directly**. It may still do so indirectly: production
  shows the iframes are injected by JS from tag and ad-server hosts, and those scripts are ordinary
  sub-resources of the main page, which the list does block. Untested — see the test in "Worker
  OOMKills: renderers set the floor, undisposed contexts set the slope".
- An iframe-host metric fed from page-session events misses every frame nested inside a
  cross-site frame.

Candidates, all unverified:
- Turn site isolation off in headless (headful already does).
- `Fetch` interception of `Document` requests, which must coexist with the `Fetch` handler
  `tab.authenticate` installs.
- `Target.setAutoAttach` to reach the child frames' sessions.

Not yet checked on Linux, over HTTPS, or through a proxy.

## Shutdown drain and unreachable-worker retry: implemented, not verified

**Status**: implemented 2026-09-22, not released or deployed yet. Remove this item once the checks
below have been read

On SIGTERM the worker now drains in-flight requests (up to `WORKER_SHUTDOWN_DRAIN_TIMEOUT_SECS`)
instead of cancelling them, and the coordinator retries a connect failure or a broken RPC once on
another pod (GRACEFUL_SHUTDOWN.md, `worker/src/shutdown.rs`). Production nodes are GKE Spot, where a
preemption grants 15 s whatever the pod spec says — which is why the coordinator retry is part of
the same change. Downstream was handed the manifest side (drop the worker `preStop` sleep, set the
drain timeout to 330, bump both the worker tag and the coordinator's `BASE_VERSION`).

**To verify** (a busy scope, over a window with a rollout or KEDA scale-down):
- startup line `Shutdown drain timeout: 330s` on every pod;
- a stopped pod logs `Draining: …` then `All in-flight requests completed` and
  `Worker shutdown complete`; `Drain timeout (…) reached` should be rare;
- `requests_retried_total{reason="terminating"}` falls towards zero outside spot preemptions;
- `requests_retried_total{reason="worker_unreachable"}` appears at preemptions and OOM kills, and
  `requests_rejected_total{reason="worker_unreachable"}` drops against it.

Known limits: a new request that reaches a draining pod is still answered TERMINATING and retried
(cheap, nothing was started); the coordinator's health monitor polls sequentially with no
connect/health-check timeout, so one hung pod can delay the others' health rounds (low priority).

## Context disposal must be verified in production

**Status**: implemented 2026-09-16, deployed; slope confirmed gone 2026-09-21, OOMKills not (raised 2026-07-27 as "empty contexts are never
disposed")

Every context-removal site now closes the tab **and** disposes the CDP BrowserContext
(`release_context_detached` in `worker/src/browser_pool.rs`; the design and its constraints are in
CLAUDE.md, "Removing a context from the pool does not free it in Chrome"). It is the fix for the
OOMKill slope in "Worker OOMKills: renderers set the floor, undisposed contexts set the slope".

One deviation from the plan written before the code: disposal runs on a **dedicated**
`BrowserCdpClient` socket (`BrowserPool::context_disposer`), not on the per-context-proxy
`cdp_client`. Calls on one socket are serialised, so a slow disposal there would have held up context
creation on the request path of per-context-proxy scopes; `cdp_client` itself is unchanged and still
exists only for those scopes.

**Verified locally** (Chrome and Brave on macOS, headless, via `BrowserCdpClient` directly, not
through a running worker): the browser accepts `disposeBrowserContext` on the browser-level socket
in 9-36 ms, both after the tab was closed and with the tab still open, and
`Target.getBrowserContexts` drops by one each time. Closing a tab whose context was already disposed
fails fast (`No session with given id`); disposing an unknown context is rejected (`Failed to find
context`) and ends up as one WARN.

**Not verified — what the deploy must show.** Read it on the scope that was being OOMKilled, no
earlier than 4-6 h after the rollout (pods used to live ~3.3 h, so a shorter window proves nothing),
best over a night. Dashboard "Browser Hive - Browser Resources":

1. **The new binary is running.** The workspace version must be bumped for this, otherwise the
   startup banner is identical to v0.32.0's (the fix ships as v0.33.0). Loki: `{app="worker-<scope>"} |= "Browser Hive library
   version="` — every pod started after the rollout must print the new version. (Selector check
   first: without the line filter the stream must return data.)
2. **"Browser contexts: in the browser vs. not in the pool"** — the "not in the pool" series stays
   near 0 and the browser total never climbs above `max_contexts` (brief +1/+2 during a recycle tick
   is expected). Before: a staircase up to 16.
3. **"Largest single process by type"**, series `network` — no monotone ramp; flat or saw-toothed
   around a level. Before: 390 → 852 MiB in 2.5 h.
4. **"Container restarts and OOMKills per pod"** — OOMKills drop from ~3/h across the scope towards
   zero. If they only become rarer, the renderer floor (item 1 of "Order of work") is next.
5. **"Memory per pod vs. container limit"** — pods stop marching up to the limit.
6. **Loki, WARNs of the change**: `{app="worker-<scope>"} |= "Could not dispose CDP context"` and
   `{app="worker-<scope>"} |= "context disposal client"` — expected empty; occasional lines around a
   browser restart are fine, a steady stream means disposal is failing and 2-3 above will not move.

If 2 holds but 3 does not, the slope has another cause and the OOM item reopens.

**First production reading, 2026-09-21** (3 h, a busy downstream `reusable` scope, 2 GiB limit,
`max_contexts = 2`; every pod in the window already ran disposal — v0.34+ lines in its logs — and a
rollout finished inside it). Only 3-5 were read; 1, 2 and 6 were not checked.

- **3 holds.** `network` is flat: largest single NetworkService ~110-160 MiB, scope-wide ~100-120 MiB
  per pod, no ramp on any pod. The slope is gone.
- **4 and 5 do not.** ~10 OOMKills in 3 h across the scope, on pods of the new rollout too, the first
  ~20 min after start. Pods sit at the limit on renderers alone: ~13 renderer processes and
  ~1.1-1.6 GiB renderer PSS per pod, the largest single renderer up to ~770 MiB. So the renderer
  floor (item 1 of "Order of work" in the OOM item) is what kills pods now, not a leak with age.
- Precursor seen in Loki: `get_content hard timeout` ~1 ms after the wait selector was found, twice
  in a row on fresh contexts (1-3 requests), 10 s before the pod went down — consistent with the
  kernel killing a renderer mid-request, not verified.
- Unexplained: that OOMKilled pod logged `Received SIGTERM` and shut down gracefully, instead of
  dying silently as a whole-cgroup OOM kill would make it. The sender is unknown (events expired).
- Downstream raised the limit of that scope to 2.5 GiB (item 3 of "Order of work").

## A replaced browser pool leaves its Chrome running

**Status**: confirmed in production 2026-09-16 as the OOMKill cause of a downstream `always_new`
scope; fixed the same day (`Drop for BrowserPool`, v0.33.0) and deployed. The verification below has
not been read on that scope yet. Why the transport closes is still open

`BrowserPool::start_lifecycle_monitor` spawns an endless task that holds clones of the pool's
`Arc<Browser>`, its contexts and its CDP clients, and nothing stops it: `recreate_browser_pool`
(`worker/src/service.rs`) only swaps the pool. headless_chrome kills the Chrome process only when the
last `Arc<Browser>` is dropped (`BrowserInner` owns the `Process`, whose `Drop` sends the kill and
removes the temporary profile directory). So after every pool replacement the old task keeps ticking
and **the old Chrome keeps running**, renderers and NetworkService included, for the pod's lifetime.

The replacement is triggered by `connection is closed` — headless_chrome's `ConnectionClosed`, i.e.
its *transport* stopped — and that does not mean the process died.

**Production, 2026-09-16 (v0.32.0), the `always_new` scope, 3-4 pods:**
- Loki: 17 replacements in 24 h, all in this scope, none in any other. Every one is the same line:
  `Browser process appears dead during context creation (always_new mode) ... Failed to create
  isolated CDP context: Unable to make method calls because underlying connection is closed`.
- "Browser main processes per pod" climbs in steps of exactly 5 — one browser's worth of the
  `browser` process type (see the OOM item): one pod went 20 → 25 → 30 → 35 → 40 → 45, i.e. 4 → 9
  Chrome instances, another 10 → 15. A pod that had just logged a replacement showed 10 (two Chromes)
  within minutes.
- "Browser processes by type": `gpu`, `network` and `storage` — one per browser — reach 11 across
  3-4 pods.
- "Memory per pod vs. container limit" rises in steps at the same moments, and the two pods with the
  most Chromes are exactly the two OOMKilled (~12:45 and ~13:15). After the restart each pod sits at
  300-500 MiB.

**Why the transport closes is unknown**, and our logs cannot tell: headless_chrome logs through the
`log` crate, and `init_logging` (`common/src/logging.rs`) installs the subscriber with
`set_global_default`, which — unlike `.init()` — does not install the `log` bridge. The crate's own
lines ("Transport loop got disconnected …", "Got a timeout while listening for browser events …")
are therefore dropped. Candidate, unproven: the transport-shutdown path upstream
rust-headless-chrome#567 fixes.

**Fixed** by point 1 of the original plan: `Drop for BrowserPool` aborts the lifecycle monitor and
SIGKILLs the old browser by PID (see CLAUDE.md, "Browser Pool Recovery"). Verified locally on Chrome
and Brave with a real `BrowserPool` replaced under an `RwLock` while an extra `Arc<Browser>` clone
was still held: the old main process became a zombie at once with no children left, and was reaped
as soon as that clone was dropped.

**To verify** (dashboard "Browser Hive - Browser Resources", the `always_new` scope,
12-24 h):
- "Browser main processes per pod" stays at 5 (one browser) — at most a brief 10 right after a
  replacement. Before: steps of +5 up to 45.
- "Browser processes by type": `gpu`/`network`/`storage` equal the pod count.
- "Memory per pod vs. container limit": no steps; "Container restarts and OOMKills per pod": none.
- Loki: `{app="worker-<scope>"} |= "Killed browser process"` — one line per replacement, i.e. as
  many as `|= "Recreating browser pool due to dead browser process"`.
  `|= "Could not kill browser process"` must stay empty.

**Still open: why the transport closes** (~17 times a day in that scope). Our logs cannot say, see
the next item. Candidate, unproven: the path upstream rust-headless-chrome#567 fixes. Decide on #567
only once the cause is known.

## headless_chrome's own log lines never reach Loki

**Status**: open, someday (raised 2026-09-16) — deliberately postponed, the volume worries us

headless_chrome logs through the `log` crate, and `init_logging` (`common/src/logging.rs`) installs
the subscriber with `set_global_default`, which — unlike `SubscriberInitExt::init()` — does not
install the `log` → `tracing` bridge. Every line the crate writes is dropped, including the ones that
would explain a closed transport ("Transport loop got disconnected …", "Got a timeout while
listening for browser events …") and its own "Killing Chrome".

To try: install `tracing_log::LogTracer` and cap the `headless_chrome` target at `warn` through the
`EnvFilter` (its `info` level is chatty). Before shipping, measure the line volume on one busy pod —
the concern is the Loki bill, not correctness. Covers every downstream binary at once, since they all
call `init_logging`.

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
