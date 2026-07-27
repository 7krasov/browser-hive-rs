//! Per-request capture of browser-side diagnostic signals.
//!
//! The returned HTML shows *what* the page ended up as, never *why*. A page whose JavaScript
//! bundle was blocked by the proxy and a page whose JavaScript threw look identical in the
//! content — both return an unrendered template. This module captures the difference.
//!
//! # Signals and where they come from
//!
//! | Signal | CDP source | Extra cost |
//! |---|---|---|
//! | Uncaught JS errors, HTTP 4xx/5xx on sub-resources | `Log.entryAdded` | `Log.enable` |
//! | Failed/blocked/aborted loads (with URL) | `Network.loadingFailed` + `Network.requestWillBeSent` | none — the `Network` domain is already enabled for the response observer |
//! | `console.*` output, uncaught exceptions with stack | `Runtime.consoleAPICalled`, `Runtime.exceptionThrown` | `Runtime.enable` — **opt-in**, see [`DiagnosticsConfig::capture_console`] |
//! | Final page state (readyState, node counts, title) | one `Runtime.evaluate` | one round-trip, bounded by [`PAGE_STATE_BUDGET`] |
//!
//! Events are only ever appended to a bounded in-memory buffer — nothing is logged per event.
//! One request produces at most a handful of log lines, written once at the end.
//!
//! # Why capture is decided before navigation
//!
//! Listeners must exist before the page starts loading, which is exactly when the interesting
//! failures happen. The request outcome is not known yet, so capture is gated on configuration
//! (enabled + mode + domain) while *logging* is gated on the outcome. See
//! [`browser_hive_common::diagnostics`].

use browser_hive_common::{DiagnosticsConfig, DiagnosticsMode};
use headless_chrome::Tab;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::service::EventListenerGuard;

/// Time budget for the final page-state snapshot.
///
/// Deliberately its own constant rather than "whatever is left of the wait timeout": the wait
/// strategy is allowed to consume the entire request budget, so a leftover-based budget is
/// always zero exactly when the request failed on a timeout — the case diagnostics exist for.
pub const PAGE_STATE_BUDGET: Duration = Duration::from_secs(3);

/// Maximum characters kept per captured entry. Stack traces and data: URLs are unbounded in
/// principle; a log line must not be.
const MAX_ENTRY_CHARS: usize = 500;

/// Maximum in-flight request ids tracked for URL resolution of `Network.loadingFailed`.
/// A page can issue thousands of requests; the map only exists to name the failures.
const MAX_TRACKED_REQUESTS: usize = 512;

/// Captured signals for one request.
#[derive(Default)]
struct DiagnosticsBuffer {
    /// Uncaught JavaScript errors (`Log` source `javascript`, `Runtime.exceptionThrown`).
    js_errors: Vec<String>,
    /// Resource loads that failed, were blocked, or returned an HTTP error status.
    failed_requests: Vec<String>,
    /// `console.error` / `console.warn` output.
    console_errors: Vec<String>,
    /// Remaining `console.*` output.
    console_messages: Vec<String>,
    /// Entries dropped because a category hit `max_entries`.
    dropped: usize,
    /// In-flight request id → URL, used to name `Network.loadingFailed` events.
    request_urls: std::collections::HashMap<String, String>,
    /// Final page state snapshot (JSON), taken once at the end of the request.
    page_state: Option<String>,
}

/// Which bucket a captured entry belongs to.
#[derive(Clone, Copy)]
enum Category {
    JsError,
    FailedRequest,
    ConsoleError,
    ConsoleMessage,
}

