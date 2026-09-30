//! Waiting out an anti-bot challenge instead of returning its page.
//!
//! Some anti-bot systems answer the first request with a challenge page (typically HTTP 403) whose
//! JavaScript proves the visitor is a browser and then **navigates the main frame again** — often a
//! form POST to the same URL — to the real document. Returning the first page, as the `network_idle`
//! early exit does for 403, returns the challenge; waiting the whole timeout for every request
//! would cost the scopes that never see one.
//!
//! The decision is split in three:
//! - **What is a challenge** is decided by the scope's [`ChallengeDetector`]s, never by a status
//!   list: a 403 is just as often a hard block, and a challenge is recognisable only by vendor
//!   markers (a response header, usually). The base ships the generic [`HeaderChallengeDetector`]
//!   and names no vendor — which headers mean "challenge" is the deployment's call.
//! - **Whether it passed** is read from the response observer through a [`MainDocumentProbe`]: a
//!   *new* main-frame document that no detector flags. Nothing is evaluated in the page while the
//!   challenge runs, so the wait adds no fingerprint of its own.
//! - **How long to wait** is the request's `challenge_timeout_ms`, spent inside its wait budget.
//!
//! With no detector or no window nothing here runs, and the request takes exactly the path it took
//! before this module existed.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_cancellation_ext::{check_cancellation, CancellationToken};

/// Wire-level facts of one main-frame document response, as the response observer captured them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MainDocument {
    /// HTTP status (0 if unknown).
    pub status: u32,
    /// Response headers as CDP delivers them: names in the server's own case, a repeated header
    /// as one entry with its values joined by `\n`.
    pub headers: HashMap<String, String>,
    /// URL of the response.
    pub url: String,
}

impl MainDocument {
    pub fn new(status: u32, headers: HashMap<String, String>, url: String) -> Self {
        Self {
            status,
            headers,
            url,
        }
    }

    /// Value of a header, looked up case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Decides whether a main-frame document is an anti-bot challenge that its own JavaScript may
/// pass. A hard block (a page nothing on it can lift) must **not** be flagged: waiting on it only
/// spends the client's budget.
pub trait ChallengeDetector: Send + Sync {
    fn is_challenge(&self, doc: &MainDocument) -> bool;

    /// Name for startup logs.
    fn name(&self) -> &str;
}

/// Flags a document carrying any of the configured headers — with the given value, compared
/// case-insensitively, or with any value when the value is `None`. Header names are
/// case-insensitive, as in HTTP. A repeated header (values joined by `\n`) matches when any of its
/// values does.
#[derive(Debug, Clone)]
pub struct HeaderChallengeDetector {
    rules: Vec<(String, Option<String>)>,
}

impl HeaderChallengeDetector {
    pub fn new(rules: Vec<(String, Option<String>)>) -> Self {
        Self { rules }
    }
}

impl ChallengeDetector for HeaderChallengeDetector {
    fn is_challenge(&self, doc: &MainDocument) -> bool {
        self.rules.iter().any(|(name, expected)| {
            doc.header(name).is_some_and(|value| match expected {
                None => true,
                Some(expected) => value
                    .split('\n')
                    .any(|v| v.trim().eq_ignore_ascii_case(expected.trim())),
            })
        })
    }

    fn name(&self) -> &str {
        "header"
    }
}

/// The latest main-frame document of the current navigation, as seen by the response observer.
#[derive(Debug, Clone)]
pub struct DocumentSnapshot {
    /// 1 for the first document response of the request, incremented for every later one
    /// (a redirect hop, a reload, the navigation that follows a passed challenge).
    pub seq: u64,
    /// Whether the document's body finished loading (or failed): by then it has replaced the
    /// previous document in the frame, so the page can be waited on and read.
    pub loaded: bool,
    pub doc: MainDocument,
}

/// Read access to the response observer, implemented by the worker. Must be cheap: it is polled
/// every [`CHALLENGE_POLL_INTERVAL`] while a challenge runs.
pub trait MainDocumentProbe: Send + Sync {
    /// `None` until the first main-frame document response arrives.
    fn latest(&self) -> Option<DocumentSnapshot>;
}

/// How often the probe is read while waiting. A read is a mutex lock, not a CDP call.
pub const CHALLENGE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What happened to a request's challenge; the `challenge` label of the page-request metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeOutcome {
    /// The document was not a challenge.
    None,
    /// A challenge was replaced by a document no detector flags.
    Passed,
    /// A challenge was still in place when the window ran out.
    Failed,
    /// A challenge, but the request asked for no window, so it was not waited on.
    Skipped,
}

impl ChallengeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Whether any detector flags the document.
pub fn is_challenge(
    detectors: &[std::sync::Arc<dyn ChallengeDetector>],
    doc: &MainDocument,
) -> bool {
    detectors.iter().any(|d| d.is_challenge(doc))
}

