//! Which third-party hosts the scraped pages load, for choosing what `BlockedUrlsMiddleware` should
//! cut. Three counters, each labelled with the site the client asked for and the foreign host:
//!
//! - `browser_hive_worker_iframes_total` — cross-site iframes, sampled from `Target.getTargets`.
//! - `browser_hive_worker_third_party_requests_total` — cross-site sub-resource loads.
//! - `browser_hive_worker_third_party_requests_blocked_total` — loads dropped by the block list.
//!
//! A fourth, `browser_hive_worker_requests_blocked_by_type_total`, shares the cap and the
//! `page_site` label but carries `resource_type` instead of a host: loads dropped by
//! `BlockedResourceTypesMiddleware`. Those are in the host-level blocked counter as well, since
//! Chrome reports both kinds of block identically.
//!
//! **Two sources, because neither sees everything.** With site isolation on, a cross-site iframe
//! runs in its own renderer: the page's CDP session neither reports nor blocks what happens inside
//! it, and never blocks the frame's own document. The frame itself is a target, though, and so is
//! every frame nested inside it. So iframes are sampled from the browser's target list, and
//! sub-resources are counted from the page session's `Network` events, which cover the main page
//! and the frames running in its process.
//!
//! **Hosts are full hosts, not registrable domains.** A block pattern is written by hand for one
//! subdomain as often as for a whole domain, so the subdomains must stay visible. Registrable
//! domains are used only to decide what is cross-site.
//!
//! **`page_site` is the host the client requested**, even when the page redirected elsewhere: that
//! is the value the client's own source list holds, so it is the one a site can be looked up by.
//!
//! **Label combinations are capped per process** ([`MAX_SERIES_ENV`]). Past the cap a combination
//! is counted as `page_site="other"` and host `"other"`, so the total stays right and only its
//! breakdown is lost; an `other` row in a top table is the sign the cap was reached. Nothing is
//! ever evicted, since a counter that disappears and comes back breaks `increase()`.

use crate::browser_cdp::{BrowserCdpClient, TargetInfo};
use crate::browser_pool::BrowserPool;
use anyhow::Result;
use prometheus::{IntCounterVec, Opts, Registry};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Environment variable with the cap on label combinations, per metric, per worker process.
pub const MAX_SERIES_ENV: &str = "WORKER_THIRD_PARTY_METRICS_MAX_SERIES";

/// Worst case across the fleet is `4 × cap × pods` series, recreated with new pod names on every
/// rollout. A container restart keeps its pod name, so an OOMKill mints no new series.
pub const DEFAULT_MAX_SERIES: usize = 2000;

/// Label value that replaces both `page_site` and the host once the cap is reached.
const OVERFLOW: &str = "other";

/// `page_site` of an iframe whose browser context is not serving any known site.
pub const UNKNOWN_SITE: &str = "unknown";

/// How often the browser's targets are listed. An iframe that lives shorter than this can be missed.
///
/// A frame lives only while its page is loaded: until the context is destroyed (`always_new`) or
/// its tab navigates for the next request (`reusable`). Measured locally, that was about 1.5 s per
/// page, and a 5 s interval counted none of 8 frames in either mode. One `Target.getTargets` a
/// second is a millisecond round-trip on a socket nothing else uses.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Bound on each call of the sampler's own CDP connection. `Target.getTargets` answers in
/// milliseconds; the client's default bound is sized for context creation.
const SAMPLE_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Loads of one scrape request still waiting for their outcome. A page that keeps opening requests
/// that never finish must not grow the map without bound; past this, new loads are not counted.
const MAX_PENDING_LOADS: usize = 4096;

/// The cap from [`MAX_SERIES_ENV`], or [`DEFAULT_MAX_SERIES`] when unset or malformed.
pub fn max_series_from_env() -> usize {
    match std::env::var(MAX_SERIES_ENV) {
        Err(_) => DEFAULT_MAX_SERIES,
        Ok(value) => value.trim().parse().unwrap_or_else(|_| {
            warn!("{MAX_SERIES_ENV}={value:?} is not a number - using {DEFAULT_MAX_SERIES}");
            DEFAULT_MAX_SERIES
        }),
    }
}

