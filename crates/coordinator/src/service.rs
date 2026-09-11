use crate::in_flight::InFlightTracker;
use crate::local_worker_discovery::LocalWorkerDiscovery;
use crate::metrics::{CoordinatorMetrics, FleetView, RejectReason, RequestMetrics, RetryReason};
use crate::worker_discovery::{ScopePresence, WorkerDiscovery};
use anyhow::Result;
use browser_hive_common::{CoordinatorConfig, SessionId, SessionManager, WorkerEndpoint};
use browser_hive_proto::coordinator::{
    scraper_coordinator_server::ScraperCoordinator, ClusterStatsResponse, ScopeStatsProto,
    ScrapePageRequest, ScrapePageResponse,
};
use browser_hive_proto::worker::worker_service_client::WorkerServiceClient;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn, Instrument};

/// Worker discovery implementation - either K8s-based or local
pub enum WorkerDiscoveryImpl {
    Kubernetes(WorkerDiscovery),
    Local(LocalWorkerDiscovery),
}

impl WorkerDiscoveryImpl {
    pub fn get_workers(&self) -> Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>> {
        match self {
            WorkerDiscoveryImpl::Kubernetes(discovery) => discovery.get_workers(),
            WorkerDiscoveryImpl::Local(discovery) => discovery.get_workers(),
        }
    }

    /// Scopes that exist at all, as opposed to scopes that can currently serve a request.
    pub fn get_known_scopes(&self) -> Arc<RwLock<HashMap<String, ScopePresence>>> {
        match self {
            WorkerDiscoveryImpl::Kubernetes(discovery) => discovery.get_known_scopes(),
            WorkerDiscoveryImpl::Local(discovery) => discovery.get_known_scopes(),
        }
    }
}

// gRPC client timeout when calling workers
const GRPC_CLIENT_TIMEOUT: Duration = Duration::from_secs(330); // 10 seconds more than server timeout

// Timeout for fetching fresh stats from worker (used to avoid stale cache rejections)
const FRESH_STATS_TIMEOUT: Duration = Duration::from_secs(2);

// Maximum size of a worker response the coordinator will decode.
//
// A scraped page is post-JavaScript `outerHTML` and can run to tens of MB, far past
// tonic's 4 MiB default for *received* messages — past which the call fails with
// `OutOfRange` and nothing is returned. Only the receiving side needs raising: the
// send side defaults to unbounded, so the worker already encodes any size and the
// coordinator already forwards it. A client of the coordinator has its own receive
// limit (4 MiB in most gRPC implementations) and must raise it to match.
const MAX_WORKER_RESPONSE_SIZE: usize = 70 * 1024 * 1024;

/// How to answer a request whose scope has no routable worker.
///
/// The scope has left (or never entered) the map discovery routes on. That happens for two
/// unrelated reasons, and conflating them is what made a pod restart look like a client
/// configuration error: a worker's gRPC server comes up only after its browser pool is ready, so
/// every pod of a scope is unreachable for the length of a restart (measured: ~9 s of browser
/// launch, plus up to the 10 s discovery interval) and the whole scope vanishes from routing.
///
/// [`ScopePresence`] answers the other question — does the cluster have pods labelled with this
/// scope at all — from the pod list discovery already fetches, so:
///
/// - **present** → transient. `NO_WORKERS_AVAILABLE` (5001), the code a client already treats as
///   retryable, with the pod breakdown in the message so the client can see waiting will help.
///   Deliberately not a new error code: 5001 already means "nothing can serve this right now",
///   and the finer distinction (capacity vs. restart) is carried by the `reason` label on
///   `browser_hive_coordinator_requests_rejected_total`, where operators need it, rather than by
///   an enum every client would have to learn.
/// - **absent** → `SCOPE_NOT_FOUND` (4003) keeps meaning only what it says: no such scope exists,
///   retrying will never help, fix the name.
/// A rolling restart can put several pods of a scope into TERMINATING at once, so that case is
/// worth walking down the list.
const MAX_TERMINATING_ATTEMPTS: u32 = 3;

/// Capacity gets **one** extra attempt, deliberately fewer than TERMINATING. A
/// `CAPACITY_EXHAUSTED` answer means the scope is at its limit, and that is the worst moment to
/// multiply RPCs across it: three attempts per client would turn a saturated scope into a
/// self-amplifying load generator. One retry covers what this is actually for — the few seconds of
/// staleness in the coordinator's slot cache, where a *different* pod really does have room.
const MAX_CAPACITY_ATTEMPTS: u32 = 2;

/// How a worker answer that another pod could still serve should be retried.
pub(crate) struct RetryPlan {
    reason: RetryReason,
    max_attempts: u32,
    code_name: &'static str,
}

/// Decide whether a worker's answer is worth re-sending to a different pod.
///
/// Only two answers are: the worker is shutting down, and the worker had no free slot. Everything
/// else — success, a dead browser, a selector that is not there, a 403 — would be answered the
/// same way anywhere in the scope, so retrying it only spends the client's deadline.
///
/// `no_session` is what keeps capacity retries honest: a session lives in one context on one pod,
/// so "somewhere else" is a different session. In practice a session request cannot produce
/// `CAPACITY_EXHAUSTED` at all (it is answered with `SESSION_NOT_FOUND` or `SESSION_BUSY`), and
/// this guard keeps that true if the worker's lookup ever changes.
fn classify_retry(error_code: i32, no_session: bool) -> Option<RetryPlan> {
    if error_code == browser_hive_proto::coordinator::ErrorCode::Terminating as i32 {
        return Some(RetryPlan {
            reason: RetryReason::Terminating,
            max_attempts: MAX_TERMINATING_ATTEMPTS,
            code_name: "TERMINATING",
        });
    }
    if error_code == browser_hive_proto::worker::ErrorCode::CapacityExhausted as i32 && no_session {
        return Some(RetryPlan {
            reason: RetryReason::NoSlots,
            max_attempts: MAX_CAPACITY_ATTEMPTS,
            code_name: "CAPACITY_EXHAUSTED",
        });
    }
    None
}

fn classify_missing_scope(
    scope_name: &str,
    presence: Option<ScopePresence>,
) -> (RejectReason, i32, String) {
    match presence {
        Some(presence) => (
            RejectReason::ScopeUnavailable,
            browser_hive_proto::coordinator::ErrorCode::NoWorkersAvailable as i32,
            format!(
                "Scope {} is temporarily unavailable ({}); retry shortly",
                scope_name,
                presence.describe()
            ),
        ),
        None => (
            RejectReason::ScopeNotFound,
            browser_hive_proto::coordinator::ErrorCode::ScopeNotFound as i32,
            format!("Scope not found: {}", scope_name),
        ),
    }
}

