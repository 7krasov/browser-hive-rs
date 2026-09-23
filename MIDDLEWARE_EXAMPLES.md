# Browser Middleware Examples

This document shows how to create and use custom browser middleware in your production Browser Hive deployment.

## Overview

Browser Hive supports two types of middleware for browser customization:

1. **BrowserBinaryParamsMiddleware** - Modifies Chrome launch arguments (runs BEFORE browser starts)
2. **TabInitMiddleware** - Executes CDP commands on tabs (runs AFTER each tab is created)

Both middleware types use a trait-based system similar to `ProxyProvider`, allowing you to extend browser behavior without modifying core code.

## Architecture

### Two Separate Middleware Groups

Browser Hive has **two independent middleware systems** that run at different stages:

#### Group 1: Binary Parameters Middleware (`BrowserBinaryParamsMiddleware`)

**Purpose**: Modify Chrome command-line arguments **before browser launch**

**When it runs**: Once, during `BrowserPool::new()` before calling `Browser::new()`

**What it does**: Builds the `Vec<&'static OsStr>` of Chrome args that will be passed to the browser binary

**Default implementation**: `DefaultBinaryParamsMiddleware`
- Adds Docker compatibility flags (`--no-sandbox`, `--disable-dev-shm-usage`)
- Adds anti-bot stealth flags (`--disable-blink-features=AutomationControlled`)
- Configures cache (`--disk-cache-dir`, `--disk-cache-size`)
- Sets window size (`--window-size=1920,1080`)
- Adds headless/headful-specific flags

**Example use cases**: Custom Chrome flags, feature toggles, performance tuning

#### Group 2: Tab Init Middleware (`TabInitMiddleware`)

**Purpose**: Execute CDP protocol commands **after each tab creation**

**When it runs**: Every time a new tab is created (initial contexts + recycled contexts)

**What it does**: Calls CDP methods or evaluates JavaScript on the newly created tab

**Default implementation**: `DefaultTabInitMiddleware`
- Detects User-Agent on first tab (lazy initialization)
- Replaces "HeadlessChrome" with "Chrome" via CDP `SetUserAgentOverride`
- Caches the corrected UA for subsequent tabs
- Only applies in headless mode

**Example use cases**: JavaScript injection, navigator property overrides, CDP configuration

### Execution Timeline

```
Browser Launch (happens once per worker):
├─ 1. Apply all BrowserBinaryParamsMiddleware → Build Chrome args
├─ 2. Launch browser with combined args
└─ 3. Browser process starts

Tab Creation (happens for each context):
├─ 1. Call browser.new_tab()
├─ 2. Apply all TabInitMiddleware sequentially
│     ├─ DefaultTabInitMiddleware (UA override)
│     ├─ CustomMiddleware1 (e.g., timezone)
│     └─ CustomMiddleware2 (e.g., WebGL)
└─ 3. Tab ready for use
```

## Example 1: Custom Chrome Arguments

Create custom binary params middleware to add specific Chrome flags:

```rust
// In your production crate (e.g., `browser-hive-prod/src/middleware/custom_args.rs`)

use browser_hive_common::BrowserBinaryParamsMiddleware;
use std::ffi::OsStr;

#[derive(Debug, Clone)]
pub struct CustomChromeArgs {
    pub disable_webrtc: bool,
}

impl BrowserBinaryParamsMiddleware for CustomChromeArgs {
    fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
        // Disable WebRTC to prevent IP leaks
        if self.disable_webrtc {
            args.push(OsStr::new("--disable-webrtc"));
            args.push(OsStr::new("--disable-webrtc-encryption"));
        }

        // Add headless-specific optimizations
        if headless {
            args.push(OsStr::new("--disable-gpu"));
        }

        // Note: args require 'static lifetime, so only string literals work here.
        // For dynamic values (e.g. --user-data-dir=<path>) see Best Practice #5 below.
    }

    fn name(&self) -> &str {
        "custom_chrome_args"
    }

    fn clone_box(&self) -> Box<dyn BrowserBinaryParamsMiddleware> {
        Box::new(self.clone())
    }
}
```

## Example 2: Timezone Override

Create middleware to override browser timezone:

```rust
// In your production crate (e.g., `browser-hive-prod/src/middleware/timezone.rs`)

use browser_hive_common::TabInitMiddleware;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct TimezoneOverride {
    pub timezone: String,
}

impl TabInitMiddleware for TimezoneOverride {
    fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()> {
        // Override Intl.DateTimeFormat to return specific timezone
        let script = format!(
            r#"
            Object.defineProperty(Intl.DateTimeFormat.prototype, 'resolvedOptions', {{
                value: function() {{
                    return {{
                        locale: 'en-US',
                        calendar: 'gregory',
                        numberingSystem: 'latn',
                        timeZone: '{}'
                    }};
                }}
            }});
            "#,
            self.timezone
        );

        tab.evaluate(&script, false)?;
        Ok(())
    }

    fn name(&self) -> &str {
        "timezone_override"
    }

    fn clone_box(&self) -> Box<dyn TabInitMiddleware> {
        Box::new(self.clone())
    }
}
```

## Example 3: WebGL Fingerprint Spoofing

Create middleware to modify WebGL fingerprint:

```rust
// In your production crate (e.g., `browser-hive-prod/src/middleware/webgl_spoof.rs`)

use browser_hive_common::TabInitMiddleware;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct WebGLSpoof {
    pub renderer: String,
    pub vendor: String,
}

impl TabInitMiddleware for WebGLSpoof {
    fn apply(&self, tab: &headless_chrome::browser::tab::Tab) -> Result<()> {
        let script = format!(
            r#"
            const getParameter = WebGLRenderingContext.prototype.getParameter;
            WebGLRenderingContext.prototype.getParameter = function(parameter) {{
                if (parameter === 37445) {{ // UNMASKED_VENDOR_WEBGL
                    return '{}';
                }}
                if (parameter === 37446) {{ // UNMASKED_RENDERER_WEBGL
                    return '{}';
                }}
                return getParameter.apply(this, arguments);
            }};
            "#,
            self.vendor,
            self.renderer
        );

        tab.evaluate(&script, false)?;
        Ok(())
    }

    fn name(&self) -> &str {
        "webgl_spoof"
    }

    fn clone_box(&self) -> Box<dyn TabInitMiddleware> {
        Box::new(self.clone())
    }
}
```

## Example 4: Blocking Third-Party Requests

`BlockedUrlsMiddleware` ships with the base library and needs no code of your own — only a list.
It installs the patterns on every tab via CDP `Network.setBlockedURLs`, so matching requests never
leave the browser.

```rust
use browser_hive_common::{BlockedUrlsMiddleware, TabInitMiddleware};

/// Third-party endpoints this deployment drops. Kept in code rather than in an env var so the
/// list is reviewable, and so a change to it goes through the same review as any other change.
const BLOCKED_URL_PATTERNS: &[&str] = &[
    // Analytics / telemetry
    "*://*.analytics.example.com/*",
    "*://metrics.example.net/*",
    // Session recording
    "*://*.recorder.example.org/*",
];

let tab_init_middlewares: Vec<Box<dyn TabInitMiddleware>> = vec![
    Box::new(DefaultTabInitMiddleware::new(headless)),
    Box::new(BlockedUrlsMiddleware::new(
        BLOCKED_URL_PATTERNS.iter().map(|p| p.to_string()).collect(),
    )),
];
```

### Pattern syntax

Patterns are sent to CDP **unchanged**. Measured (2026-09-22, Chrome, macOS, headless): the
pattern is split at every `*`, and a URL is blocked when it contains those pieces **in order,
anywhere** — no anchoring to the start or end of the URL, `*` is the only special character, `?`
is a literal and there is no escape syntax. The matcher knows nothing about hosts or paths; the
patterns below work as host rules only because `://` and `/` frame the host:

| Intent | Pattern |
|---|---|
| one host, no subdomains | `*://example.com/*` |
| subdomains only | `*://*.example.com/*` |
| both | the two patterns above |
| one path on a host | `*://example.com/tracker/*` |
| one file, any subdomain | `*://*.example.com/beacon.js*` |

Things to know before writing a list:

- A bare host (`example.com`) matches **too much**, not nothing: it behaves like `*example.com*`
  and blocks every URL containing the string — `notexample.com`, and any URL carrying it in a
  query, such as `https://other.test/?ref=example.com`. `BlockedUrlsMiddleware::new` logs a WARN at
  startup for every pattern without a `*`.
- Even `*://example.com/*` also matches a URL that carries that address in its query string (a
  redirect parameter), and it does not match the host with an explicit port.
- There is no way to say "the URL **ends** with X": `*.m3u8` also blocks `x.m3u8.js` and
  `a.m3u8x/b.js`. Narrow it with what follows instead — `*.m3u8?*` blocks only a manifest with a
  query string.
- `?` is a plain character, not a single-character wildcard. The `Fetch` domain's `urlPattern`
  does treat `?` as a wildcard — it is a different matcher, do not assume the two agree.

