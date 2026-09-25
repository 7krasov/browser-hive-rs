//! Evidence that a page did not finish loading, used to tell a broken load from a missing
//! selector.
//!
//! A `SELECTOR_NOT_FOUND` (4042) is final for the client — the page loaded and the element is not
//! on it, so a retry would find the same page. That is wrong when the page never loaded in the
//! first place: a script bundle stuck on a dead connection or refused by a CDN leaves an unfilled
//! template, and a retry through another context usually succeeds. [`PageLoadTracker::verdict`]
//! finds the markers that separate the two, and the worker then answers `PAGE_LOAD_INCOMPLETE`
//! (5009), which the client retries.
//!
//! The listener only **records** here: which loads of the interesting types are still pending,
//! and how the others ended. Nothing is judged until the request has already failed with 4042 —
//! every other outcome drops the tracker unread.
//!
//! What counts (see ERROR_HANDLING.md, "When 5009 is returned"):
//! - C1 a script/stylesheet — or a same-site XHR/fetch — failed with a transport error;
//! - C2 the same, blocked by CORS or ORB/CORP (a CDN answering the asset with a block page);
//! - C3 the same, answered HTTP 403/429;
//! - C4 a script/stylesheet still pending for at least [`STALLED_AFTER`];
//! - C5 the main document itself still pending (the HTML was cut short).
//!
//! What does not: HTTP 404 (usually the site's own permanent bug), JS exceptions, images, fonts,
//! media, beacons, loads the page cancelled itself (`ERR_ABORTED`), loads our own block lists
//! dropped, and pending XHR/fetch (long-polling looks exactly like a stall).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use headless_chrome::protocol::cdp::Network::{BlockedReason, ResourceType};

use crate::third_party::{host_of, is_cross_site};

/// A script or stylesheet pending this long when the request gives up counts as stalled.
///
/// Not measured. The verdict is reached at least ~40 s after navigation, and a page's rendering
/// scripts are requested early, so a stalled one is tens of seconds old by then; the bound
/// only spares scripts requested in the last seconds (lazy widgets) and slow-but-alive loads
/// through a slow proxy (a main document was seen taking 10 s). Each pending load's age is in
/// the error message, so the value can be tuned from logs.
pub const STALLED_AFTER: Duration = Duration::from_secs(10);

/// Loads tracked per request; past this, new loads are ignored. Only four resource types are
/// tracked and finished loads are forgotten, so a real page stays far below it.
const MAX_TRACKED: usize = 4096;

/// Markers listed in the error message; the rest are counted.
const MAX_LISTED: usize = 5;

/// URLs in the error message are cut to this many characters (query strings are dropped first).
const MAX_URL_CHARS: usize = 120;

/// Transport-level failures: the connection broke, timed out or delivered a truncated body.
/// Proxy/tunnel errors are absent on purpose — they are reported as `PROXY_ERROR` (5007), which
/// takes precedence.
const TRANSPORT_ERRORS: [&str; 10] = [
    "ERR_TIMED_OUT",
    "ERR_CONNECTION_TIMED_OUT",
    "ERR_CONNECTION_RESET",
    "ERR_CONNECTION_CLOSED",
    "ERR_CONNECTION_ABORTED",
    "ERR_EMPTY_RESPONSE",
    "ERR_HTTP2_",
    "ERR_QUIC_PROTOCOL_ERROR",
    "ERR_INCOMPLETE_CHUNKED_ENCODING",
    "ERR_CONTENT_LENGTH_MISMATCH",
];

/// Response blocked by the browser's cross-origin read rules — what a CDN block page served in
/// place of an asset turns into.
const CROSS_ORIGIN_BLOCKS: [&str; 2] = ["ERR_BLOCKED_BY_ORB", "ERR_BLOCKED_BY_RESPONSE"];

/// The kinds of load whose loss can keep the page from rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Script or stylesheet, from any host: bundles are often served from a CDN on another
    /// domain.
    Asset,
    /// XHR or fetch — counted only same-site, since third-party ones are mostly analytics.
    Data,
}