/// Result of worker selection
#[derive(Debug, Clone)]
pub struct SelectedWorker<'a> {
    pub worker: &'a WorkerEndpoint,
    /// Free slots the worker was ranked by: the discovery cache corrected by in-flight requests,
    /// not the raw `worker.stats.available_slots`.
    pub free_slots: usize,
    pub used_fallback: bool, // true if no healthy workers were found and we fell back to all workers
}

/// Select the best worker from a list of scope workers based on available slots
///
/// # Algorithm
/// 1. Filter workers by healthy status (must be in `healthy_workers` set)
/// 2. If no healthy workers, fall back to all workers (race condition window)
/// 3. Select the worker with the most free slots (see [`pick_most_free`])
///
/// # Returns
/// - `Some(SelectedWorker)` with the best worker and whether fallback was used
/// - `None` if no workers available
/// # Parameters
/// * `free_slots` - free slots of a worker. In production that is
///   [`InFlightTracker::free_slots`], which corrects the discovery cache by the requests
///   dispatched since it was taken; a parameter so selection stays a pure function in tests.
/// * `rotation` - a counter that advances once per routing decision; it selects among the workers
///   that tie on free slots. See [`pick_most_free`].
pub fn select_best_worker<'a>(
    scope_workers: &'a [WorkerEndpoint],
    healthy_workers: &HashSet<String>,
    free_slots: impl Fn(&WorkerEndpoint) -> usize,
    rotation: u64,
) -> Option<SelectedWorker<'a>> {
    let healthy: Vec<_> = scope_workers
        .iter()
        .filter(|w| healthy_workers.contains(&w.pod_name))
        .collect();

    let (candidates, used_fallback) = if healthy.is_empty() {
        // Fall back to all workers if no healthy ones
        (scope_workers.iter().collect::<Vec<_>>(), true)
    } else {
        (healthy, false)
    };

    pick_most_free(candidates, free_slots, rotation).map(|(worker, free_slots)| SelectedWorker {
        worker,
        free_slots,
        used_fallback,
    })
}

/// Of `candidates`, the worker with the most free slots, ties broken round-robin.
///
/// Both routing decisions go through here — the first one and the retry after
/// `CAPACITY_EXHAUSTED`. The retry used to rank with a bare `max_by_key` on the cached
/// `available_slots`, which returns the *last* maximum, so every capacity retry in one discovery
/// window went to the same pod — full after the first two at 2 slots per pod. Observed in
/// production as one pod taking ~85 second attempts in five minutes while its neighbours had room.
///
/// `free_slots` is evaluated once per candidate: the in-flight count moves under concurrent
/// requests, and the tie has to be decided on one consistent reading.
fn pick_most_free(
    candidates: Vec<&WorkerEndpoint>,
    free_slots: impl Fn(&WorkerEndpoint) -> usize,
    rotation: u64,
) -> Option<(&WorkerEndpoint, usize)> {
    let ranked: Vec<(&WorkerEndpoint, usize)> =
        candidates.into_iter().map(|w| (w, free_slots(w))).collect();
    let best_slots = ranked.iter().map(|(_, slots)| *slots).max()?;

    // Rotate between the workers that tie on free capacity instead of always taking the same one.
    //
    // `max_by_key` returns the *last* maximum, and every pod of an idle scope reports the same
    // `available_slots` — so under a client that sends one request at a time (the steady state for
    // many scopes) one pod served all the traffic while its replicas idled. That is invisible on
    // every dashboard: the scope looks healthy and has spare capacity. It also quietly cancels
    // replica count as a way to spread exit IPs, since a proxy identity is per worker context.
    //
    // Round-robin rather than a random pick: it needs no RNG dependency, and it is exactly the
    // sequential case that random choice serves worst (over n pods a random pick still lands on
    // the same one 1/n of the time in a row). Candidates are ordered by pod name first, because
    // discovery rebuilds its vector every 10 s and its order is not stable — round-robin over an
    // unstable order is not round-robin. Selection stays a pure function of its inputs plus the
    // caller's counter, so it remains testable.
    let mut tied: Vec<(&WorkerEndpoint, usize)> = ranked
        .into_iter()
        .filter(|(_, slots)| *slots == best_slots)
        .collect();
    tied.sort_by(|a, b| a.0.pod_name.cmp(&b.0.pod_name));

    let index = (rotation % tied.len() as u64) as usize;
    tied.get(index).copied()
}

/// Fetch fresh stats from a worker to verify slot availability
/// Returns None if the call fails (timeout, connection error, etc.)
///
/// Awaited inside the request handler, so its log lines inherit that request's span
/// (`span_ray_id`, `span_scope`, …) — no ray_id parameter is needed for correlation.
async fn fetch_fresh_worker_stats(endpoint: &str) -> Option<usize> {
    let connect_result = tokio::time::timeout(
        FRESH_STATS_TIMEOUT,
        WorkerServiceClient::connect(endpoint.to_string()),
    )
    .await;

    let mut client = match connect_result {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            debug!("Fresh stats: connection failed: {}", e);
            return None;
        }
        Err(_) => {
            debug!("Fresh stats: connection timeout");
            return None;
        }
    };

    let stats_result = tokio::time::timeout(
        FRESH_STATS_TIMEOUT,
        client.get_stats(tonic::Request::new(())),
    )
    .await;

    match stats_result {
        Ok(Ok(response)) => {
            let stats = response.into_inner();
            debug!(
                "Fresh stats from worker: available_slots={}",
                stats.available_slots
            );
            Some(stats.available_slots as usize)
        }
        Ok(Err(e)) => {
            debug!("Fresh stats: gRPC error: {}", e);
            None
        }
        Err(_) => {
            debug!("Fresh stats: request timeout");
            None
        }
    }
}

/// RAII guard that automatically decrements active request counter on drop
struct ActiveRequestGuard {
    counter: Arc<AtomicUsize>,
}

impl ActiveRequestGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct CoordinatorService {
    _config: CoordinatorConfig,
    worker_discovery: WorkerDiscoveryImpl,
    session_manager: Arc<SessionManager>,
    active_requests: Arc<AtomicUsize>,
    cancellation_token: tokio_cancellation_ext::CancellationToken,
    healthy_workers: Arc<RwLock<HashSet<String>>>,
    /// `None` when `COORDINATOR_ENABLE_METRICS` is off.
    metrics: Option<CoordinatorMetrics>,
    /// Advances once per routing decision and breaks ties in [`select_best_worker`]. Shared by
    /// every scope on purpose: it is a rotation, not a per-scope cursor, and the alternative
    /// (a counter per scope) would need a map, a lock, and eviction to say the same thing.
    routing_rotation: Arc<AtomicU64>,
    /// Requests sent to each worker and not yet answered; corrects the discovery cache for
    /// routing. Exact only while there is a single coordinator — see `in_flight.rs`.
    in_flight: Arc<InFlightTracker>,
}