impl DiagnosticsBuffer {
    /// Append `text` to `category`, honouring the cap and collapsing duplicates.
    ///
    /// Duplicates are common and uninformative: a page in an error loop emits the same message
    /// hundreds of times. Keeping one copy with a count preserves the signal without the volume.
    fn push(&mut self, category: Category, text: String, max_entries: usize) {
        let dropped = &mut self.dropped;
        let category = match category {
            Category::JsError => &mut self.js_errors,
            Category::FailedRequest => &mut self.failed_requests,
            Category::ConsoleError => &mut self.console_errors,
            Category::ConsoleMessage => &mut self.console_messages,
        };
        let text = truncate(&text, MAX_ENTRY_CHARS);

        for existing in category.iter_mut() {
            if entry_body(existing) == text {
                let count = entry_count(existing) + 1;
                *existing = format!("{} (x{})", text, count);
                return;
            }
        }

        if category.len() >= max_entries {
            *dropped += 1;
            return;
        }
        category.push(text);
    }

    fn total_entries(&self) -> usize {
        self.js_errors.len()
            + self.failed_requests.len()
            + self.console_errors.len()
            + self.console_messages.len()
    }
}

/// Strip a trailing `" (xN)"` repetition marker, to compare entries by their body.
fn entry_body(entry: &str) -> &str {
    match entry.rfind(" (x") {
        Some(idx) if entry.ends_with(')') => &entry[..idx],
        _ => entry,
    }
}

/// Read back the repetition count encoded by [`entry_body`]'s marker (1 when absent).
fn entry_count(entry: &str) -> usize {
    entry
        .rfind(" (x")
        .filter(|_| entry.ends_with(')'))
        .and_then(|idx| entry[idx + 3..entry.len() - 1].parse().ok())
        .unwrap_or(1)
}

/// Truncate on a character boundary, marking that it happened.
fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{}…[truncated]", cut)
}

/// Per-worker rate limiter for diagnostics emission.
///
/// Guards against the one failure mode entry caps cannot: a domain breaking entirely and every
/// request emitting a well-formed, capped, and utterly redundant dump.
pub struct DiagnosticsLimiter {
    max_per_minute: u32,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    window_start: Instant,
    used: u32,
    /// Emissions suppressed since the last allowed one, reported with the next allowed line.
    suppressed: u32,
}

impl DiagnosticsLimiter {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            state: Mutex::new(LimiterState {
                window_start: Instant::now(),
                used: 0,
                suppressed: 0,
            }),
        }
    }

    /// Take a slot. Returns the number of emissions suppressed since the previous allowed one,
    /// or `None` when this emission itself must be suppressed.
    fn try_acquire(&self) -> Option<u32> {
        if self.max_per_minute == 0 {
            return Some(0); // 0 = unlimited
        }

        let mut state = self.state.lock().unwrap();
        if state.window_start.elapsed() >= Duration::from_secs(60) {
            state.window_start = Instant::now();
            state.used = 0;
        }

        if state.used >= self.max_per_minute {
            state.suppressed += 1;
            return None;
        }

        state.used += 1;
        Some(std::mem::take(&mut state.suppressed))
    }
}

/// A live diagnostics capture for one request.
///
/// Emission happens in `Drop` rather than at an explicit call site so that the hard-timeout,
/// cancellation and panic paths — which return early from the handler and are the most
/// interesting failures — are covered without threading a call through every `return`.
/// The default outcome is therefore "failed"; the success path calls [`Self::mark_success`].
pub struct DiagnosticsSession {
    buffer: Arc<Mutex<DiagnosticsBuffer>>,
    limiter: Arc<DiagnosticsLimiter>,
    mode: DiagnosticsMode,
    succeeded: bool,
    /// Removes the CDP listener when the session ends.
    _listener_guard: Option<EventListenerGuard>,
}

impl DiagnosticsSession {
    /// Mark the request as successful, suppressing emission in [`DiagnosticsMode::OnError`].
    ///
    /// A found `skip_selector` counts as success: it is an outcome the client asked for, not a
    /// malfunction, and treating it as an error would dump diagnostics on every skipped page.
    pub fn mark_success(&mut self) {
        self.succeeded = true;
    }

    /// Store the final page-state snapshot.
    pub fn set_page_state(&self, json: String) {
        self.buffer.lock().unwrap().page_state = Some(json);
    }

