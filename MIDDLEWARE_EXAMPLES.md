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

#### Group 2: Evaluate Middleware (`TabInitMiddleware`)

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
    pub custom_user_data_dir: Option<String>,
}

impl BrowserBinaryParamsMiddleware for CustomChromeArgs {
    fn apply_args(&self, args: &mut Vec<&'static OsStr>, headless: bool) {
        // Disable WebRTC to prevent IP leaks
        if self.disable_webrtc {
            args.push(OsStr::new("--disable-webrtc"));
            args.push(OsStr::new("--disable-webrtc-encryption"));
        }

        // Use custom user data directory (for profile persistence)
        if let Some(ref data_dir) = self.custom_user_data_dir {
            // Note: This is a simplified example - in production you'd need to
            // use a dynamically allocated string with proper lifetime
            args.push(OsStr::new(&format!("--user-data-dir={}", data_dir)));
        }

        // Add headless-specific optimizations
        if headless {
            args.push(OsStr::new("--disable-gpu"));
        }
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

## Example 4: Using Middleware in Production

Here's how to configure middleware in your production worker binary:

```rust
// In your production crate (e.g., `browser-hive-prod/src/worker/main.rs`)

mod middleware;

use browser_hive_common::{
    BrowserBinaryParamsMiddleware, TabInitMiddleware,
    DefaultBinaryParamsMiddleware, DefaultTabInitMiddleware,
    ScopeConfig, WorkerConfig,
};
use middleware::{CustomChromeArgs, TimezoneOverride, WebGLSpoof};

fn create_scope_config() -> ScopeConfig {
    // 1. Create binary params middlewares
    let binary_params_middlewares: Vec<Box<dyn BrowserBinaryParamsMiddleware>> = vec![
        // Start with default middleware (anti-bot args, cache, etc.)
        Box::new(DefaultBinaryParamsMiddleware),

        // Add custom Chrome arguments
        Box::new(CustomChromeArgs {
            disable_webrtc: true,
            custom_user_data_dir: None,
        }),
    ];

    // 2. Create evaluate middlewares
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
    ];

    ScopeConfig {
        name: "production_scope".to_string(),
        proxy_provider: create_proxy_provider(), // Your custom proxy provider
        min_contexts: 5,
        max_contexts: 20,
        session_mode: SessionMode::AlwaysNew, // One-shot scraping
        headless: true,
        lifecycle: Default::default(),
        enable_browser_diagnostics: false,
        binary_params_middlewares,
        tab_init_middlewares,
        context_isolation: ContextIsolation::Isolated,
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

// Evaluate: each middleware modifies the tab sequentially (runs per tab)
let evaluate = vec![
    Box::new(DefaultTabInitMiddleware::new(headless)), // UA override (runs first)
    Box::new(TimezoneOverride { ... }),                 // Runs second
    Box::new(WebGLSpoof { ... }),                       // Runs third
];
```

### 3. Keep Evaluate Middleware Fast (Critical!)

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
            custom_user_data_dir: None,
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
INFO  Browser initialized with 3 evaluate middleware(s)
INFO    - Registered middleware: default_user_agent_override
INFO    - Registered middleware: timezone_override
INFO    - Registered middleware: webgl_spoof
DEBUG Successfully applied evaluate middleware 'default_user_agent_override'
DEBUG Successfully applied evaluate middleware 'timezone_override'
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