impl CoordinatorService {
    pub async fn new(
        config: CoordinatorConfig,
        cancellation_token: tokio_cancellation_ext::CancellationToken,
    ) -> Result<Self> {
        info!("Initializing CoordinatorService");

        // Check if running in local mode
        let is_local_mode = std::env::var("COORDINATOR_MODE")
            .map(|m| m == "local")
            .unwrap_or(false);

        let in_flight = Arc::new(InFlightTracker::default());

        let worker_discovery = if is_local_mode {
            info!("Running in LOCAL mode - using hardcoded worker endpoint");
            let local_discovery = LocalWorkerDiscovery::new().await?;
            local_discovery.start_discovery().await;
            WorkerDiscoveryImpl::Local(local_discovery)
        } else {
            info!("Running in KUBERNETES mode - using K8s API for worker discovery");
            let k8s_discovery = WorkerDiscovery::new(in_flight.clone()).await?;
            k8s_discovery.start_discovery().await;
            WorkerDiscoveryImpl::Kubernetes(k8s_discovery)
        };

        let session_manager = Arc::new(SessionManager::default());
        let healthy_workers = Arc::new(RwLock::new(HashSet::new()));

        let metrics = if config.enable_metrics {
            Some(CoordinatorMetrics::new()?)
        } else {
            info!("Coordinator metrics are disabled (COORDINATOR_ENABLE_METRICS=false)");
            None
        };

        let service = Self {
            _config: config,
            worker_discovery,
            session_manager,
            active_requests: Arc::new(AtomicUsize::new(0)),
            cancellation_token: cancellation_token.clone(),
            healthy_workers: healthy_workers.clone(),
            metrics,
            routing_rotation: Arc::new(AtomicU64::new(0)),
            in_flight,
        };

        // Start health monitoring background task
        service.start_health_monitor();

        Ok(service)
    }

    /// Get the active requests counter for shutdown monitoring
    pub fn active_requests(&self) -> Arc<AtomicUsize> {
        self.active_requests.clone()
    }

    /// The metrics registry, or `None` when metrics are disabled.
    pub fn metrics(&self) -> Option<CoordinatorMetrics> {
        self.metrics.clone()
    }

    /// Discovery cache, health set and in-flight counts, for the metrics server's scrape-time
    /// gauge refresh.
    pub fn fleet_view(&self) -> FleetView {
        FleetView {
            workers: self.worker_discovery.get_workers(),
            healthy: self.healthy_workers.clone(),
            in_flight: self.in_flight.clone(),
        }
    }

    /// Start background task that monitors worker health every 1 second
    fn start_health_monitor(&self) {
        let worker_discovery = match &self.worker_discovery {
            WorkerDiscoveryImpl::Kubernetes(d) => d.get_workers(),
            WorkerDiscoveryImpl::Local(d) => d.get_workers(),
        };
        let healthy_workers = self.healthy_workers.clone();
        let cancellation_token = self.cancellation_token.clone();

        // Whether to log connect/health-check warnings for terminating pods.
        // Terminating pods (deletionTimestamp set) routinely fail to answer during
        // graceful shutdown / rollout churn; those failures are expected and not
        // actionable. The `is_terminating` flag is carried on each WorkerEndpoint by
        // the discovery loop, so this adds no extra K8s API calls. Shares the env var
        // with worker discovery. Read once at startup (env vars don't change at runtime).
        let log_terminating_pod_warnings =
            std::env::var("COORDINATOR_ENABLE_TERMINATING_POD_WARNINGS")
                .map(|v| v == "true")
                .unwrap_or(false);

        // Background task, so no request span reaches it: it opens its own with a sentinel
        // ray_id, keeping every line filterable by `span_ray_id` / `span_name`. No `scope`
        // field — this loop walks the workers of every scope.
        let span = tracing::info_span!("health_monitor", ray_id = "health-monitor");

        let monitor = async move {
            info!("Starting health monitor task");
            let mut interval = tokio::time::interval(Duration::from_secs(1));

            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Health monitor task stopping due to cancellation");
                        break;
                    }
                    _ = interval.tick() => {
                        // Get all workers
                        let workers_guard = worker_discovery.read().await;
                        let workers = workers_guard.clone();
                        drop(workers_guard);

                        let mut healthy_set = HashSet::new();

                        // Check health of each worker
                        for (_scope_name, workers_list) in workers.iter() {
                            for worker in workers_list {
                                let endpoint = format!("http://{}:{}", worker.pod_ip, worker.port);
                                let worker_id = worker.pod_name.clone();

                                // Try to connect and check health
                                match WorkerServiceClient::connect(endpoint.clone()).await {
                                    Ok(mut client) => {
                                        match client.health_check(Request::new(())).await {
                                            Ok(response) => {
                                                let health = response.into_inner();
                                                if health.healthy {
                                                    healthy_set.insert(worker_id.clone());
                                                } else {
                                                    warn!("Worker {} reported unhealthy: {}", worker_id, health.message);
                                                }
                                            }
                                            Err(e) => {
                                                // Suppress expected churn noise for terminating pods
                                                // unless explicitly enabled for debugging.
                                                if worker.is_terminating && !log_terminating_pod_warnings {
                                                    debug!("Health check failed for terminating worker {}: {}", worker_id, e);
                                                } else {
                                                    warn!("Health check failed for worker {}: {}", worker_id, e);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // Suppress expected churn noise for terminating pods
                                        // unless explicitly enabled for debugging.
                                        if worker.is_terminating && !log_terminating_pod_warnings {
                                            debug!("Failed to connect to terminating worker {} at {}: {}", worker_id, endpoint, e);
                                        } else {
                                            warn!("Failed to connect to worker {} at {}: {}", worker_id, endpoint, e);
                                        }
                                    }
                                }
                            }
                        }

                        // Update the healthy workers set
                        let mut healthy_guard = healthy_workers.write().await;
                        *healthy_guard = healthy_set;
                    }
                }
            }

            info!("Health monitor task stopped");
        };

        tokio::spawn(monitor.instrument(span));
    }
}