/// Lowercase host of a web URL. `None` for anything without a network host (`data:`, `blob:`,
/// `about:blank`, extension pages).
pub fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return None;
    }
    parsed.host_str().map(str::to_ascii_lowercase)
}

/// Registrable domain of a host; the host itself for an IP address or a name the public suffix
/// list does not know (`localhost`).
fn registrable_domain(host: &str) -> &str {
    if host.starts_with('[') || host.parse::<IpAddr>().is_ok() {
        return host;
    }
    psl::domain_str(host).unwrap_or(host)
}

/// Whether two hosts belong to different sites, i.e. their registrable domains differ.
pub fn is_cross_site(page_host: &str, other_host: &str) -> bool {
    registrable_domain(page_host) != registrable_domain(other_host)
}

/// A counter with labels `scope`, `page_site` and one host label, whose `(page_site, host)`
/// combinations are capped per process.
struct CappedCounter {
    counter: IntCounterVec,
    admitted: Mutex<HashSet<(String, String)>>,
    cap: usize,
}

impl CappedCounter {
    fn register(
        registry: &Registry,
        name: &str,
        help: &str,
        host_label: &str,
        cap: usize,
    ) -> Result<Self> {
        let counter =
            IntCounterVec::new(Opts::new(name, help), &["scope", "page_site", host_label])?;
        registry.register(Box::new(counter.clone()))?;
        Ok(Self {
            counter,
            admitted: Mutex::new(HashSet::new()),
            cap,
        })
    }

    fn inc(&self, scope: &str, page_site: &str, host: &str) {
        let admitted = match self.admitted.lock() {
            Ok(mut admitted) => {
                let key = (page_site.to_string(), host.to_string());
                admitted.contains(&key) || (admitted.len() < self.cap && admitted.insert(key))
            }
            Err(_) => false,
        };
        let labels = if admitted {
            [scope, page_site, host]
        } else {
            [scope, OVERFLOW, OVERFLOW]
        };
        self.counter.with_label_values(&labels).inc();
    }
}

pub struct ThirdPartyMetrics {
    scope: String,
    iframes: CappedCounter,
    requests: CappedCounter,
    blocked: CappedCounter,
    blocked_by_type: CappedCounter,
}

impl ThirdPartyMetrics {
    pub fn register(registry: &Registry, scope: &str, max_series: usize) -> Result<Self> {
        Ok(Self {
            scope: scope.to_string(),
            iframes: CappedCounter::register(
                registry,
                "browser_hive_worker_iframes_total",
                "Cross-site iframes loaded by scraped pages, each frame counted once",
                "iframe_host",
                max_series,
            )?,
            requests: CappedCounter::register(
                registry,
                "browser_hive_worker_third_party_requests_total",
                "Cross-site sub-resource loads of scraped pages that were not blocked",
                "request_host",
                max_series,
            )?,
            blocked: CappedCounter::register(
                registry,
                "browser_hive_worker_third_party_requests_blocked_total",
                "Loads of scraped pages dropped by the scope's blocked-URL list",
                "request_host",
                max_series,
            )?,
            blocked_by_type: CappedCounter::register(
                registry,
                "browser_hive_worker_requests_blocked_by_type_total",
                "Loads of scraped pages dropped by the scope's blocked resource types",
                "resource_type",
                max_series,
            )?,
        })
    }

    /// Per-request state for counting the loads of one page, attributed to `page_site`.
    pub fn request_loads(self: &Arc<Self>, page_site: String) -> RequestLoads {
        RequestLoads {
            metrics: self.clone(),
            page_site,
            pending: HashMap::new(),
        }
    }
}

struct PendingLoad {
    host: String,
    document: bool,
}