impl Kind {
    /// The kind and the CDP name of a tracked type, or `None` for a type that is not tracked.
    fn of(resource_type: &ResourceType) -> Option<(Self, &'static str)> {
        match resource_type {
            ResourceType::Script => Some((Self::Asset, "Script")),
            ResourceType::Stylesheet => Some((Self::Asset, "Stylesheet")),
            ResourceType::Xhr => Some((Self::Data, "XHR")),
            ResourceType::Fetch => Some((Self::Data, "Fetch")),
            _ => None,
        }
    }
}

/// How a tracked load ended, as recorded by the listener. Judged only in the verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Pending,
    /// Answered with a status the verdict may care about (403/429); the body may still arrive.
    Status(u32),
    Failed {
        error_text: String,
        canceled: bool,
        blocked_by_us: bool,
        cors: bool,
    },
}

#[derive(Debug)]
struct Load {
    url: String,
    type_label: &'static str,
    kind: Kind,
    started: Instant,
    outcome: Outcome,
}

/// Records the loads of one scrape request that can explain a page that did not render.
pub struct PageLoadTracker {
    page_host: String,
    loads: HashMap<String, Load>,
    /// Request id of the main document while it is still loading.
    main_document: Option<String>,
}

impl PageLoadTracker {
    pub fn new(requested_url: &str) -> Self {
        Self {
            page_host: host_of(requested_url).unwrap_or_default(),
            loads: HashMap::new(),
            main_document: None,
        }
    }

    /// `Network.requestWillBeSent`. A redirect arrives as another event with the same id, which
    /// replaces the URL but keeps the original start time.
    pub fn will_be_sent(
        &mut self,
        request_id: &str,
        url: &str,
        resource_type: Option<&ResourceType>,
        is_main_document: bool,
    ) {
        if is_main_document {
            self.main_document = Some(request_id.to_string());
            return;
        }
        let Some((kind, type_label)) = resource_type.and_then(Kind::of) else {
            return;
        };
        if let Some(load) = self.loads.get_mut(request_id) {
            load.url = url.to_string();
            return;
        }
        if self.loads.len() >= MAX_TRACKED {
            return;
        }
        self.loads.insert(
            request_id.to_string(),
            Load {
                url: url.to_string(),
                type_label,
                kind,
                started: Instant::now(),
                outcome: Outcome::Pending,
            },
        );
    }

    /// `Network.responseReceived`. Only 403/429 are kept: every other status either loaded the
    /// resource or (404, 5xx) is not a marker.
    pub fn response(&mut self, request_id: &str, status: u32) {
        if matches!(status, 403 | 429) {
            if let Some(load) = self.loads.get_mut(request_id) {
                load.outcome = Outcome::Status(status);
            }
        }
    }

    /// `Network.loadingFinished`. A load that ended with a recorded 403/429 is kept.
    pub fn finished(&mut self, request_id: &str) {
        if self.main_document.as_deref() == Some(request_id) {
            self.main_document = None;
        }
        if let Some(load) = self.loads.get(request_id) {
            if load.outcome == Outcome::Pending {
                self.loads.remove(request_id);
            }
        }
    }

    /// `Network.loadingFailed`.
    pub fn failed(
        &mut self,
        request_id: &str,
        error_text: &str,
        canceled: bool,
        blocked_reason: Option<&BlockedReason>,
        cors: bool,
    ) {
        if self.main_document.as_deref() == Some(request_id) {
            // A failed main document is the navigation error path, not this one.
            self.main_document = None;
        }
        if let Some(load) = self.loads.get_mut(request_id) {
            load.outcome = Outcome::Failed {
                error_text: error_text.to_string(),
                canceled,
                blocked_by_us: matches!(blocked_reason, Some(BlockedReason::Inspector)),
                cors,
            };
        }
    }

    /// The markers of an incomplete load, as `(listed, total)`: human-readable descriptions of
    /// the first [`MAX_LISTED`] and the number found. Empty when the page looks fully loaded.
    ///
    /// Called only once the request has already failed with `SELECTOR_NOT_FOUND`.
    pub fn verdict(&self) -> (Vec<String>, usize) {
        self.verdict_at(Instant::now())
    }