    fn should_emit(&self) -> bool {
        match self.mode {
            DiagnosticsMode::Off => false,
            DiagnosticsMode::Always => true,
            DiagnosticsMode::OnError => !self.succeeded,
        }
    }
}

impl Drop for DiagnosticsSession {
    fn drop(&mut self) {
        if !self.should_emit() {
            return;
        }

        let Some(suppressed) = self.limiter.try_acquire() else {
            return;
        };

        let buffer = self.buffer.lock().unwrap();
        let failed = !self.succeeded;

        // Summary always goes out once the decision is made to emit: "no signals at all" is
        // itself a result — it rules out JS errors and blocked resources as the explanation.
        let counts = if buffer.total_entries() == 0 {
            "no browser-side signals captured".to_string()
        } else {
            format!(
                "{} JS error(s), {} failed request(s), {} console error(s), {} console message(s)",
                buffer.js_errors.len(),
                buffer.failed_requests.len(),
                buffer.console_errors.len(),
                buffer.console_messages.len(),
            )
        };
        let summary = format!(
            "Browser diagnostics: {}{}{}",
            counts,
            if buffer.dropped > 0 {
                format!(", {} entr(ies) dropped over cap", buffer.dropped)
            } else {
                String::new()
            },
            if suppressed > 0 {
                format!(
                    ", {} earlier report(s) suppressed by rate limit",
                    suppressed
                )
            } else {
                String::new()
            },
        );

        if failed {
            warn!("{}", summary);
        } else {
            info!("{}", summary);
        }

        if !buffer.js_errors.is_empty() {
            warn!("Diagnostics/JS errors: {:?}", buffer.js_errors);
        }
        if !buffer.failed_requests.is_empty() {
            warn!("Diagnostics/failed requests: {:?}", buffer.failed_requests);
        }
        if !buffer.console_errors.is_empty() {
            warn!("Diagnostics/console errors: {:?}", buffer.console_errors);
        }
        if !buffer.console_messages.is_empty() {
            info!("Diagnostics/console: {:?}", buffer.console_messages);
        }
        if let Some(state) = &buffer.page_state {
            info!("Diagnostics/page state: {}", state);
        }
    }
}