/// Counts the loads of one scrape request from the page session's `Network` events.
///
/// A load is counted when its outcome is known — `loadingFinished`, or `loadingFailed`, which
/// separates a load dropped by the block list from one that went out. So `requestWillBeSent` only
/// remembers the host; a redirect arrives as another `requestWillBeSent` with the same id and
/// replaces it, and the final host is the one counted. Loads still pending when the request ends
/// are not counted.
pub struct RequestLoads {
    metrics: Arc<ThirdPartyMetrics>,
    page_site: String,
    pending: HashMap<String, PendingLoad>,
}

impl RequestLoads {
    pub fn will_be_sent(&mut self, request_id: &str, url: &str, document: bool) {
        let Some(host) = host_of(url) else {
            self.pending.remove(request_id);
            return;
        };
        if self.pending.len() >= MAX_PENDING_LOADS && !self.pending.contains_key(request_id) {
            return;
        }
        self.pending
            .insert(request_id.to_string(), PendingLoad { host, document });
    }

    pub fn finished(&mut self, request_id: &str) {
        if let Some(load) = self.pending.remove(request_id) {
            self.count_sent(&load);
        }
    }

    /// `blocked_by_list`: the failure is `blockedReason: inspector`, i.e. our own block list.
    pub fn failed(&mut self, request_id: &str, blocked_by_list: bool) {
        let Some(load) = self.pending.remove(request_id) else {
            return;
        };
        if blocked_by_list {
            // Any host and any type: a list can block a same-site path as well.
            let metrics = &self.metrics;
            metrics
                .blocked
                .inc(&metrics.scope, &self.page_site, &load.host);
        } else {
            self.count_sent(&load);
        }
    }

    /// A load dropped by `BlockedResourceTypesMiddleware`, by the type's metric label.
    pub fn blocked_by_type(&self, resource_type: &str) {
        let metrics = &self.metrics;
        metrics
            .blocked_by_type
            .inc(&metrics.scope, &self.page_site, resource_type);
    }

    /// Documents are left out: the main one is the page itself, and a frame's document is what
    /// the iframe metric counts.
    fn count_sent(&self, load: &PendingLoad) {
        if !load.document && is_cross_site(&self.page_site, &load.host) {
            let metrics = &self.metrics;
            metrics
                .requests
                .inc(&metrics.scope, &self.page_site, &load.host);
        }
    }
}

/// The iframes of a target list that were not in the previous sample, as `(page_site, host)`.
///
/// A frame is keyed by target id **and** host, so a frame that navigates to another host is
/// counted again for the new one. `seen` is replaced by the frames of this sample, which keeps it
/// as small as the browser's current target list.
fn new_iframes(
    targets: &[TargetInfo],
    sites: &HashMap<String, String>,
    seen: &mut HashSet<(String, String)>,
) -> Vec<(String, String)> {
    let mut current = HashSet::new();
    let mut found = Vec::new();
    for target in targets.iter().filter(|t| t.target_type == "iframe") {
        // A frame with no host yet is looked at again in the next sample.
        let Some(host) = host_of(&target.url) else {
            continue;
        };
        let key = (target.target_id.clone(), host.clone());
        if !seen.contains(&key) {
            let site = target
                .browser_context_id
                .as_ref()
                .and_then(|id| sites.get(id))
                .map_or(UNKNOWN_SITE, String::as_str);
            // An out-of-process frame is cross-site to its parent, not necessarily to the site
            // the client requested (after a redirect, say), so the check is repeated here.
            if site == UNKNOWN_SITE || is_cross_site(site, &host) {
                found.push((site.to_string(), host));
            }
        }
        current.insert(key);
    }
    *seen = current;
    found
}

/// Samples the browser's iframe targets over a CDP connection of its own.
///
/// Not the metrics probe's connection: that one refuses to wait for a call already in progress,
/// and a sample colliding with a Prometheus scrape would leave the scrape's gauges absent.
#[derive(Default)]
struct IframeSampler {
    client: Option<(String, BrowserCdpClient)>,
    seen: HashSet<(String, String)>,
}

