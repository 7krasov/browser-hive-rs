# TODO

Open items that need investigation or a decision. Remove an item once it is resolved.

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
