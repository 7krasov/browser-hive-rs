# TODO

Open items that need investigation or a decision. Remove an item once it is resolved.

## Decide whether `AlwaysNew` should use sticky proxy sessions

**Status**: open decision (raised 2026-07-27)

Downstream `src/providers/mod.rs::create_from_env` derives `use_sticky` from the session mode, so
`AlwaysNew` runs without a session ID. Every new CONNECT tunnel therefore draws a fresh exit IP —
including the extra tunnel Chromium opens for `crossorigin="anonymous"` sub-resources, which is how
one page load ends up served by two different exit IPs. See PROXY_NETWORKING.md.

Setting `use_sticky = true` unconditionally costs nothing in IP diversity: the session ID is the
context UUID, and an `AlwaysNew` context lives for exactly one request, so each request still gets
its own IP. It only stops the scatter *within* one page load, turning a partial failure (document
loads, scripts do not, 40 s selector timeout, plausible but useless HTML) into a complete one that
existing error handling surfaces and a client can retry.

**Unverified**: whether the proxy provider's plan meters sessions separately. Check before applying.

**Still open**: why the second tunnel failed in production at all. Not reproducible locally —
ten runs without a session ID all succeeded, and five simultaneously open contexts all got working
tunnels. The `corsErrorStatus` field now logged by browser diagnostics decides it from the first
production occurrence: `InvalidResponse` (the tunnel failed) vs `MissingAllowOriginHeader` (a block
or challenge page really arrived).

## Empty CDP BrowserContexts are never disposed

**Status**: residue of the tab-leak fix; needs an upstream change (raised 2026-07-27)

Tabs are now closed at every context-removal site (`close_tab_detached` in
`worker/src/browser_pool.rs`), so the per-request tab leak in `AlwaysNew` is gone. What remains is
the empty CDP BrowserContext behind each closed tab: `Target.disposeBrowserContext` is rejected
over a page session (`Not allowed`), and headless_chrome exposes no browser-level method call
(`Transport::call_method_on_browser` exists but is unreachable from the public API).

Low priority — an empty context holds no renderer, no sockets and no proxy tunnel, which is what
the memory and connection pressure actually came from. Fixing it needs either an upstream PR
exposing browser-level calls, or a raw CDP call over the browser WebSocket alongside
headless_chrome.

**Verification level of the tab fix**: `tab.close(false)` was measured to hold the browser's tab
count flat over 5 create/use/drop cycles (`2 → 3 → 4 → 5 → 6` before, `1 → 1 → 1 → 1 → 1` after)
in a standalone reproduction. The wiring into `BrowserPool` itself is covered only by
`cargo test` + build; no end-to-end check against a running worker was made. A production pod on
an `AlwaysNew` scope should now show flat memory over its lifetime instead of growing with
request count.

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

## Redirect follow-ups (off-domain detection is DONE)

**Status**: off-domain detection shipped 2026-07-23; two optional follow-ups remain

Off-domain redirect detection is implemented: the worker returns
`ERROR_CODE_REDIRECT_TO_ANOTHER_DOMAIN` (4050) when the main navigation lands on a different
registrable domain (eTLD+1 via the `psl` crate), overriding the wait result so no selector
error surfaces. Returns the landing page's status. See RESPONSE_OBSERVERS.md.

Remaining, both optional:

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
thread. In reusable contexts (`WORKER_SESSION_MODE=reusable` / `reusable_preinit`) the tab
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
