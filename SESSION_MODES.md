# Session Modes

`WORKER_SESSION_MODE` decides three things at once: whether a browser context outlives the
request that created it, who may be handed that context next, and what the scope's capacity
number actually counts. Everything else in this document follows from those three.

| Mode | Context outlives the request? | Who gets it next | Capacity of a scope | Returns `session_id` |
|---|---|---|---|---|
| `always_new` | no — destroyed when the request ends | nobody | `max_contexts` concurrent **requests** | no |
| `reusable` (default) | yes, until recycled by the lifecycle monitor | any later request | `max_contexts` concurrent **requests** | no |
| `dedicated` | yes, until idle, expired or blocked | only the session that owns it | `max_contexts` concurrent **sessions** | yes |

The design decisions behind this split, and the alternatives that were rejected, are in
[Design decisions](#design-decisions) at the end. Implemented 2026-08-03; there was no production
deployment at the time, so the previous `reusable_preinit` mode was removed outright rather than
deprecated.

---

## `dedicated`: a context that belongs to one session

A request that arrives **without** a session id always gets a **fresh** context
(`BrowserPool::create_dedicated_context`). Contexts already in the pool are never offered to it —
each of them belongs to a client. A session reaches its own context by sending back the
`session_id` it was given, which the worker resolves through `find_context_by_id`.

That single rule is what `session_id` was always supposed to mean, and it removes a whole class of
failure: two unrelated clients can no longer hold the same session, and `Context <id> is already
busy` can now only mean what it says — the client itself sent two concurrent requests on one
session.

**Capacity is the headline.** A `dedicated` scope with `max_contexts = 10` serves **10 concurrent
sessions**, not 10 requests per unit time. A client that fetches one page and disappears holds a
slot until the idle timeout. Size such scopes from the number of concurrent scraping processes,
never from request rate. When the slots run out, the response says so explicitly (`No available
session slots - all N contexts are claimed by sessions`).

### How a slot comes back

There is no release RPC (see [Design decisions](#design-decisions)), so three things end a session:

| Trigger | Default | Notes |
|---|---|---|
| Idle timeout | 1 min (`WORKER_MAX_IDLE_TIME_SECS`) | The only mechanism that needs no cooperation from anyone. Checked by the lifecycle monitor, which ticks every 60 s — so removal happens up to a minute after expiry. |
| `max_lifetime` / `max_requests` | 6 h / 10 000 | The ordinary lifecycle thresholds. |
| HTTP 403 / 429 | **on** in `dedicated` (`WORKER_DESTROY_SESSION_ON_BLOCK`) | See below. |

Idle time is measured from the **end** of the last request, not its start: `ContextBusyGuard`
re-stamps `last_used_at` on drop. Without that, a request taking longer than the idle timeout
would count as idle for its entire duration the moment it finished, and the monitor would remove
the session of a client that is still working.

**Expiry removes the context, it does not recycle it.** Recycling would put a fresh context that
nobody owns into the same slot, and the pool would report itself full with no session in sight —
the same reason `always_new` removes leaked contexts instead of replacing them. Removal is also
what makes the [`claimed_contexts`](#metrics) gauge fall back to a truthful value.

The one-minute default is deliberate and short: every abandoned session costs capacity for exactly
that long, while a client that is genuinely working a site returns far sooner.

⚠️ **The coordinator's session TTL is not aligned with this and stays unresolved.**
`SessionManager` defaults to 30 minutes (`common/src/session.rs`) while a `dedicated` context is
removed after a minute idle, so a client returning after a two-minute pause presents a session id
the coordinator still accepts and the worker no longer has → `SESSION_NOT_FOUND`. That answer is
correct (the session really is gone) and clients must handle it by retrying without a session id,
but it makes `SESSION_NOT_FOUND` a routine outcome rather than an exception. Aligning the two is
not trivial: the coordinator has one `SessionManager` for all scopes and does not know any scope's
session mode or idle timeout. Left open deliberately — decide it with real numbers on how often
clients actually pause, not up front.

### Destroying the context on 403/429

`WORKER_DESTROY_SESSION_ON_BLOCK` releases a session's context as soon as its page comes back
403 or 429 — the same statuses the wait strategy already exits early on
(`EARLY_EXIT_STATUS_CODES`, `common/src/wait_strategy.rs`), so the signal costs nothing extra.

**It defaults to on in `dedicated`** (and to off in the other modes, where it does nothing —
see the default table above). The default follows the mode for the same reason `max_idle_time`
does: in `dedicated` a blocked context is a slot nobody is coming back for, and nothing but the
idle timeout would ever free it. An explicit value always wins, so a scope that scrapes a site
where 403 is an ordinary answer (a paywalled or permission-gated page rather than a block) can set
`WORKER_DESTROY_SESSION_ON_BLOCK=false` and keep the session's cookies and warm-up.

This is what replaces an explicit release. A client drops its session on those statuses anyway, so
nothing still in use is taken away; without it the worker would keep the slot — and the exit IP the
origin has just refused — claimed until the idle timeout.

The response of that request carries **no `context_id`**, so the coordinator caches no session for
it. A later request that still presents the old `session_id` gets `SESSION_NOT_FOUND`, which is
true.

It stays an **option** because a 403 on one page does not universally mean the session is dead —
but that is the rarer case, and it costs one extra warm-up, while the opposite mistake costs a
slot out of `max_contexts` for up to two minutes on a scope whose capacity *is* its slot count.
The setting has no effect in the other modes, and `validate()` says so at startup rather than
ignoring it — which is also why the default is mode-dependent rather than a flat `true`, since a
flat `true` would make every `reusable` scope warn on every start.

**`max_lifetime` is left at its default and measured, not guessed.** For a gateway provider the
useful session length is bounded by the provider's sticky TTL, which providers generally do not
publish. Rather than guessing, let it happen: the TTL expires, the exit IP changes under the
session, the origin answers the mismatched cookies with a 403, the client drops the session and the
context is destroyed. The system self-corrects at a cost of one wasted page load. The line logged
when a context is released after a block carries its **age** for exactly this reason — if those
ages cluster, the cluster is the provider's sticky TTL and `max_lifetime` can be set just below it.

For a dedicated IP pool there is no sticky TTL, so no ceiling is needed at all.

---

## `reusable`: an anonymous pool

- **Returns no `session_id`.** Nothing is lost: it never guaranteed anything. A context here
  belongs to nobody in particular, and an id addressing it promised an ownership the pool does not
  provide. A `context_id` arriving in a request to a `reusable` scope is ignored.
- **Context selection is even.** `find_least_busy_context` now picks the idle context with the
  fewest `total_requests`, not the first idle one in the vector. The old behaviour concentrated
  traffic on the earliest context — and therefore, with per-context proxy routing, on the first
  exit IP, while the rest of a purchased pool idled.
- **Domain affinity stays off.** `find_best_context_for_domain` has been deleted, along with
  `BrowserContextMetadata::primary_domains`, which nothing read. Reasoning below.
- Contexts are **recycled** (replaced by a fresh context in the same slot) by the lifecycle
  monitor, so recycling never frees a slot — it only resets state.

---

## `always_new`

Unchanged: a context is created per request, pre-marked busy under the pool write lock, and
removed when the request scope ends (`AlwaysNewContextGuard`, plus an early destroy as a latency
optimisation). Any idle context found in the pool is a leak and is removed, never recycled. See
CLAUDE.md, "Context Lifecycle Monitoring".

---

## Configuration

| Variable | Default | Applies to |
|---|---|---|
| `WORKER_SESSION_MODE` | `reusable` | `always_new`, `reusable`, `dedicated` |
| `WORKER_MAX_CONTEXTS` | 3 | all — but counts **sessions** in `dedicated` |
| `WORKER_MIN_CONTEXTS` | **0** | `reusable` only (pre-creates contexts at startup) |
| `WORKER_MAX_IDLE_TIME_SECS` | 300, **60 in `dedicated`** | `reusable`, `dedicated` |
| `WORKER_DESTROY_SESSION_ON_BLOCK` | `true` in `dedicated`, `false` elsewhere | `dedicated` only |
| `WORKER_CONTEXT_ISOLATION` | `isolated` | all — `dedicated` requires `isolated` |

Pre-initialization is an option, not a mode. `min_contexts > 0` fills the pool at startup, which
only pays off in `reusable`, where any request can use a pre-created context. Its default is 0 so
that no scope pre-fills by accident.

### Validation at startup

`ScopeConfig::validate()` runs in `run_worker` before the browser is launched — the same shape as
the "a worker never starts without a proxy" guard. A scope that cannot do what it is configured to
do must not serve traffic while its logs claim otherwise.

**Hard error — the combination silently does the wrong thing:**

| Rule | Why |
|---|---|
| `dedicated` + `context_isolation = shared` | in `shared` every request runs in the browser's default context; exclusivity cannot exist |
| `dedicated` + `rotation_strategy != Hybrid` | `max_idle_time` would be ignored, and claimed slots would never be released |
| `min_contexts > max_contexts` | nonsense in any mode |
| `max_contexts == 0` | the scope can serve nothing |
| `max_lifetime < max_idle_time` | the context dies of age before it can ever be idle long enough — checked only in `reusable`/`dedicated`, since `always_new` ignores both |

**Warning — the setting is valid but inert in this mode.** Returned to the caller and logged,
never dropped:

| Setting | Mode | Message |
|---|---|---|
| `min_contexts > 0` | `always_new`, `dedicated` | pre-created contexts are destroyed unused |
| all lifecycle thresholds | `always_new` | contexts do not outlive their request |
| `rotation_strategy != Hybrid` | `reusable` | `max_idle_time` and `max_cache_size_mb` are ignored |
| `destroy_session_on_block` | `always_new`, `reusable` | no session to destroy |

**Neither — the parameter works and only its value is uncertain:** `max_lifetime` in `dedicated`.
There is nothing to validate; see the measurement approach above.

---

## Lifecycle thresholds

Driven by `ContextLifecycleConfig` (`common/src/config.rs`), evaluated in
`BrowserPool::should_recycle_context`:

| Field | Measured from | Default |
|---|---|---|
| `max_idle_time` | the **end** of the last request on that context | 5 min (1 min in `dedicated`) |
| `max_lifetime` | context **creation** | 6 h |
| `max_requests` | request counter | 10 000 |
| `max_cache_size_mb` | estimated cache size | 500 MB |

⚠️ **`rotation_strategy` gates which of these are consulted at all.** Only `Hybrid` (the value both
worker binaries hardcode) honours all four. `TimeBasedOnly` honours `max_lifetime` alone and
`RequestBasedOnly` honours `max_requests` alone — under either, `max_idle_time` is silently
ignored. This is a hard error in `dedicated` and a startup warning in `reusable`; the general
problem is the corresponding item in TODO.md.

What expiry *does* differs by mode: `reusable` **replaces** the context (the slot count is
unchanged), `dedicated` and `always_new` **remove** it.

---

## Metrics

`browser_hive_worker_claimed_contexts{scope}` — slots that cannot be given to a new client —
exists **alongside** `active_contexts`, which counts contexts that are busy right now.

The two are identical in `always_new` and `reusable`; in `dedicated`, claimed is a superset,
because a slot held by a session that is idle between two requests is not busy. An autoscaler
reading `active/total_slots` would see an empty worker whose every slot is spoken for, so
**`claimed_contexts` is the gauge to autoscale on in every mode**. `available_slots` (reported to
the coordinator and used for routing) is derived from claimed, not from busy. See METRICS.md.

---

## Interaction with the two kinds of proxy provider

Session modes behave differently depending on how the provider assigns an exit IP. The two kinds
are distinguished by `ProxyProvider::assigns_proxy_host_per_context()` (see PROXY_NETWORKING.md):

| | Gateway provider (session ID in credentials) | Dedicated IP pool (host per context) |
|---|---|---|
| What fixes the exit IP | the session ID, derived from the context id | the pool entry assigned to the context |
| Effect of time | the provider's sticky TTL expires and **the IP changes on its own** | none, the address is fixed |
| Destroying a context | new context id → new session ID → **fresh IP immediately** | next pool entry → a different IP only probabilistically |
| What a 403 means | this session is burnt, the next one is clean | this **address** is burnt for a while |
| Supply | effectively unbounded | bounded by the size of the purchased pool |

Two consequences worth stating explicitly:

1. **An expiring sticky TTL surfaces as a 403, never as `PROXY_ERROR`.** `is_proxy_error`
   (`worker/src/service.rs`) matches only Chromium tunnel failures
   (`ERR_TUNNEL_CONNECTION_FAILED`, `ERR_PROXY_CONNECTION_FAILED`, …). When a sticky TTL expires
   the tunnel still succeeds — it just leaves through a different exit IP, and the origin answers
   the now-mismatched cookies with a block. Clients must treat 403/429 as the session-invalidation
   signal; there is no separate error code for this and there should not be one (the fact is
   already in `status_code`).
2. **A context is not an IP.** With a dedicated IP pool, if `max_contexts` exceeds the number of
   purchased addresses, two contexts share one address: their cookie jars stay isolated, but the
   per-address request rate is a multiple of what the capacity numbers suggest. Size such scopes
   with `max_contexts ≤ pool size`. The worker cannot check this — it does not know the pool size.

---

## Design decisions

### What a shared context actually exposes to anti-bot systems

A recurring argument for letting one context scrape many sites is that a browser carrying cookies
from many origins "looks more like a real user". **It does not, and the reasoning matters for
choosing a mode.** An anti-bot system on site A cannot see cookies belonging to site B —
same-origin policy keeps them out of every request A receives. What is actually shared across
sites within one context:

| Signal | Scoped to the context? | Does mixing sites change it? |
|---|---|---|
| Exit IP | yes (sticky session = context id) | **yes — the only real cross-site channel** |
| TLS / HTTP2 fingerprint | no, shared by the whole browser process | no |
| Canvas / WebGL / fonts / screen | no, shared by the whole browser process | no |
| Cookie jar of the visited domain | yes | no — other domains' cookies are invisible |
| HTTP cache, HSTS state | yes | a theoretical timing side-channel, not used in practice |

So the choice between a shared pool and an exclusive context is **not** an anti-fingerprinting
question. It is a question of who holds the exit IP and at what rate a single address hits a
single origin — and there the shared pool is, if anything, slightly better: the same number of
requests spread over several origins raises no single origin's per-IP rate.

The counter-intuitive consequence for `dedicated`: by pinning one session to one context it
**concentrates** all of a site's traffic on one exit IP, which is exactly the pattern rate
limiters detect best. `dedicated` is chosen for determinism (a session that nobody else can
disturb), not for evasion — and it pays for that determinism in per-IP concentration.

### Why domain affinity is not switched on

`find_best_context_for_domain` preferred a context that had seen the domain **and was idle**; a
busy one was never selected. Trace the realistic load of ~50 concurrent requests against a single
site through a 6-context pool:

- request 1 takes context 1 and records the domain;
- request 2 arrives while 1 is busy, finds no *idle* warm context, falls back to any idle one;
- …and so on until all 6 are busy, after which requests are refused;
- within seconds **all six contexts know the domain**, and affinity becomes a no-op.

So it neither pins a site to one context (the fear) nor warms anything (the hope) — under
single-domain load every context is warm anyway. It pays off only in the opposite regime: many
distinct domains, low concurrency, a warm context among the free ones.

It does have a cost in the regime that matters here. After a 403 the client drops its session and
retries; affinity would preferentially route that retry back to the **burnt** context, because
that is the warm one for the domain. The function and the `primary_domains` set it read were
deleted rather than left as dead code.

### Rejected alternatives

| Rejected | Why |
|---|---|
| `ReleaseSession` RPC | burden on every client, and every client eventually fails to call it (crash, timeout, deploy) — so the idle timeout has to exist anyway, at which point the RPC only optimises the well-behaved case |
| Exclusivity as a per-request flag instead of a mode | without an explicit release, claimed slots would strand inside a pool that also serves ordinary traffic, forcing quotas and priorities; a homogeneous scope has no such problem |
| Keeping `reusable_preinit` as a deprecated alias | it was one startup branch wearing a mode's clothes, and with a fourth mode it would have forced a combinatorial `dedicated_preinit`; nothing was in production, so it was deleted |
| A burnt-address quarantine hook on `ProxyProvider` | it would only help a partially burnt dedicated IP pool, where the real answer is buying usable addresses; not worth a trait change in `common` |
| A distinct `ErrorCode` for 403/429 | duplicates a fact `status_code` already carries (same reasoning as the HTTP early exit, CLAUDE.md) |
| Replacing `active_contexts` with `claimed_contexts` | dashboards and KEDA triggers already reference the existing gauge; the new one is added beside it |

---

## Downstream migration

The production repo builds its own `ScopeConfig`, so it must be updated in the same change:

- `src/bin/worker.rs`: `ScopeConfig` gains `destroy_session_on_block`; `SessionMode::ReusablePreinit`
  no longer exists; `min_contexts` no longer defaults to `max_contexts`.
- Manifests in `ops/`, `ops_local/` and `docker-compose*.yml` that set `WORKER_SESSION_MODE`
  (`reusable_preinit` is now an unknown value and **fails startup**) or rely on the old
  `WORKER_MIN_CONTEXTS` default.
- Any dashboard or KEDA trigger that should follow claimed rather than busy slots.