| Pattern | Blocked (measured) |
|---|---|
| `*.m3u8` | `playlist.m3u8`, `playlist.m3u8?token=1`, `playlist.m3u8x/a.js`, `x.m3u8.js` |
| `*.m3u8?*` | only `playlist.m3u8?token=1` |
| `*.m3u8\?*` | nothing |
| `pla?list` | nothing (`plaXlist.js` is not matched) |
| `example.com` | `notexample.com.js` too |
| `*://blocked.test/*` | `/r?next=http://blocked.test/x` too |

### What must never be blocked

⚠️ Anti-bot, CAPTCHA and consent-manager scripts. A blocked analytics endpoint costs the page
nothing; a blocked challenge script turns a page that would have loaded into a hard block, and a
blocked consent manager can leave the content gated forever. These frequently live on the same
host as blockable content, which is why path-level patterns exist.

Do not block CDNs that serve the page's own assets either — a page missing its bundle fails as a
plain `SELECTOR_NOT_FOUND` that blames the site.

### Confirming a list is in force

A mistyped pattern blocks nothing and looks exactly like a page with no trackers. Blocked loads
are counted per request and recorded on the worker's `scrape_page` span as `blocked_requests`
(omitted when zero), so in Loki:

```
{app="worker-<scope>"} | json | span_blocked_requests != ""
```

They are deliberately kept **out** of browser diagnostics: dozens of self-inflicted blocks per
page would fill `WORKER_DIAGNOSTICS_MAX_ENTRIES` and push out the failures that explain a bad page.


## Example 4b: Blocking by Resource Type

`BlockedResourceTypesMiddleware` drops every load of the given CDP resource types — `media` is the
intended use (`<video>`/`<audio>`), `font` a candidate. It works through Fetch interception rather
than `setBlockedURLs`, which cannot see the type. Configured from `WORKER_BLOCKED_RESOURCE_TYPES`:

```rust
use browser_hive_common::{BlockedResourceTypesMiddleware, TabInitMiddleware};

let tab_init_middlewares: Vec<Box<dyn TabInitMiddleware>> = vec![
    Box::new(DefaultTabInitMiddleware::new(headless)),
    // WORKER_BLOCKED_RESOURCE_TYPES=media  (unset or empty: no-op)
    // Installs only the request interceptor; the worker enables Fetch. Never call
    // `Fetch.enable` / `tab.enable_fetch` from a middleware: it replaces the worker's call and,
    // behind a proxy with credentials, fails every request with ERR_INVALID_AUTH_CREDENTIALS.
    Box::new(BlockedResourceTypesMiddleware::from_env()),
];
```

⚠️ **Fetch belongs to the worker.** `Fetch.enable` is called in one place, the worker's request
path, which combines proxy authentication and the blocked types' patterns in one call. A second
caller breaks proxy authentication whichever runs last: each call replaces the previous one, and
Chrome answers the proxy's 407 only for requests matching the current patterns. Measured with an
authenticating proxy: every navigation failed with `ERR_INVALID_AUTH_CREDENTIALS` (failed, not sent
around the proxy). The same goes for `enable_request_interception`: a tab has one interceptor.

- Names are CDP resource types, case-insensitive, comma-separated.
- Chrome's Fetch filter rejects `texttrack`, `prefetch`, `websocket`, `manifest`, `signedexchange`,
  `preflight` and `fedcm` — and one rejected type fails the whole `Fetch.enable`. These (and unknown
  names) are dropped with a startup WARN. `prefetch` could not work anyway: prefetches arrive as `fetch`.
- Types that can break pages or trip anti-bot checks (`image`, `script`, `stylesheet`, `document`,
  `xhr`, `fetch`, `eventsource`, `ping`, `other`) are **allowed** — the choice is the deployment's —
  with a startup WARN.
- Not caught: HLS/DASH video (segments are `xhr`/`fetch`) and anything inside a cross-site iframe.
- A tab has **one** request interceptor; a second middleware installing its own replaces this one.
- Blocked loads show up exactly like URL-list blocks (`span_blocked_requests`, the third-party
  blocked metric) and additionally on `browser_hive_worker_requests_blocked_by_type_total{page_site,
  resource_type}`. See METRICS.md.
## Example 5: Using Middleware in Production

Here's how to configure middleware in your production worker binary:

```rust
// In your production crate (e.g., `browser-hive-prod/src/worker/main.rs`)

mod middleware;

use browser_hive_common::{
    BlockedUrlsMiddleware, BrowserBinaryParamsMiddleware, TabInitMiddleware,
    DefaultBinaryParamsMiddleware, DefaultTabInitMiddleware,
    ContextIsolation, ScopeConfig, SessionMode,
};
use middleware::{CustomChromeArgs, TimezoneOverride, WebGLSpoof};

fn create_scope_config() -> ScopeConfig {
    let headless = true;

    // 1. Create binary params middlewares
    let binary_params_middlewares: Vec<Box<dyn BrowserBinaryParamsMiddleware>> = vec![
        // Start with default middleware (anti-bot args, cache, etc.)
        Box::new(DefaultBinaryParamsMiddleware),

        // Add custom Chrome arguments
        Box::new(CustomChromeArgs {
            disable_webrtc: true,
        }),
    ];

    // 2. Create tab init middlewares
    let tab_init_middlewares: Vec<Box<dyn TabInitMiddleware>> = vec![
        // Add default UA override (HeadlessChrome → Chrome)
        Box::new(DefaultTabInitMiddleware::new(headless)),

        // Add timezone override
        Box::new(TimezoneOverride {
            timezone: "America/New_York".to_string(),
        }),

        // Add WebGL spoofing
        Box::new(WebGLSpoof {
            renderer: "Intel Iris OpenGL Engine".to_string(),
            vendor: "Intel Inc.".to_string(),
        }),

        // Drop third-party analytics/ad requests (see Example 4 for the list and its rules)
        Box::new(BlockedUrlsMiddleware::new(
            BLOCKED_URL_PATTERNS.iter().map(|p| p.to_string()).collect(),
        )),
    ];

    ScopeConfig {
        name: "production_scope".to_string(),
        proxy_provider: create_proxy_provider(), // Your custom proxy provider
        min_contexts: 0, // Pre-created contexts are only useful in SessionMode::Reusable
        max_contexts: 20,
        session_mode: SessionMode::AlwaysNew, // One-shot scraping
        headless,
        lifecycle: Default::default(),
        browser_path: None, // Auto-detect Chrome/Chromium; Some("/usr/bin/brave-browser".into()) for Brave
        diagnostics: DiagnosticsConfig::from_env(), // Or ::default() to disable
        binary_params_middlewares,
        tab_init_middlewares,
        context_isolation: ContextIsolation::Isolated,
        destroy_session_on_block: false, // Only meaningful for SessionMode::Dedicated (set true there)
        block_quarantine: std::time::Duration::ZERO, // Only acted on in SessionMode::Reusable
    }
}
```

## Best Practices

### 1. Understand the Two Middleware Types

**BinaryParamsMiddleware** and **TabInitMiddleware** are **completely different**:

| Aspect | BinaryParamsMiddleware | TabInitMiddleware |
|--------|------------------------|-------------------|
| **Runs when** | Once per browser launch | Every tab creation |
| **Purpose** | Build Chrome args | Execute CDP/JS on tabs |
| **Input** | `&mut Vec<&'static OsStr>` | `&Tab` |
| **Example** | `--disable-webrtc` | `tab.evaluate(...)` |
| **Performance** | Not critical (runs once) | Critical (runs often) |

### 2. Middleware Order Matters (Within Each Group)

Middlewares are applied in the order they appear in their respective vector:

```rust
// Binary params: each middleware adds args sequentially (runs once)
let binary_params = vec![
    Box::new(DefaultBinaryParamsMiddleware), // Adds base args
    Box::new(CustomChromeArgs { ... }),             // Adds custom args
];

// Tab init: each middleware modifies the tab sequentially (runs per tab)
let tab_init = vec![
    Box::new(DefaultTabInitMiddleware::new(headless)), // UA override (runs first)
    Box::new(TimezoneOverride { ... }),                 // Runs second
    Box::new(WebGLSpoof { ... }),                       // Runs third
];
```

### 3. Keep Tab Init Middleware Fast (Critical!)

`TabInitMiddleware::apply()` is called for **EVERY new tab** (including recycled contexts). Keep operations lightweight:

- ✅ Use simple JavaScript overrides
- ✅ Cache expensive computations in middleware struct (like `DefaultTabInitMiddleware` does with UA)
- ❌ Avoid heavy computations or network calls
- ❌ Don't create additional browser tabs
- ❌ Don't do synchronous operations that block

**BinaryParamsMiddleware** performance is not critical - it only runs once during browser startup.

### 4. Handle Errors Gracefully

**TabInitMiddleware** errors are logged but don't fail tab creation:

```rust
impl TabInitMiddleware for MyMiddleware {
    fn apply(&self, tab: &Tab) -> Result<()> {
        // If this fails, error is logged and next middleware runs
        tab.evaluate(SCRIPT, false)?;
        Ok(())
    }
}
```

**BinaryParamsMiddleware** errors will fail browser startup (which is desired behavior).

