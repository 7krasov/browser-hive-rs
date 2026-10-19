//! What a page did while it loaded: its XHR/fetch requests and its lifecycle timeline.
//!
//! Part of browser diagnostics. Failure-only signals cannot explain a page whose content is
//! rendered by JavaScript from an API response that never came: nothing failed, so nothing was
//! captured, and the returned HTML shows a loading spinner where the content should be. What
//! explains it is the API request itself — still pending, slow, or answered with a tiny body —
//! and when the `load` event fired, since such pages often start their API calls only then.
//!
//! Everything here comes from events the `Network` and `Page` domains already deliver (the
//! response observer enables `Network`; headless_chrome enables `Page` lifecycle events), so it
//! costs no CDP call. Unlike [`crate::page_load::PageLoadTracker`] it records cross-site requests
//! and pending ones too: it never decides an error code, it only reports.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use headless_chrome::protocol::cdp::Network::ResourceType;

use crate::page_load::short_url;

/// XHR/fetch requests recorded per request; later ones are only counted. A page that polls can
/// issue hundreds, and the report must stay one readable line.
const MAX_DATA_REQUESTS: usize = 40;

/// Main-frame lifecycle events reported in the timeline, in the order they normally occur.
const TIMELINE_EVENTS: [&str; 4] = [
    "DOMContentLoaded",
    "load",
    "networkAlmostIdle",
    "networkIdle",
];

#[derive(Debug, Clone, PartialEq)]
enum State {
    Pending,
    Finished { took: Duration, bytes: f64 },
    Failed { took: Duration, error: String },
}

#[derive(Debug)]
struct DataRequest {
    method: String,
    url: String,
    type_label: &'static str,
    /// Offset from the start of capture.
    started: Duration,
    status: Option<u32>,
    state: State,
}

/// The XHR/fetch requests and main-frame lifecycle events of one scrape request.
pub struct PageActivity {
    started: Instant,
    requests: HashMap<String, DataRequest>,
    untracked: usize,
    /// Offset of the latest occurrence of each [`TIMELINE_EVENTS`] entry.
    timeline: [Option<Duration>; TIMELINE_EVENTS.len()],
}