    fn verdict_at(&self, now: Instant) -> (Vec<String>, usize) {
        let mut loads: Vec<(Instant, String)> = self
            .loads
            .values()
            .filter_map(|load| {
                let reason = self.judge(load, now)?;
                let text = format!("{} {} {}", load.type_label, reason, short_url(&load.url));
                Some((load.started, text))
            })
            .collect();
        // Oldest first: the earliest failure is the likeliest cause of the others.
        loads.sort_by_key(|(started, _)| *started);
        // C5 leads: it is the most telling marker there is.
        let markers: Vec<String> = self
            .main_document
            .iter()
            .map(|_| "main document still loading".to_string())
            .chain(loads.into_iter().map(|(_, text)| text))
            .collect();
        let total = markers.len();
        (markers.into_iter().take(MAX_LISTED).collect(), total)
    }

    /// Why this load is a marker, or `None`.
    fn judge(&self, load: &Load, now: Instant) -> Option<String> {
        if load.kind == Kind::Data && !self.is_same_site(&load.url) {
            return None;
        }
        match &load.outcome {
            // C4. Pending XHR/fetch is excluded: long-polling looks exactly like a stall.
            Outcome::Pending => {
                let age = now.saturating_duration_since(load.started);
                (load.kind == Kind::Asset && age >= STALLED_AFTER)
                    .then(|| format!("pending {}s", age.as_secs()))
            }
            // C3.
            Outcome::Status(status) => Some(format!("HTTP {}", status)),
            Outcome::Failed {
                error_text,
                canceled,
                blocked_by_us,
                cors,
            } => {
                if *canceled || *blocked_by_us || error_text.contains("ERR_ABORTED") {
                    return None;
                }
                let error = error_text.trim_start_matches("net::");
                if *cors {
                    // C2. CORS failures arrive as a plain ERR_FAILED; the status is what says so.
                    Some(format!("CORS {}", error))
                } else if CROSS_ORIGIN_BLOCKS.iter().any(|e| error_text.contains(e))
                    || TRANSPORT_ERRORS.iter().any(|e| error_text.contains(e))
                {
                    // C2 / C1.
                    Some(error.to_string())
                } else {
                    None
                }
            }
        }
    }

    fn is_same_site(&self, url: &str) -> bool {
        match host_of(url) {
            Some(host) if !self.page_host.is_empty() => !is_cross_site(&self.page_host, &host),
            _ => false,
        }
    }
}