/// Start capturing diagnostics for a request, if configuration says to.
///
/// Must be called before navigation. Returns `None` when diagnostics are inactive for this
/// request, in which case no CDP domain is touched and no listener is registered.
///
/// Failures to enable a domain or register the listener are non-fatal — diagnostics never break
/// a scrape.
pub fn start_capture(
    tab: &Arc<Tab>,
    config: &DiagnosticsConfig,
    limiter: &Arc<DiagnosticsLimiter>,
    url: &str,
) -> Option<DiagnosticsSession> {
    use headless_chrome::protocol::cdp::types::Event;
    use headless_chrome::protocol::cdp::{Log, Runtime};

    if !config.is_active_for(url) {
        return None;
    }

    // Log domain: uncaught JS errors and network-level console entries, with URLs and line
    // numbers, without the Runtime domain's fingerprinting surface.
    if let Err(e) = tab.call_method(Log::Enable(None)) {
        debug!("Failed to enable Log domain for diagnostics: {}", e);
    }

    // Runtime domain: console.* and exception objects. Opt-in — enabling it is detectable by
    // anti-bot systems, so it must never be a side effect of turning diagnostics on.
    if config.capture_console {
        if let Err(e) = tab.call_method(Runtime::Enable(None)) {
            debug!("Failed to enable Runtime domain for diagnostics: {}", e);
        }
    }

    let buffer: Arc<Mutex<DiagnosticsBuffer>> = Arc::new(Mutex::new(DiagnosticsBuffer::default()));
    let max_entries = config.max_entries;
    let capture_console = config.capture_console;
    let listener_buffer = buffer.clone();

    let listener: Arc<dyn headless_chrome::browser::tab::EventListener<Event> + Send + Sync> =
        Arc::new(move |event: &Event| {
            let mut buf = match listener_buffer.lock() {
                Ok(buf) => buf,
                Err(_) => return, // Poisoned by a panicking emitter - diagnostics are not worth a cascade
            };

            match event {
                Event::LogEntryAdded(ev) => {
                    let entry = &ev.params.entry;
                    if !matches!(
                        entry.level,
                        Log::LogEntryLevel::Error | Log::LogEntryLevel::Warning
                    ) {
                        return;
                    }
                    let location = match (&entry.url, entry.line_number) {
                        (Some(url), Some(line)) => format!(" @ {}:{}", url, line),
                        (Some(url), None) => format!(" @ {}", url),
                        _ => String::new(),
                    };
                    let text = format!("[{:?}] {}{}", entry.source, entry.text, location);
                    let category = match entry.source {
                        Log::LogEntrySource::Network => Category::FailedRequest,
                        _ => Category::JsError,
                    };
                    buf.push(category, text, max_entries);
                }

                // Track request ids so a failure can be named. Network is already enabled by the
                // response observer, so these events arrive whether or not we look at them.
                Event::NetworkRequestWillBeSent(ev) => {
                    if buf.request_urls.len() < MAX_TRACKED_REQUESTS {
                        buf.request_urls
                            .insert(ev.params.request_id.clone(), ev.params.request.url.clone());
                    }
                }

                Event::NetworkLoadingFailed(ev) => {
                    let url = buf
                        .request_urls
                        .get(&ev.params.request_id)
                        .cloned()
                        .unwrap_or_else(|| "(unknown url)".to_string());
                    let blocked = match &ev.params.blocked_reason {
                        Some(reason) => format!(" blocked={:?}", reason),
                        None => String::new(),
                    };
                    let canceled = if ev.params.canceled.unwrap_or(false) {
                        " canceled"
                    } else {
                        ""
                    };
                    let text = format!(
                        "[{:?}] {}{}{} — {}",
                        ev.params.Type, ev.params.error_text, blocked, canceled, url
                    );
                    buf.push(Category::FailedRequest, text, max_entries);
                }

                Event::RuntimeExceptionThrown(ev) if capture_console => {
                    let details = &ev.params.exception_details;
                    let description = details
                        .exception
                        .as_ref()
                        .and_then(|e| e.description.clone())
                        .unwrap_or_else(|| details.text.clone());
                    let location = match &details.url {
                        Some(url) => format!(" @ {}:{}", url, details.line_number),
                        None => String::new(),
                    };
                    buf.push(
                        Category::JsError,
                        format!("[uncaught] {}{}", description, location),
                        max_entries,
                    );
                }

                Event::RuntimeConsoleAPICalled(ev) if capture_console => {
                    let rendered = ev
                        .params
                        .args
                        .iter()
                        .map(render_remote_object)
                        .collect::<Vec<_>>()
                        .join(" ");
                    let text = format!("[{:?}] {}", ev.params.Type, rendered);
                    let is_error = matches!(
                        ev.params.Type,
                        Runtime::ConsoleAPICalledEventTypeOption::Error
                            | Runtime::ConsoleAPICalledEventTypeOption::Assert
                            | Runtime::ConsoleAPICalledEventTypeOption::Warning
                    );
                    let category = if is_error {
                        Category::ConsoleError
                    } else {
                        Category::ConsoleMessage
                    };
                    buf.push(category, text, max_entries);
                }

                _ => {}
            }
        });

    let listener_guard = match tab.add_event_listener(listener) {
        Ok(weak) => {
            let tab_for_remove = tab.clone();
            Some(EventListenerGuard::new(move || {
                let _ = tab_for_remove.remove_event_listener(&weak);
            }))
        }
        Err(e) => {
            debug!("Failed to add diagnostics listener: {}", e);
            None
        }
    };

    Some(DiagnosticsSession {
        buffer,
        limiter: limiter.clone(),
        mode: config.mode,
        succeeded: false,
        _listener_guard: listener_guard,
    })
}