#[tonic::async_trait]
impl ScraperCoordinator for CoordinatorService {
    async fn scrape_page(
        &self,
        request: Request<ScrapePageRequest>,
    ) -> Result<Response<ScrapePageResponse>, Status> {
        let req = request.into_inner();

        // Generate or use provided ray_id for request tracing (as early as possible)
        let ray_id = if req.ray_id.is_empty() {
            format!("ray_{}", uuid::Uuid::new_v4())
        } else {
            req.ray_id.clone()
        };

        // Per-request span, mirroring the worker's: every log line emitted while routing this
        // request inherits the context, so in JSON logs it lands under the `span` object (Loki:
        // span_ray_id, span_scope, …) and the same query works across coordinator and worker.
        // `scope` matters more here than in the worker — the coordinator serves every scope.
        // `session_id` is recorded only when the client sent one; `worker_id` starts Empty and
        // is recorded once routing has picked a worker (and re-recorded on a retry, so the field
        // always names the worker that produced the response).
        let span = tracing::info_span!(
            "scrape_page",
            scope = %req.scope_name,
            ray_id = %ray_id,
            url = %req.url,
            session_id = tracing::field::Empty,
            worker_id = tracing::field::Empty,
        );
        if !req.session_id.is_empty() {
            span.record("session_id", req.session_id.as_str());
        }

        async move {
        // Track active request (automatically decrements on drop)
        let _active_guard = ActiveRequestGuard::new(self.active_requests.clone());

        // Counts the request and records its duration on drop, so a future dropped mid-request
        // (client disconnect, gRPC deadline) is still counted. `reject()` marks the reason.
        let mut request_metrics = RequestMetrics::new(self.metrics.clone(), &req.scope_name);

        let start_time = std::time::Instant::now();

        // Check if coordinator is terminating - return immediately
        if self.cancellation_token.is_cancelled() {
            request_metrics.reject(RejectReason::Terminating);
            let execution_time_ms = start_time.elapsed().as_millis() as u64;
            return Ok(Response::new(ScrapePageResponse {
                success: false,
                status_code: 0,
                content: String::new(),
                error_message: "Coordinator is shutting down, please retry".to_string(),
                error_code: browser_hive_proto::coordinator::ErrorCode::Terminating as i32,
                response_headers: std::collections::HashMap::new(),
                session_id: String::new(),
                worker_id: String::new(),
                context_id: String::new(),
                execution_time_ms,
                ray_id: ray_id.clone(),
            }));
        }

        debug!(
            "Received scraping request for scope: {}, URL: {}, wait_timeout_ms: {}, wait_selector: {:?}, skip_selector: {:?}",
            req.scope_name, req.url, req.wait_timeout_ms,
            if req.wait_selector.is_empty() { None } else { Some(&req.wait_selector) },
            if req.skip_selector.is_empty() { None } else { Some(&req.skip_selector) }
        );

        let workers_guard = self.worker_discovery.get_workers();
        let workers = workers_guard.read().await;

        // If session_id is provided - use the stored worker
        let (worker_endpoint, worker_id) = if !req.session_id.is_empty() {
            debug!("Using existing session: {}", req.session_id);

            if let Some(session_info) = self.session_manager.get_session(&req.session_id).await {
                // Routing follows the session's scope, not the request's — keep the span field
                // and the metric label pointing at the scope the request is actually served from.
                tracing::Span::current().record("scope", session_info.scope_name.as_str());
                request_metrics.set_scope(&session_info.scope_name);

                // Check that worker still exists
                let scope_workers = workers.get(&session_info.scope_name);
                let worker_exists = scope_workers
                    .map(|ws| {
                        ws.iter()
                            .any(|w| w.pod_name == session_info.session_id.worker_id)
                    })
                    .unwrap_or(false);

                if worker_exists {
                    (
                        session_info.worker_endpoint.clone(),
                        session_info.session_id.worker_id.clone(),
                    )
                } else {
                    // Worker unavailable - remove session and select a new one
                    warn!("Session worker not available, creating new session");
                    self.session_manager.remove_session(&req.session_id).await;

                    request_metrics.reject(RejectReason::SessionNotFound);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message:
                            "Session expired or worker unavailable. Please retry without session_id"
                                .to_string(),
                        error_code: browser_hive_proto::coordinator::ErrorCode::SessionNotFound
                            as i32,
                        response_headers: std::collections::HashMap::new(),
                        session_id: String::new(),
                        worker_id: String::new(),
                        context_id: String::new(),
                        execution_time_ms,
                        ray_id: ray_id.clone(),
                    }));
                }
            } else {
                request_metrics.reject(RejectReason::SessionNotFound);
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(Response::new(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: "Session not found or expired".to_string(),
                    error_code: browser_hive_proto::coordinator::ErrorCode::SessionNotFound as i32,
                    response_headers: std::collections::HashMap::new(),
                    session_id: String::new(),
                    worker_id: String::new(),
                    context_id: String::new(),
                    execution_time_ms,
                    ray_id: ray_id.clone(),
                }));
            }
        } else {
            // New request - select the best worker
            let scope_workers = match workers.get(&req.scope_name) {
                Some(w) => w,
                None => {
                    // No routable worker — which is either a wrong scope name or a scope whose
                    // pods are all restarting. `classify_missing_scope` decides which, and why
                    // the two must not share an error code.
                    let presence = self
                        .worker_discovery
                        .get_known_scopes()
                        .read()
                        .await
                        .get(&req.scope_name)
                        .copied();

                    let (reason, error_code, error_message) =
                        classify_missing_scope(&req.scope_name, presence);

                    if reason == RejectReason::ScopeUnavailable {
                        warn!("{}", error_message);
                    } else {
                        warn!("Scope not found: {}", req.scope_name);
                    }
                    request_metrics.reject(reason);

                    let execution_time_ms = start_time.elapsed().as_millis() as u64;

                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message,
                        error_code,
                        response_headers: std::collections::HashMap::new(),
                        session_id: String::new(),
                        worker_id: String::new(),
                        context_id: String::new(),
                        execution_time_ms,
                        ray_id: ray_id.clone(),
                    }));
                }
            };

            if scope_workers.is_empty() {
                request_metrics.reject(RejectReason::NoWorkers);
                let execution_time_ms = start_time.elapsed().as_millis() as u64;
                return Ok(Response::new(ScrapePageResponse {
                    success: false,
                    status_code: 0,
                    content: String::new(),
                    error_message: format!("No workers available for scope: {}", req.scope_name),
                    error_code: browser_hive_proto::coordinator::ErrorCode::NoWorkersAvailable
                        as i32,
                    response_headers: std::collections::HashMap::new(),
                    session_id: String::new(),
                    worker_id: String::new(),
                    context_id: String::new(),
                    execution_time_ms,
                    ray_id: ray_id.clone(),
                }));
            }

            // Select best worker using routing logic
            let healthy_guard = self.healthy_workers.read().await;
            let rotation = self.routing_rotation.fetch_add(1, Ordering::Relaxed);
            let selected = select_best_worker(
                scope_workers,
                &healthy_guard,
                |w| self.in_flight.free_slots(w),
                rotation,
            );
            drop(healthy_guard);

            let selected = match selected {
                Some(s) => {
                    if s.used_fallback {
                        warn!("No healthy workers in health cache, using all discovered workers");
                    }
                    s
                }
                None => {
                    request_metrics.reject(RejectReason::NoWorkers);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!(
                            "No available workers for scope: {}",
                            req.scope_name
                        ),
                        error_code: browser_hive_proto::coordinator::ErrorCode::NoWorkersAvailable
                            as i32,
                        response_headers: std::collections::HashMap::new(),
                        session_id: String::new(),
                        worker_id: String::new(),
                        context_id: String::new(),
                        execution_time_ms,
                        ray_id: ray_id.clone(),
                    }));
                }
            };

            let best_worker = selected.worker;
            let endpoint = format!("http://{}:{}", best_worker.pod_ip, best_worker.port);

            // No free slot as far as the coordinator can tell. The cache under that estimate may
            // still be stale (a `dedicated` session released its slot), so ask the worker itself
            // before rejecting the request.
            if selected.free_slots == 0 {
                debug!(
                    "No free slots on worker {} (cache corrected by in-flight requests), fetching fresh stats",
                    best_worker.pod_name
                );

                let fresh_slots = fetch_fresh_worker_stats(&endpoint).await;

                match fresh_slots {
                    Some(slots) if slots > 0 => {
                        // Cache was stale - worker actually has slots available
                        info!(
                            "Fresh stats show {} available slots (cache was stale), proceeding with request",
                            slots
                        );
                        // Continue with the request
                    }
                    _ => {
                        // Fresh stats confirm no slots, or fetch failed - reject.
                        // This is the capacity signal: demand that existed and was refused.
                        request_metrics.reject(RejectReason::NoSlots);
                        let execution_time_ms = start_time.elapsed().as_millis() as u64;
                        return Ok(Response::new(ScrapePageResponse {
                            success: false,
                            status_code: 0,
                            content: String::new(),
                            error_message: format!(
                                "No available slots in scope: {}. All workers are busy.",
                                req.scope_name
                            ),
                            error_code:
                                browser_hive_proto::coordinator::ErrorCode::NoWorkersAvailable
                                    as i32,
                            response_headers: std::collections::HashMap::new(),
                            session_id: String::new(),
                            worker_id: String::new(),
                            context_id: String::new(),
                            execution_time_ms,
                            ray_id: ray_id.clone(),
                        }));
                    }
                }
            }

            (endpoint, best_worker.pod_name.clone())
        };

        drop(workers); // Release lock

        // Routing is decided: add the target worker to the span so every later line names it.
        tracing::Span::current().record("worker_id", worker_id.as_str());

        // Calculate request deadline
        let request_deadline = start_time + Duration::from_secs(req.timeout_seconds as u64);
        let min_retry_time_remaining = Duration::from_secs(10);

        let mut excluded_workers = HashSet::new();
        let mut attempt = 0;
        let mut last_worker_id = worker_id.clone();
        let mut last_worker_endpoint = worker_endpoint.clone();

        // Retry loop for worker answers that another pod could still serve (see `retry_plan`)
        let mut worker_response = loop {
            attempt += 1;

            // This attempt occupies a slot on `last_worker_id` until the iteration ends — by
            // `break`, `return`, or the future being dropped. Counted from before the connect, so
            // concurrent routing decisions already see the slot as taken. A request continuing a
            // session is not counted: its slot was claimed when the session was created and is
            // already missing from the worker's `available_slots`.
            let _in_flight = req
                .session_id
                .is_empty()
                .then(|| self.in_flight.start(&last_worker_id));

            // Connect to worker
            let mut client = match WorkerServiceClient::connect(last_worker_endpoint.clone()).await
            {
                Ok(client) => client.max_decoding_message_size(MAX_WORKER_RESPONSE_SIZE),
                Err(e) => {
                    // Unreachable worker is an operational error, not an infrastructure one from
                    // the client's point of view: the coordinator answered, so the answer must be
                    // a parseable response carrying `ray_id` and `execution_time_ms` like every
                    // other error. Returning `Status::internal` here (as this did) forced clients
                    // to read a free-text gRPC message and lost the tracing id with it.
                    warn!("Failed to connect to worker {}: {}", last_worker_id, e);
                    request_metrics.reject(RejectReason::WorkerUnreachable);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!(
                            "Failed to connect to worker {}: {}",
                            last_worker_id, e
                        ),
                        error_code: browser_hive_proto::coordinator::ErrorCode::WorkerUnreachable
                            as i32,
                        response_headers: std::collections::HashMap::new(),
                        session_id: String::new(),
                        worker_id: last_worker_id.clone(),
                        context_id: String::new(),
                        execution_time_ms,
                        ray_id: ray_id.clone(),
                    }));
                }
            };

            // Parse session_id to get context_id
            let context_id = if !req.session_id.is_empty() {
                SessionId::from_string(&req.session_id)
                    .map(|sid| sid.context_id)
                    .unwrap_or_default()
            } else {
                String::new()
            };

            // Calculate remaining time for this attempt
            let now = std::time::Instant::now();
            let remaining_time = request_deadline.saturating_duration_since(now);
            let timeout_seconds = remaining_time.as_secs().max(1) as u32; // At least 1 second

            let worker_request = browser_hive_proto::worker::ScrapePageRequest {
                url: req.url.clone(),
                timeout_seconds,
                context_id: context_id.clone(),
                wait_strategy: req.wait_strategy.clone(),
                wait_timeout_ms: req.wait_timeout_ms,
                wait_selector: req.wait_selector.clone(),
                skip_selector: req.skip_selector.clone(),
                ray_id: ray_id.clone(),
                country_code: req.country_code.clone(),
            };

            debug!(
                "Attempt {}: Forwarding to worker {} (timeout: {}s, wait_timeout_ms={}, wait_selector={:?}, skip_selector={:?})",
                attempt,
                last_worker_id,
                timeout_seconds,
                worker_request.wait_timeout_ms,
                if worker_request.wait_selector.is_empty() { None } else { Some(&worker_request.wait_selector) },
                if worker_request.skip_selector.is_empty() { None } else { Some(&worker_request.skip_selector) }
            );

            // Set timeout for the gRPC request
            let mut request = Request::new(worker_request);
            request.set_timeout(GRPC_CLIENT_TIMEOUT);

            let response = match client.scrape_page(request).await {
                Ok(resp) => resp,
                // The RPC itself failed: the worker died mid-request, or the connection broke.
                // `InvalidArgument` is the exception — it is the worker rejecting the *request*
                // (an unknown `wait_strategy`, a `wait_timeout_ms` over the maximum), which is a
                // defect in the call and must reach the client as such instead of being dressed
                // up as an infrastructure problem it could retry forever.
                Err(e) if e.code() == tonic::Code::InvalidArgument => return Err(e),
                Err(e) => {
                    warn!("Worker {} RPC failed: {}", last_worker_id, e);
                    request_metrics.reject(RejectReason::WorkerUnreachable);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!(
                            "Worker {} became unreachable during the request: {}",
                            last_worker_id, e
                        ),
                        error_code: browser_hive_proto::coordinator::ErrorCode::WorkerUnreachable
                            as i32,
                        response_headers: std::collections::HashMap::new(),
                        session_id: String::new(),
                        worker_id: last_worker_id.clone(),
                        context_id: String::new(),
                        execution_time_ms,
                        ray_id: ray_id.clone(),
                    }));
                }
            };

            let worker_resp = response.into_inner();

            // Is this an answer another pod could still serve?
            let Some(plan) = classify_retry(worker_resp.error_code, req.session_id.is_empty())
            else {
                // Success, or an error no other worker would answer differently
                break worker_resp;
            };
            let RetryPlan {
                reason: retry_reason,
                max_attempts,
                code_name,
            } = plan;

            warn!(
                "Worker {} returned {} (attempt {})",
                last_worker_id, code_name, attempt
            );

            excluded_workers.insert(last_worker_id.clone());

            // Check if we have time and attempts left
            let now = std::time::Instant::now();
            let remaining_time = request_deadline.saturating_duration_since(now);

            if remaining_time < min_retry_time_remaining {
                warn!(
                    "Not enough time remaining for retry ({:?} < {:?}), returning {} to client",
                    remaining_time, min_retry_time_remaining, code_name
                );
                break worker_resp;
            }

            if attempt >= max_attempts {
                warn!(
                    "Max retry attempts ({}) reached, returning {} to client",
                    max_attempts, code_name
                );
                break worker_resp;
            }

            // Try to find another worker
            debug!("Attempting to retry on another worker (remaining time: {:?})", remaining_time);

            let workers_guard = self.worker_discovery.get_workers();
            let workers = workers_guard.read().await;

            let scope_workers = match workers.get(&req.scope_name) {
                Some(w) => w,
                None => {
                    warn!("Scope {} not found during retry", req.scope_name);
                    break worker_resp;
                }
            };

            // Filter by healthy workers and exclude failed workers
            let healthy_guard = self.healthy_workers.read().await;
            let candidates: Vec<_> = scope_workers
                .iter()
                .filter(|w| {
                    healthy_guard.contains(&w.pod_name) && !excluded_workers.contains(&w.pod_name)
                })
                .collect();
            drop(healthy_guard);

            // Ranked exactly like the first routing decision (see `pick_most_free`).
            let rotation = self.routing_rotation.fetch_add(1, Ordering::Relaxed);
            let Some((best_worker, free_slots)) =
                pick_most_free(candidates, |w| self.in_flight.free_slots(w), rotation)
            else {
                warn!("No healthy workers available for retry");
                break worker_resp;
            };

            if free_slots == 0 {
                warn!("No available slots for retry");
                break worker_resp;
            }

            last_worker_id = best_worker.pod_name.clone();
            last_worker_endpoint = format!("http://{}:{}", best_worker.pod_ip, best_worker.port);
            tracing::Span::current().record("worker_id", last_worker_id.as_str());

            drop(workers);

            // A retry that succeeds leaves no other trace: the client sees a normal response and
            // nothing was rejected. This counter is the only place a scope that survives on
            // retries is visible.
            request_metrics.record_retry(retry_reason);

            debug!("Retrying on worker: {}", last_worker_id);
        };

        // A worker that ran out of slots is answering about **capacity**, and capacity is the
        // coordinator's own vocabulary: the client learns no new code, it gets the 5001 it already
        // treats as retryable, and the refusal is counted where an operator looks for "add
        // replicas". The worker's own wording is kept — it names the session mode's limit, which
        // is what actually has to change.
        if worker_response.error_code
            == browser_hive_proto::worker::ErrorCode::CapacityExhausted as i32
        {
            request_metrics.reject(RejectReason::NoSlots);
            let detail = std::mem::take(&mut worker_response.error_message);
            worker_response.error_message =
                format!("No available slots in scope {}: {}", req.scope_name, detail);
            worker_response.error_code =
                browser_hive_proto::coordinator::ErrorCode::NoWorkersAvailable as i32;
        }

        // Create or update session
        let session_id = if !worker_response.context_id.is_empty() {
            let session_id = self
                .session_manager
                .create_session(
                    req.scope_name.clone(),
                    last_worker_id.clone(),
                    worker_response.context_id.clone(),
                    last_worker_endpoint,
                )
                .await;
            session_id.to_string()
        } else {
            String::new()
        };

        // Calculate total execution time including all retries
        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        Ok(Response::new(ScrapePageResponse {
            success: worker_response.success,
            status_code: worker_response.status_code,
            content: worker_response.content,
            error_message: worker_response.error_message,
            error_code: worker_response.error_code,
            response_headers: worker_response.response_headers,
            session_id,
            worker_id: last_worker_id,
            context_id: worker_response.context_id,
            execution_time_ms,
            ray_id,
        }))
        }
        .instrument(span)
        .await
    }

    async fn get_cluster_stats(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ClusterStatsResponse>, Status> {
        let workers_guard = self.worker_discovery.get_workers();
        let workers = workers_guard.read().await;

        let mut scope_stats_map = HashMap::new();
        let mut total_requests = 0u64;
        let mut active_jobs = 0u64;

        for (scope_name, workers_list) in workers.iter() {
            let total_slots: usize = workers_list
                .iter()
                .map(|w| w.stats.total_contexts * 3)
                .sum();
            let available_slots: usize = workers_list.iter().map(|w| w.stats.available_slots).sum();
            let active_requests: usize = workers_list.iter().map(|w| w.stats.active_requests).sum();
            let scope_total_requests: u64 =
                workers_list.iter().map(|w| w.stats.total_requests).sum();

            total_requests += scope_total_requests;
            active_jobs += active_requests as u64;

            let success_rate: f64 = if workers_list.is_empty() {
                0.0
            } else {
                workers_list
                    .iter()
                    .map(|w| w.stats.success_rate)
                    .sum::<f64>()
                    / workers_list.len() as f64
            };

            scope_stats_map.insert(
                scope_name.clone(),
                ScopeStatsProto {
                    name: scope_name.clone(),
                    total_workers: workers_list.len() as u32,
                    total_slots: total_slots as u32,
                    available_slots: available_slots as u32,
                    active_requests: active_requests as u32,
                    success_rate,
                },
            );
        }

        Ok(Response::new(ClusterStatsResponse {
            scope_stats: scope_stats_map,
            total_requests,
            active_jobs,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_hive_common::WorkerStats;

    /// Only two worker answers earn a second pod. Everything else would be answered identically
    /// anywhere in the scope, so retrying spends the client's deadline for nothing.
    #[test]
    fn only_terminating_and_capacity_are_retried() {
        use browser_hive_proto::coordinator::ErrorCode as CoordCode;
        use browser_hive_proto::worker::ErrorCode as WorkerCode;

        let terminating = classify_retry(CoordCode::Terminating as i32, true).expect("retryable");
        assert_eq!(terminating.reason, RetryReason::Terminating);
        assert_eq!(terminating.max_attempts, MAX_TERMINATING_ATTEMPTS);

        let capacity =
            classify_retry(WorkerCode::CapacityExhausted as i32, true).expect("retryable");
        assert_eq!(capacity.reason, RetryReason::NoSlots);
        assert_eq!(capacity.max_attempts, MAX_CAPACITY_ATTEMPTS);

        for code in [
            0,
            CoordCode::BrowserError as i32,
            CoordCode::NetworkError as i32,
            CoordCode::ContextCreationFailed as i32,
            CoordCode::ProxyError as i32,
            CoordCode::SessionNotFound as i32,
            WorkerCode::SessionBusy as i32,
            CoordCode::SelectorNotFound as i32,
            CoordCode::TimeoutBrowser as i32,
        ] {
            assert!(
                classify_retry(code, true).is_none(),
                "error code {code} must not be retried on another worker"
            );
        }
    }

    /// Capacity gets fewer attempts than TERMINATING on purpose: retrying hard against a scope
    /// that is already at its limit is how a saturated scope becomes a load generator.
    #[test]
    fn capacity_is_retried_less_eagerly_than_terminating() {
        assert!(MAX_CAPACITY_ATTEMPTS < MAX_TERMINATING_ATTEMPTS);
    }

    /// A session lives in one context on one pod, so re-sending it elsewhere would silently hand
    /// the client a different session. TERMINATING is the exception — that pod is going away and
    /// the session with it.
    #[test]
    fn a_session_request_is_never_retried_for_capacity() {
        use browser_hive_proto::coordinator::ErrorCode as CoordCode;
        use browser_hive_proto::worker::ErrorCode as WorkerCode;

        assert!(classify_retry(WorkerCode::CapacityExhausted as i32, false).is_none());
        assert!(classify_retry(CoordCode::Terminating as i32, false).is_some());
    }

    /// Helper to create a WorkerEndpoint for testing
    fn make_worker(name: &str, available_slots: usize) -> WorkerEndpoint {
        let total_contexts = available_slots.max(3); // Ensure total >= available
        WorkerEndpoint {
            pod_name: name.to_string(),
            pod_ip: format!("10.0.0.{}", name.len()),
            port: 50052,
            scope_name: "test_scope".to_string(),
            stats: WorkerStats {
                scope_name: "test_scope".to_string(),
                pod_name: name.to_string(),
                pod_ip: format!("10.0.0.{}", name.len()),
                total_contexts,
                available_slots,
                active_requests: total_contexts.saturating_sub(available_slots),
                total_requests: 100,
                total_contexts_created: 50,
                total_contexts_recycled: 10,
                success_rate: 0.95,
            },
            in_flight_at_snapshot: 0,
            is_terminating: false,
        }
    }

    /// Free slots straight from the discovery cache, for tests about ranking rather than in-flight
    /// accounting.
    fn cached(worker: &WorkerEndpoint) -> usize {
        worker.stats.available_slots
    }

    /// The production failure: two 2-slot pods, a cache taken while both were idle, and requests
    /// arriving faster than discovery refreshes. Ranking on the cache alone kept offering pods it
    /// had already filled; with in-flight accounting each pod gets exactly its two slots and the
    /// fifth request sees a full scope instead of a pod that will answer CAPACITY_EXHAUSTED.
    #[test]
    fn dispatches_within_one_discovery_round_fill_every_pod_before_any_overflows() {
        let tracker = Arc::new(InFlightTracker::default());
        let workers = vec![make_worker("worker-1", 2), make_worker("worker-2", 2)];
        let healthy: HashSet<String> = ["worker-1", "worker-2"]
            .into_iter()
            .map(String::from)
            .collect();

        let mut held = Vec::new();
        let mut per_pod: HashMap<String, usize> = HashMap::new();
        for rotation in 0..4 {
            let selected =
                select_best_worker(&workers, &healthy, |w| tracker.free_slots(w), rotation)
                    .unwrap();
            assert!(
                selected.free_slots > 0,
                "request {rotation} found no free slot"
            );
            *per_pod.entry(selected.worker.pod_name.clone()).or_default() += 1;
            held.push(tracker.start(&selected.worker.pod_name));
        }

        assert_eq!(per_pod.get("worker-1"), Some(&2));
        assert_eq!(per_pod.get("worker-2"), Some(&2));
        let fifth = select_best_worker(&workers, &healthy, |w| tracker.free_slots(w), 4).unwrap();
        assert_eq!(fifth.free_slots, 0);

        // A finished request gives its slot back without waiting for the next discovery round.
        held.pop();
        let sixth = select_best_worker(&workers, &healthy, |w| tracker.free_slots(w), 5).unwrap();
        assert_eq!(sixth.free_slots, 1);
    }

    /// The retry path ranks through `pick_most_free` too. It used `max_by_key`, which returns the
    /// last maximum and therefore sent every retry in a discovery window to the same pod.
    #[test]
    fn retry_candidates_rotate_between_ties() {
        let workers = [
            make_worker("worker-1", 2),
            make_worker("worker-2", 2),
            make_worker("worker-3", 2),
        ];

        let picked: Vec<&str> = (0..3)
            .map(|rotation| {
                pick_most_free(workers.iter().collect(), cached, rotation)
                    .unwrap()
                    .0
                    .pod_name
                    .as_str()
            })
            .collect();

        assert_eq!(picked, vec!["worker-1", "worker-2", "worker-3"]);
    }

    // ==================== select_best_worker Tests ====================

    #[test]
    fn test_select_best_worker_empty_list() {
        let workers: Vec<WorkerEndpoint> = vec![];
        let healthy = HashSet::new();

        let result = select_best_worker(&workers, &healthy, cached, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_select_best_worker_single_worker_healthy() {
        let workers = vec![make_worker("worker-1", 5)];
        let healthy: HashSet<String> = ["worker-1".to_string()].into_iter().collect();

        let result = select_best_worker(&workers, &healthy, cached, 0).unwrap();
        assert_eq!(result.worker.pod_name, "worker-1");
        assert!(!result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_selects_highest_slots() {
        let workers = vec![
            make_worker("worker-1", 2),
            make_worker("worker-2", 5),
            make_worker("worker-3", 3),
        ];
        let healthy: HashSet<String> = ["worker-1", "worker-2", "worker-3"]
            .into_iter()
            .map(String::from)
            .collect();

        let result = select_best_worker(&workers, &healthy, cached, 0).unwrap();
        assert_eq!(result.worker.pod_name, "worker-2");
        assert_eq!(result.worker.stats.available_slots, 5);
        assert!(!result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_filters_unhealthy() {
        let workers = vec![
            make_worker("worker-1", 10), // unhealthy, highest slots
            make_worker("worker-2", 3),  // healthy
            make_worker("worker-3", 5),  // healthy, should be selected
        ];
        // Only worker-2 and worker-3 are healthy
        let healthy: HashSet<String> = ["worker-2", "worker-3"]
            .into_iter()
            .map(String::from)
            .collect();

        let result = select_best_worker(&workers, &healthy, cached, 0).unwrap();
        assert_eq!(result.worker.pod_name, "worker-3");
        assert_eq!(result.worker.stats.available_slots, 5);
        assert!(!result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_fallback_when_no_healthy() {
        let workers = vec![make_worker("worker-1", 2), make_worker("worker-2", 5)];
        // Empty healthy set - should fall back to all workers
        let healthy: HashSet<String> = HashSet::new();

        let result = select_best_worker(&workers, &healthy, cached, 0).unwrap();
        assert_eq!(result.worker.pod_name, "worker-2");
        assert!(result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_all_zero_slots() {
        let workers = vec![make_worker("worker-1", 0), make_worker("worker-2", 0)];
        let healthy: HashSet<String> = ["worker-1", "worker-2"]
            .into_iter()
            .map(String::from)
            .collect();

        // Should still select a worker (any of them)
        let result = select_best_worker(&workers, &healthy, cached, 0).unwrap();
        assert_eq!(result.worker.stats.available_slots, 0);
        assert!(!result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_partial_healthy() {
        let workers = vec![
            make_worker("worker-1", 1),
            make_worker("worker-2", 2),
            make_worker("worker-3", 3),
        ];
        // Only worker-1 is healthy
        let healthy: HashSet<String> = ["worker-1"].into_iter().map(String::from).collect();

        let result = select_best_worker(&workers, &healthy, cached, 0).unwrap();
        assert_eq!(result.worker.pod_name, "worker-1");
        assert_eq!(result.worker.stats.available_slots, 1);
        assert!(!result.used_fallback);
    }

    /// Workers that tie on free capacity are used in turn. This is the steady state of a scope
    /// driven one request at a time: every pod is idle and reports the same `available_slots`, and
    /// before the rotation the same pod took all of the traffic while its replicas sat idle.
    #[test]
    fn test_select_best_worker_rotates_between_ties() {
        let workers = vec![
            make_worker("worker-1", 5),
            make_worker("worker-2", 5),
            make_worker("worker-3", 5),
        ];
        let healthy: HashSet<String> = ["worker-1", "worker-2", "worker-3"]
            .into_iter()
            .map(String::from)
            .collect();

        let picked: Vec<String> = (0..6)
            .map(|rotation| {
                select_best_worker(&workers, &healthy, cached, rotation)
                    .unwrap()
                    .worker
                    .pod_name
                    .clone()
            })
            .collect();

        assert_eq!(
            picked,
            vec!["worker-1", "worker-2", "worker-3", "worker-1", "worker-2", "worker-3"]
        );
    }

    /// The rotation must never cost capacity: a worker with fewer free slots is not part of the
    /// tie, however the counter happens to fall.
    #[test]
    fn test_rotation_never_picks_a_worker_with_fewer_slots() {
        let workers = vec![
            make_worker("worker-1", 5),
            make_worker("worker-2", 1),
            make_worker("worker-3", 5),
        ];
        let healthy: HashSet<String> = ["worker-1", "worker-2", "worker-3"]
            .into_iter()
            .map(String::from)
            .collect();

        for rotation in 0..6 {
            let selected = select_best_worker(&workers, &healthy, cached, rotation).unwrap();
            assert_eq!(selected.worker.stats.available_slots, 5);
            assert_ne!(selected.worker.pod_name, "worker-2");
        }
    }

    /// Discovery rebuilds its worker vector on every round and does not promise an order, so the
    /// rotation is taken over a stable one — otherwise the same counter would revisit the same pod.
    #[test]
    fn test_rotation_is_independent_of_discovery_order() {
        let healthy: HashSet<String> = ["worker-1", "worker-2"]
            .into_iter()
            .map(String::from)
            .collect();

        let one_order = vec![make_worker("worker-1", 5), make_worker("worker-2", 5)];
        let other_order = vec![make_worker("worker-2", 5), make_worker("worker-1", 5)];

        for rotation in 0..4 {
            assert_eq!(
                select_best_worker(&one_order, &healthy, cached, rotation)
                    .unwrap()
                    .worker
                    .pod_name,
                select_best_worker(&other_order, &healthy, cached, rotation)
                    .unwrap()
                    .worker
                    .pod_name
            );
        }
    }

    /// The whole point of the presence map: a scope whose pods exist but cannot yet serve must
    /// be answered with a retryable code, never with "no such scope".
    #[test]
    fn known_scope_with_no_reachable_pod_is_retryable() {
        let presence = ScopePresence {
            pods_total: 3,
            pods_reachable: 0,
            pods_unreachable: 3,
            ..Default::default()
        };

        let (reason, code, message) = classify_missing_scope("scope_a", Some(presence));

        assert_eq!(reason, RejectReason::ScopeUnavailable);
        assert_eq!(
            code,
            browser_hive_proto::coordinator::ErrorCode::NoWorkersAvailable as i32
        );
        // The client cannot query K8s; the pod breakdown is how it learns waiting will help.
        assert!(message.contains("3 pod(s), 0 reachable"), "{message}");
        assert!(message.contains("retry shortly"), "{message}");
    }

    /// A name no pod carries stays a configuration error — retrying it never helps.
    #[test]
    fn unknown_scope_stays_scope_not_found() {
        let (reason, code, message) = classify_missing_scope("typo-from-client", None);

        assert_eq!(reason, RejectReason::ScopeNotFound);
        assert_eq!(
            code,
            browser_hive_proto::coordinator::ErrorCode::ScopeNotFound as i32
        );
        assert!(message.contains("typo-from-client"), "{message}");
    }
}