/// A URL short enough for an error message: without its query and fragment, then truncated.
fn short_url(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    if base.chars().count() <= MAX_URL_CHARS {
        base.to_string()
    } else {
        let cut: String = base.chars().take(MAX_URL_CHARS).collect();
        format!("{}…", cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "https://www.example.com/listing/1";

    fn tracker() -> PageLoadTracker {
        PageLoadTracker::new(PAGE)
    }

    fn send(t: &mut PageLoadTracker, id: &str, url: &str, ty: ResourceType) {
        t.will_be_sent(id, url, Some(&ty), false);
    }

    fn fail(t: &mut PageLoadTracker, id: &str, error: &str) {
        t.failed(id, error, false, None, false);
    }

    fn later() -> Instant {
        Instant::now() + STALLED_AFTER + Duration::from_secs(1)
    }

    fn markers(t: &PageLoadTracker) -> Vec<String> {
        t.verdict_at(later()).0
    }

    #[test]
    fn fully_loaded_page_has_no_markers() {
        let mut t = tracker();
        t.will_be_sent("doc", PAGE, Some(&ResourceType::Document), true);
        send(
            &mut t,
            "1",
            "https://cdn.other.net/app.js",
            ResourceType::Script,
        );
        t.response("1", 200);
        t.finished("1");
        t.finished("doc");
        assert!(markers(&t).is_empty());
    }

    #[test]
    fn transport_failure_of_a_script_on_any_host_counts() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://cdn.other.net/app.js?v=3",
            ResourceType::Script,
        );
        fail(&mut t, "1", "net::ERR_CONNECTION_RESET");
        assert_eq!(
            markers(&t),
            vec!["Script ERR_CONNECTION_RESET https://cdn.other.net/app.js"]
        );
    }

    #[test]
    fn transport_failure_of_xhr_counts_only_same_site() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://api.example.com/data",
            ResourceType::Xhr,
        );
        fail(&mut t, "1", "net::ERR_TIMED_OUT");
        send(
            &mut t,
            "2",
            "https://analytics.other.net/c",
            ResourceType::Fetch,
        );
        fail(&mut t, "2", "net::ERR_TIMED_OUT");
        assert_eq!(
            markers(&t),
            vec!["XHR ERR_TIMED_OUT https://api.example.com/data"]
        );
    }

    #[test]
    fn cors_and_orb_blocks_count() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://static.example.com/a.css",
            ResourceType::Stylesheet,
        );
        t.failed("1", "net::ERR_FAILED", false, None, true);
        send(
            &mut t,
            "2",
            "https://static.example.com/b.js",
            ResourceType::Script,
        );
        fail(&mut t, "2", "net::ERR_BLOCKED_BY_ORB");
        assert_eq!(t.verdict_at(later()).1, 2);
    }

    #[test]
    fn only_403_and_429_statuses_count() {
        let mut t = tracker();
        for (id, status) in [("1", 403), ("2", 429), ("3", 404), ("4", 503)] {
            send(
                &mut t,
                id,
                "https://cdn.other.net/x.js",
                ResourceType::Script,
            );
            t.response(id, status);
            t.finished(id);
        }
        let (listed, total) = t.verdict_at(later());
        assert_eq!(total, 2);
        assert!(listed
            .iter()
            .all(|m| m.contains("HTTP 403") || m.contains("HTTP 429")));
    }

    #[test]
    fn page_cancelled_and_our_own_blocks_are_ignored() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://www.example.com/g/collect",
            ResourceType::Fetch,
        );
        t.failed("1", "net::ERR_ABORTED", true, None, false);
        send(
            &mut t,
            "2",
            "https://ads.other.net/tag.js",
            ResourceType::Script,
        );
        t.failed(
            "2",
            "net::ERR_BLOCKED_BY_CLIENT.Inspector",
            false,
            Some(&BlockedReason::Inspector),
            false,
        );
        assert!(markers(&t).is_empty());
    }

    #[test]
    fn a_404_and_non_critical_types_are_ignored() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://www.example.com/missing.js",
            ResourceType::Script,
        );
        t.response("1", 404);
        t.finished("1");
        send(
            &mut t,
            "2",
            "https://www.example.com/a.png",
            ResourceType::Image,
        );
        fail(&mut t, "2", "net::ERR_CONNECTION_RESET");
        assert!(markers(&t).is_empty());
    }

    #[test]
    fn a_stalled_script_counts_but_a_young_one_or_pending_xhr_does_not() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://cdn.other.net/app.js",
            ResourceType::Script,
        );
        send(
            &mut t,
            "2",
            "https://www.example.com/poll",
            ResourceType::Xhr,
        );
        assert!(
            t.verdict_at(Instant::now()).0.is_empty(),
            "too young to count"
        );
        let listed = markers(&t);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].starts_with("Script pending "), "{}", listed[0]);
    }

    #[test]
    fn a_main_document_still_loading_counts_and_is_listed_first() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://cdn.other.net/app.js",
            ResourceType::Script,
        );
        fail(&mut t, "1", "net::ERR_EMPTY_RESPONSE");
        t.will_be_sent("doc", PAGE, Some(&ResourceType::Document), true);
        assert_eq!(markers(&t)[0], "main document still loading");
    }

    #[test]
    fn proxy_errors_are_left_to_proxy_error() {
        let mut t = tracker();
        send(
            &mut t,
            "1",
            "https://cdn.other.net/app.js",
            ResourceType::Script,
        );
        fail(&mut t, "1", "net::ERR_TUNNEL_CONNECTION_FAILED");
        assert!(markers(&t).is_empty());
    }

    #[test]
    fn long_urls_are_shortened() {
        let long = format!("https://cdn.other.net/{}", "a".repeat(200));
        assert!(short_url(&long).chars().count() <= MAX_URL_CHARS + 1);
        assert_eq!(short_url("https://x.net/a.js?q=1#f"), "https://x.net/a.js");
    }
}