impl IframeSampler {
    /// Blocking. A failed sample keeps `seen`, so frames still alive are not counted twice.
    fn sample(
        &mut self,
        ws_url: &str,
        sites: &HashMap<String, String>,
        metrics: &ThirdPartyMetrics,
    ) -> Result<()> {
        let client = match self.client.take() {
            Some((url, client)) if url == ws_url => self.client.insert((url, client)),
            // A recreated browser has a new endpoint.
            _ => self.client.insert((
                ws_url.to_string(),
                BrowserCdpClient::connect_with_timeout(ws_url, SAMPLE_CALL_TIMEOUT)?,
            )),
        };
        let targets = client.1.targets()?;
        for (site, host) in new_iframes(&targets, sites, &mut self.seen) {
            metrics.iframes.inc(&metrics.scope, &site, &host);
        }
        Ok(())
    }
}

/// Sample iframe targets every [`SAMPLE_INTERVAL`] for the life of the process.
///
/// One sample at a time: the next tick waits for the previous one, so a wedged browser holds at
/// most one blocking thread, each call bounded by [`SAMPLE_CALL_TIMEOUT`].
pub async fn run_iframe_sampler(
    metrics: Arc<ThirdPartyMetrics>,
    browser_pool: Arc<RwLock<BrowserPool>>,
) {
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sampler = IframeSampler::default();

    loop {
        interval.tick().await;
        let (ws_url, sites) = {
            let pool = browser_pool.read().await;
            (
                pool.get_browser().get_ws_url(),
                pool.sites_by_cdp_context().await,
            )
        };

        let metrics = metrics.clone();
        let span = tracing::Span::current();
        let task = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let result = sampler.sample(&ws_url, &sites, &metrics);
            (sampler, result)
        });
        sampler = match task.await {
            Ok((returned, result)) => {
                if let Err(e) = result {
                    debug!("Iframe sample failed: {e:#}");
                }
                returned
            }
            Err(e) => {
                warn!("Iframe sampler failed: {e}");
                IframeSampler::default()
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cap: usize) -> Arc<ThirdPartyMetrics> {
        Arc::new(ThirdPartyMetrics::register(&Registry::new(), "s", cap).unwrap())
    }

    fn value(counter: &CappedCounter, site: &str, host: &str) -> u64 {
        counter.counter.with_label_values(&["s", site, host]).get()
    }

    fn iframe(id: &str, url: &str, context: Option<&str>) -> TargetInfo {
        TargetInfo {
            target_id: id.to_string(),
            target_type: "iframe".to_string(),
            url: url.to_string(),
            browser_context_id: context.map(str::to_string),
        }
    }

    #[test]
    fn hosts_are_full_lowercase_web_hosts() {
        assert_eq!(
            host_of("https://Cdn.Tracker.example.com/a.js").as_deref(),
            Some("cdn.tracker.example.com")
        );
        assert_eq!(host_of("data:text/plain,hi"), None);
        assert_eq!(host_of("about:blank"), None);
        assert_eq!(host_of("chrome-extension://abc/x.js"), None);
    }

    #[test]
    fn cross_site_compares_registrable_domains() {
        assert!(!is_cross_site("www.example.com", "static.example.com"));
        assert!(!is_cross_site("shop.example.co.uk", "img.example.co.uk"));
        assert!(is_cross_site("www.example.com", "cdn.example.net"));
        assert!(is_cross_site("a.github.io", "b.github.io"));
        assert!(!is_cross_site("10.0.0.1", "10.0.0.1"));
        assert!(is_cross_site("10.0.0.1", "10.0.0.2"));
        assert!(!is_cross_site("localhost", "localhost"));
    }

    #[test]
    fn combinations_past_the_cap_are_counted_as_other() {
        let m = metrics(2);
        m.iframes.inc("s", "a.com", "x.net");
        m.iframes.inc("s", "a.com", "y.net");
        m.iframes.inc("s", "b.com", "x.net");
        m.iframes.inc("s", "a.com", "x.net");
        assert_eq!(value(&m.iframes, "a.com", "x.net"), 2);
        assert_eq!(value(&m.iframes, "a.com", "y.net"), 1);
        assert_eq!(value(&m.iframes, OVERFLOW, OVERFLOW), 1);
    }

    #[test]
    fn loads_are_counted_by_outcome() {
        let m = metrics(100);
        let mut loads = m.request_loads("www.site.com".to_string());

        loads.will_be_sent("1", "https://cdn.tracker.net/t.js", false);
        loads.finished("1");
        // Same site: not a third party.
        loads.will_be_sent("2", "https://img.site.com/a.png", false);
        loads.finished("2");
        // A failure that is not ours still went out.
        loads.will_be_sent("3", "https://cdn.tracker.net/b.js", false);
        loads.failed("3", false);
        // Blocked by the list, whatever the host.
        loads.will_be_sent("4", "https://img.site.com/pixel", false);
        loads.failed("4", true);
        // A frame document is the iframe metric's business.
        loads.will_be_sent("5", "https://ads.other.org/frame", true);
        loads.finished("5");
        // Redirected: the final host is counted.
        loads.will_be_sent("6", "https://t.redirect.io/r", false);
        loads.will_be_sent("6", "https://final.cdn.io/x", false);
        loads.finished("6");
        // Outcome without a start is ignored.
        loads.finished("7");

        assert_eq!(value(&m.requests, "www.site.com", "cdn.tracker.net"), 2);
        assert_eq!(value(&m.requests, "www.site.com", "img.site.com"), 0);
        assert_eq!(value(&m.requests, "www.site.com", "ads.other.org"), 0);
        assert_eq!(value(&m.requests, "www.site.com", "t.redirect.io"), 0);
        assert_eq!(value(&m.requests, "www.site.com", "final.cdn.io"), 1);
        assert_eq!(value(&m.blocked, "www.site.com", "img.site.com"), 1);
    }

    #[test]
    fn blocked_types_are_counted_per_site() {
        let m = metrics(100);
        let loads = m.request_loads("www.site.com".to_string());
        loads.blocked_by_type("media");
        loads.blocked_by_type("media");
        loads.blocked_by_type("font");

        assert_eq!(value(&m.blocked_by_type, "www.site.com", "media"), 2);
        assert_eq!(value(&m.blocked_by_type, "www.site.com", "font"), 1);
    }

    #[test]
    fn each_iframe_is_counted_once_per_host() {
        let sites = HashMap::from([("ctx".to_string(), "www.site.com".to_string())]);
        let mut seen = HashSet::new();

        let first = [
            iframe("f1", "https://ads.tracker.net/x", Some("ctx")),
            iframe("f2", "https://widget.site.com/w", Some("ctx")),
            iframe("f3", "about:blank", Some("ctx")),
            iframe("f4", "https://chat.vendor.io/", Some("gone")),
        ];
        assert_eq!(
            new_iframes(&first, &sites, &mut seen),
            vec![
                ("www.site.com".to_string(), "ads.tracker.net".to_string()),
                (UNKNOWN_SITE.to_string(), "chat.vendor.io".to_string()),
            ]
        );

        // f1 still there, f3 got its URL, f1 is gone next time and comes back as a new frame.
        let second = [
            iframe("f1", "https://ads.tracker.net/x", Some("ctx")),
            iframe("f3", "https://embed.maps.org/", Some("ctx")),
        ];
        assert_eq!(
            new_iframes(&second, &sites, &mut seen),
            vec![("www.site.com".to_string(), "embed.maps.org".to_string())]
        );

        let third = [iframe("f5", "https://ads.tracker.net/x", Some("ctx"))];
        assert_eq!(new_iframes(&third, &sites, &mut seen).len(), 1);
    }
}