/// Waits until the challenge on the current page is passed, or `window` runs out.
///
/// Returns [`ChallengeOutcome::None`] at once when the first document is not a challenge (or none
/// arrives within the window, e.g. a navigation with no response), `Passed` on the first *later*
/// document that is loaded and not a challenge, and `Failed` when the window ends first — a new
/// document that is still a challenge (a challenge reloading itself) keeps the wait going.
/// Blocking: call it from the blocking pool, like the wait strategies.
///
/// Callers skip it entirely when there are no detectors or the window is zero.
pub fn wait_out_challenge(
    probe: &dyn MainDocumentProbe,
    detectors: &[std::sync::Arc<dyn ChallengeDetector>],
    window: Duration,
    cancellation_token: &CancellationToken,
) -> Result<ChallengeOutcome> {
    let start = Instant::now();
    let mut first: Option<u64> = None;
    loop {
        check_cancellation(cancellation_token, "challenge")?;
        if let Some(snapshot) = probe.latest() {
            match first {
                None => {
                    if !is_challenge(detectors, &snapshot.doc) {
                        return Ok(ChallengeOutcome::None);
                    }
                    tracing::info!(
                        "Challenge detected (HTTP {}); waiting up to {:?} for it to pass",
                        snapshot.doc.status,
                        window
                    );
                    first = Some(snapshot.seq);
                }
                Some(first_seq) => {
                    if snapshot.seq > first_seq
                        && snapshot.loaded
                        && !is_challenge(detectors, &snapshot.doc)
                    {
                        tracing::info!(
                            "Challenge passed after {:?}: HTTP {} {}",
                            start.elapsed(),
                            snapshot.doc.status,
                            snapshot.doc.url
                        );
                        return Ok(ChallengeOutcome::Passed);
                    }
                }
            }
        }
        if start.elapsed() >= window {
            return Ok(match first {
                None => ChallengeOutcome::None,
                Some(_) => {
                    tracing::info!("Challenge not passed within {:?}", window);
                    ChallengeOutcome::Failed
                }
            });
        }
        std::thread::sleep(CHALLENGE_POLL_INTERVAL.min(window.saturating_sub(start.elapsed())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn doc(status: u32, headers: &[(&str, &str)]) -> MainDocument {
        MainDocument::new(
            status,
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            "https://site.test/".into(),
        )
    }

    fn detector() -> Vec<Arc<dyn ChallengeDetector>> {
        vec![Arc::new(HeaderChallengeDetector::new(vec![(
            "X-Challenge".into(),
            Some("challenge".into()),
        )]))]
    }

    /// Plays a scripted sequence of snapshots: each `latest()` call returns the next one, and the
    /// last one sticks.
    struct Script(Mutex<Vec<Option<DocumentSnapshot>>>);

    impl Script {
        fn new(mut steps: Vec<Option<DocumentSnapshot>>) -> Self {
            steps.reverse();
            Self(Mutex::new(steps))
        }
    }

    impl MainDocumentProbe for Script {
        fn latest(&self) -> Option<DocumentSnapshot> {
            let mut steps = self.0.lock().unwrap();
            if steps.len() > 1 {
                steps.pop().unwrap()
            } else {
                steps.last().cloned().flatten()
            }
        }
    }

    fn snap(seq: u64, loaded: bool, doc: MainDocument) -> Option<DocumentSnapshot> {
        Some(DocumentSnapshot { seq, loaded, doc })
    }

    fn run(steps: Vec<Option<DocumentSnapshot>>, window_ms: u64) -> ChallengeOutcome {
        wait_out_challenge(
            &Script::new(steps),
            &detector(),
            Duration::from_millis(window_ms),
            &CancellationToken::new(),
        )
        .unwrap()
    }

    fn challenge() -> MainDocument {
        doc(403, &[("x-challenge", "challenge")])
    }

    #[test]
    fn header_detector_matches_names_and_values_case_insensitively() {
        let d = &detector()[0];
        assert!(d.is_challenge(&doc(403, &[("x-challenge", "Challenge")])));
        assert!(d.is_challenge(&doc(403, &[("X-CHALLENGE", "other\nchallenge")])));
        assert!(!d.is_challenge(&doc(403, &[("x-challenge", "block")])));
        assert!(!d.is_challenge(&doc(403, &[])));
        let any_value = HeaderChallengeDetector::new(vec![("x-challenge".into(), None)]);
        assert!(any_value.is_challenge(&doc(200, &[("X-Challenge", "")])));
    }

    #[test]
    fn a_page_that_is_not_a_challenge_returns_at_once() {
        let start = Instant::now();
        assert_eq!(
            run(vec![snap(1, false, doc(403, &[]))], 5_000),
            ChallengeOutcome::None
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_later_loaded_non_challenge_document_passes() {
        let steps = vec![
            None,
            snap(1, false, challenge()),
            snap(1, true, challenge()),
            // A new document that has not finished loading is not the page yet.
            snap(2, false, doc(200, &[])),
            snap(2, true, doc(200, &[])),
        ];
        assert_eq!(run(steps, 5_000), ChallengeOutcome::Passed);
    }

    #[test]
    fn a_challenge_replaced_by_another_challenge_keeps_waiting_until_the_window_ends() {
        let steps = vec![snap(1, true, challenge()), snap(2, true, challenge())];
        let start = Instant::now();
        assert_eq!(run(steps, 300), ChallengeOutcome::Failed);
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn no_document_within_the_window_is_not_a_challenge() {
        assert_eq!(run(vec![None], 200), ChallengeOutcome::None);
    }

    #[test]
    fn cancellation_ends_the_wait() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(wait_out_challenge(
            &Script::new(vec![snap(1, true, challenge())]),
            &detector(),
            Duration::from_secs(5),
            &token,
        )
        .is_err());
    }
}