/// Render a CDP `RemoteObject` the way a console would: by value when it has one, otherwise by
/// the description Chrome already computed, otherwise by type.
fn render_remote_object(obj: &headless_chrome::protocol::cdp::Runtime::RemoteObject) -> String {
    if let Some(value) = &obj.value {
        return match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    obj.description
        .clone()
        .unwrap_or_else(|| format!("{:?}", obj.Type))
}

/// Snapshot the final page state, with a hard time bound.
///
/// Runs after the wait strategy, so the tab may be dead or unresponsive; every failure mode is
/// swallowed. Answers "did the document finish, and did anything render at all", which
/// separates a broken page from a page that simply lacked the selector.
pub async fn capture_page_state(tab: Arc<Tab>) -> Option<String> {
    let span = tracing::Span::current();
    let evaluate = tokio::task::spawn_blocking(move || {
        let _span_guard = span.enter();
        tab.evaluate(
            r#"JSON.stringify({
                documentReady: document.readyState,
                title: document.title || "(no title)",
                url: document.URL,
                scriptsCount: document.getElementsByTagName('script').length,
                bodyChildrenCount: document.body ? document.body.children.length : 0,
                htmlLength: document.documentElement.outerHTML.length
            })"#,
            false,
        )
    });

    match tokio::time::timeout(PAGE_STATE_BUDGET, evaluate).await {
        Ok(Ok(Ok(result))) => result
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .map(str::to_string),
        Ok(Ok(Err(e))) => {
            debug!("Page state snapshot failed: {}", e);
            None
        }
        Ok(Err(e)) => {
            debug!("Page state snapshot task failed: {}", e);
            None
        }
        Err(_) => {
            debug!(
                "Page state snapshot timed out after {:?} - skipping",
                PAGE_STATE_BUDGET
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_caps_entries_and_counts_drops() {
        let mut buf = DiagnosticsBuffer::default();
        for i in 0..5 {
            buf.push(Category::JsError, format!("error {}", i), 3);
        }
        assert_eq!(buf.js_errors.len(), 3);
        assert_eq!(buf.dropped, 2);
    }

    #[test]
    fn test_push_collapses_duplicates_without_consuming_cap() {
        let mut buf = DiagnosticsBuffer::default();
        for _ in 0..4 {
            buf.push(Category::JsError, "same error".to_string(), 3);
        }
        assert_eq!(buf.js_errors, vec!["same error (x4)".to_string()]);
        assert_eq!(buf.dropped, 0);
    }

    #[test]
    fn test_categories_are_independent() {
        let mut buf = DiagnosticsBuffer::default();
        buf.push(Category::JsError, "boom".to_string(), 20);
        buf.push(Category::FailedRequest, "net::ERR_FAILED".to_string(), 20);
        buf.push(Category::ConsoleError, "console boom".to_string(), 20);
        buf.push(Category::ConsoleMessage, "hello".to_string(), 20);
        assert_eq!(buf.total_entries(), 4);
    }

    #[test]
    fn test_truncate_is_char_safe() {
        let text = "ключова помилка".repeat(100);
        let truncated = truncate(&text, MAX_ENTRY_CHARS);
        assert!(truncated.ends_with("…[truncated]"));
        assert_eq!(
            truncated.chars().count(),
            MAX_ENTRY_CHARS + "…[truncated]".chars().count()
        );
    }

    #[test]
    fn test_limiter_suppresses_over_budget_and_reports_count() {
        let limiter = DiagnosticsLimiter::new(2);
        assert_eq!(limiter.try_acquire(), Some(0));
        assert_eq!(limiter.try_acquire(), Some(0));
        assert_eq!(limiter.try_acquire(), None);
        assert_eq!(limiter.try_acquire(), None);

        // A new window lets the next report through, carrying the suppressed count.
        limiter.state.lock().unwrap().window_start = Instant::now() - Duration::from_secs(61);
        assert_eq!(limiter.try_acquire(), Some(2));
    }

    #[test]
    fn test_limiter_zero_means_unlimited() {
        let limiter = DiagnosticsLimiter::new(0);
        for _ in 0..100 {
            assert_eq!(limiter.try_acquire(), Some(0));
        }
    }
}