### 5. Use Static String Literals for Chrome Args

**BinaryParamsMiddleware only**: Chrome args must have `'static` lifetime. Use string literals:

```rust
fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
    // ✅ Good: static string literal
    args.push(OsStr::new("--disable-webrtc"));

    // ❌ Bad: dynamic string (won't compile)
    // let arg = format!("--user-data-dir={}", self.path);
    // args.push(OsStr::new(&arg));
}
```

For dynamic args, you'll need to use a different approach (e.g., environment variables, `lazy_static!`, or `Box::leak`).

### 5a. Feature Switches: Only the Last One Counts

Chromium reads `--disable-features=` and `--enable-features=` as **one value each**. When the
command line carries the same switch twice, the last one replaces the first; the two lists are
**not** combined. Measured 2026-09-23 (Chrome 153 and Brave, headless):

```text
--disable-features=BackForwardCache --disable-features=TranslateUI
    → only TranslateUI is disabled; the back/forward cache stays ON
--disable-features=TranslateUI --disable-features=BackForwardCache
    → only BackForwardCache is disabled; TranslateUI is back ON
--disable-features=TranslateUI,BackForwardCache
    → both disabled (one switch, comma-separated list)
```

The command line is built from three sources, in this order: headless_chrome's own `DEFAULT_ARGS`,
then every `BrowserBinaryParamsMiddleware` in order, then the pool's options. Which switch wins is
therefore a matter of position, not of intent.

- **`--disable-features` is merged for you.** `BrowserPool::new` (`worker/src/launch_args.rs`)
  collects every `--disable-features=` value from all three sources: headless_chrome's
  (`TranslateUI,BlinkGenPropertyTrees`), the middlewares' and `ScopeConfig::disable_back_forward_cache`.
  It passes them as a single switch and drops headless_chrome's default through
  `ignore_default_args`. A middleware may push its own `--disable-features=…` safely. The startup
  line `Chrome feature switch: …` shows the result.
- **`--enable-features` is NOT merged, and already overrides a default.** headless_chrome passes
  `--enable-features=NetworkService,NetworkServiceInProcess`, and the default middlewares
  (`DefaultBinaryParamsMiddleware`, `BraveBinaryParamsMiddleware`) push
  `--enable-features=TabDiscarding` after it. So only `TabDiscarding` is enabled, and the network
  service runs as a separate process (the `network` type in the process metrics). This has been
  the case since `TabDiscarding` was added. Changing it is an open decision in TODO.md, since
  merging would move the network service into the browser process. A middleware that pushes
  another `--enable-features=` cancels `TabDiscarding` the same way. Put every feature to enable in
  **one** switch.
- The same rule applies to any switch that takes a value (`--js-flags=`, `--proxy-server=`, …):
  pass it once.

### 6. Test Middleware in Isolation

Create unit tests for your middleware:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_args() {
        let middleware = CustomChromeArgs {
            disable_webrtc: true,
        };

        let mut args = Vec::new();
        middleware.apply_args(&mut args, true);

        assert!(args.contains(&OsStr::new("--disable-webrtc")));
    }
}
```

## Debugging

Enable debug logging to see middleware execution:

```bash
RUST_LOG=debug cargo run --bin worker
```

You'll see logs like:

```
INFO  Applying 2 binary params middleware(s)
INFO    - Applying middleware: default_binary_params
INFO    - Applying middleware: custom_chrome_args
INFO  Browser initialized with 3 tab init middleware(s)
INFO    - Registered middleware: default_user_agent_override
INFO    - Registered middleware: timezone_override
INFO    - Registered middleware: webgl_spoof
DEBUG Successfully applied tab init middleware 'default_user_agent_override'
DEBUG Successfully applied tab init middleware 'timezone_override'
```

## Migration from Old Code

If you're migrating from the old hardcoded approach:

**Before** (hardcoded in `BrowserPool`):
```rust
// Chrome args hardcoded in browser_pool.rs
let chrome_args = vec![
    OsStr::new("--no-sandbox"),
    OsStr::new("--my-custom-arg"),
    // ...
];
```

**After** (middleware in production code):
```rust
// In your production crate
struct MyBinaryParams;
impl BrowserBinaryParamsMiddleware for MyBinaryParams {
    fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
        args.push(OsStr::new("--my-custom-arg"));
    }
    // ...
}

// In config
let binary_params_middlewares = vec![
    Box::new(DefaultBinaryParamsMiddleware), // Base args
    Box::new(MyBinaryParams),                       // Custom args
];
```

This pattern keeps the core Browser Hive codebase clean while allowing full customization in your production deployment.