impl Default for PageActivity {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl PageActivity {
    /// `started` is the origin of every offset in the report: the start of capture, which comes
    /// right before navigation.
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            requests: HashMap::new(),
            untracked: 0,
            timeline: [None; TIMELINE_EVENTS.len()],
        }
    }

    /// `Network.requestWillBeSent`. A redirect repeats the request id: the URL is replaced and
    /// the start time kept.
    pub fn will_be_sent(
        &mut self,
        request_id: &str,
        method: &str,
        url: &str,
        resource_type: Option<&ResourceType>,
        now: Instant,
    ) {
        let type_label = match resource_type {
            Some(ResourceType::Xhr) => "XHR",
            Some(ResourceType::Fetch) => "Fetch",
            _ => return,
        };
        if let Some(request) = self.requests.get_mut(request_id) {
            request.url = url.to_string();
            return;
        }
        if self.requests.len() >= MAX_DATA_REQUESTS {
            self.untracked += 1;
            return;
        }
        self.requests.insert(
            request_id.to_string(),
            DataRequest {
                method: method.to_string(),
                url: url.to_string(),
                type_label,
                started: self.offset(now),
                status: None,
                state: State::Pending,
            },
        );
    }

    /// `Network.responseReceived`.
    pub fn response(&mut self, request_id: &str, status: u32) {
        if let Some(request) = self.requests.get_mut(request_id) {
            request.status = Some(status);
        }
    }

    /// `Network.loadingFinished`. `bytes` is the encoded (on-the-wire) body size.
    pub fn finished(&mut self, request_id: &str, bytes: f64, now: Instant) {
        let offset = self.offset(now);
        if let Some(request) = self.requests.get_mut(request_id) {
            request.state = State::Finished {
                took: offset.saturating_sub(request.started),
                bytes,
            };
        }
    }

    /// `Network.loadingFailed`. Loads the scope blocked itself are dropped from the report: they
    /// are expected and counted elsewhere (`blocked_requests`).
    pub fn failed(
        &mut self,
        request_id: &str,
        error_text: &str,
        blocked_by_us: bool,
        now: Instant,
    ) {
        if blocked_by_us {
            self.requests.remove(request_id);
            return;
        }
        let offset = self.offset(now);
        if let Some(request) = self.requests.get_mut(request_id) {
            request.state = State::Failed {
                took: offset.saturating_sub(request.started),
                error: error_text.trim_start_matches("net::").to_string(),
            };
        }
    }

    /// `Page.lifecycleEvent` of the main frame. The latest occurrence wins: a second main
    /// document (a passed challenge, a script navigation) restarts the lifecycle, and it is the
    /// page that was returned that matters.
    pub fn lifecycle(&mut self, name: &str, now: Instant) {
        if let Some(index) = TIMELINE_EVENTS.iter().position(|event| *event == name) {
            self.timeline[index] = Some(self.offset(now));
        }
    }

    /// One line listing the recorded XHR/fetch requests in start order, or `None` when the page
    /// made none.
    pub fn data_requests_report(&self, now: Instant) -> Option<String> {
        if self.requests.is_empty() {
            return None;
        }
        let now = self.offset(now);
        let mut requests: Vec<&DataRequest> = self.requests.values().collect();
        requests.sort_by_key(|request| request.started);
        let pending = requests
            .iter()
            .filter(|request| request.state == State::Pending)
            .count();
        let entries: Vec<String> = requests
            .iter()
            .map(|request| describe(request, now))
            .collect();
        Some(format!(
            "{} recorded, {} pending{}: {:?}",
            requests.len(),
            pending,
            if self.untracked > 0 {
                format!(", {} more not recorded", self.untracked)
            } else {
                String::new()
            },
            entries
        ))
    }

    /// The main-frame lifecycle as offsets from the start of capture, e.g.
    /// `DOMContentLoaded +4.1s, load +19.8s, networkAlmostIdle +19.9s, networkIdle not reached`.
    pub fn timeline_report(&self) -> String {
        TIMELINE_EVENTS
            .iter()
            .zip(self.timeline.iter())
            .map(|(name, at)| match at {
                Some(at) => format!("{} +{}", name, secs(*at)),
                None => format!("{} not reached", name),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn offset(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }
}

/// `XHR POST https://api.example.com/v1/items +19.9s → 200 in 0.4s, 12.3 KB`.
fn describe(request: &DataRequest, now: Duration) -> String {
    let status = match request.status {
        Some(status) => format!("{} ", status),
        None => String::new(),
    };
    let outcome = match &request.state {
        State::Pending => format!(
            "{}pending {}",
            status,
            secs(now.saturating_sub(request.started))
        ),
        State::Finished { took, bytes } => {
            format!("{}in {}, {}", status, secs(*took), size(*bytes))
        }
        State::Failed { took, error } => {
            format!("{}failed after {}: {}", status, secs(*took), error)
        }
    };
    format!(
        "{} {} {} +{} → {}",
        request.type_label,
        request.method,
        short_url(&request.url),
        secs(request.started),
        outcome
    )
}

fn secs(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

fn size(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{} B", bytes.max(0.0) as u64)
    } else {
        format!("{:.1} KB", bytes / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn reports_pending_finished_and_failed_requests_in_start_order() {
        let start = Instant::now();
        let mut activity = PageActivity::new(start);
        activity.will_be_sent(
            "2",
            "POST",
            "https://api.example.net/v1/items?token=secret",
            Some(&ResourceType::Xhr),
            at(start, 20_000),
        );
        activity.will_be_sent(
            "1",
            "GET",
            "https://www.example.com/config",
            Some(&ResourceType::Fetch),
            at(start, 1_000),
        );
        activity.response("1", 200);
        activity.finished("1", 2_048.0, at(start, 1_500));
        activity.will_be_sent(
            "3",
            "GET",
            "https://www.example.com/track",
            Some(&ResourceType::Xhr),
            at(start, 5_000),
        );
        activity.failed("3", "net::ERR_CONNECTION_RESET", false, at(start, 8_000));

        let report = activity.data_requests_report(at(start, 41_000)).unwrap();
        assert_eq!(
            report,
            "3 recorded, 1 pending: [\
             \"Fetch GET https://www.example.com/config +1.0s → 200 in 0.5s, 2.0 KB\", \
             \"XHR GET https://www.example.com/track +5.0s → failed after 3.0s: ERR_CONNECTION_RESET\", \
             \"XHR POST https://api.example.net/v1/items +20.0s → pending 21.0s\"]"
        );
    }

    #[test]
    fn ignores_other_types_and_self_blocked_loads() {
        let start = Instant::now();
        let mut activity = PageActivity::new(start);
        activity.will_be_sent(
            "1",
            "GET",
            "https://x.example/a.js",
            Some(&ResourceType::Script),
            start,
        );
        activity.will_be_sent(
            "2",
            "GET",
            "https://x.example/beacon",
            Some(&ResourceType::Xhr),
            start,
        );
        activity.failed("2", "net::ERR_BLOCKED_BY_CLIENT", true, start);
        assert_eq!(activity.data_requests_report(start), None);
    }

    #[test]
    fn redirect_keeps_start_and_cap_counts_the_rest() {
        let start = Instant::now();
        let mut activity = PageActivity::new(start);
        activity.will_be_sent(
            "r",
            "GET",
            "https://a.example/old",
            Some(&ResourceType::Xhr),
            start,
        );
        activity.will_be_sent(
            "r",
            "GET",
            "https://a.example/new",
            Some(&ResourceType::Xhr),
            at(start, 900),
        );
        for i in 0..MAX_DATA_REQUESTS + 2 {
            activity.will_be_sent(
                &format!("n{}", i),
                "GET",
                "https://a.example/p",
                Some(&ResourceType::Fetch),
                start,
            );
        }
        let report = activity.data_requests_report(start).unwrap();
        assert!(
            report.starts_with("40 recorded, 40 pending, 3 more not recorded: "),
            "{}",
            report
        );
        assert!(
            report.contains("XHR GET https://a.example/new +0.0s"),
            "{}",
            report
        );
    }

    #[test]
    fn timeline_keeps_the_latest_occurrence_and_names_missing_events() {
        let start = Instant::now();
        let mut activity = PageActivity::new(start);
        activity.lifecycle("init", at(start, 100));
        activity.lifecycle("load", at(start, 2_000));
        activity.lifecycle("DOMContentLoaded", at(start, 4_100));
        activity.lifecycle("load", at(start, 19_800));
        activity.lifecycle("networkAlmostIdle", at(start, 19_900));
        assert_eq!(
            activity.timeline_report(),
            "DOMContentLoaded +4.1s, load +19.8s, networkAlmostIdle +19.9s, networkIdle not reached"
        );
    }
}
