use crate::local_worker_discovery::LocalWorkerDiscovery;
use crate::worker_discovery::WorkerDiscovery;
use anyhow::Result;
use browser_hive_common::{CoordinatorConfig, SessionId, SessionManager, WorkerEndpoint};
use browser_hive_proto::coordinator::{
    scraper_coordinator_server::ScraperCoordinator, ClusterStatsResponse, ScopeStatsProto,
    ScrapePageRequest, ScrapePageResponse,
};
use browser_hive_proto::worker::worker_service_client::WorkerServiceClient;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

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
}

// gRPC client timeout when calling workers
const GRPC_CLIENT_TIMEOUT: Duration = Duration::from_secs(330); // 10 seconds more than server timeout

// Timeout for fetching fresh stats from worker (used to avoid stale cache rejections)
const FRESH_STATS_TIMEOUT: Duration = Duration::from_secs(2);

/// Result of worker selection
#[derive(Debug, Clone)]
pub struct SelectedWorker<'a> {
    pub worker: &'a WorkerEndpoint,
    pub used_fallback: bool, // true if no healthy workers were found and we fell back to all workers
}

/// Select the best worker from a list of scope workers based on available slots
///
/// # Algorithm
/// 1. Filter workers by healthy status (must be in `healthy_workers` set)
/// 2. If no healthy workers, fall back to all workers (race condition window)
/// 3. Select worker with the highest `available_slots`
///
/// # Returns
/// - `Some(SelectedWorker)` with the best worker and whether fallback was used
/// - `None` if no workers available
pub fn select_best_worker<'a>(
    scope_workers: &'a [WorkerEndpoint],
    healthy_workers: &HashSet<String>,
) -> Option<SelectedWorker<'a>> {
    if scope_workers.is_empty() {
        return None;
    }

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

    candidates
        .into_iter()
        .max_by_key(|w| w.stats.available_slots)
        .map(|worker| SelectedWorker {
            worker,
            used_fallback,
        })
}

/// Fetch fresh stats from a worker to verify slot availability
/// Returns None if the call fails (timeout, connection error, etc.)
async fn fetch_fresh_worker_stats(endpoint: &str, ray_id: &str) -> Option<usize> {
    let connect_result = tokio::time::timeout(
        FRESH_STATS_TIMEOUT,
        WorkerServiceClient::connect(endpoint.to_string()),
    )
    .await;

    let mut client = match connect_result {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            debug!(ray_id = %ray_id, "Fresh stats: connection failed: {}", e);
            return None;
        }
        Err(_) => {
            debug!(ray_id = %ray_id, "Fresh stats: connection timeout");
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
                ray_id = %ray_id,
                "Fresh stats from worker: available_slots={}",
                stats.available_slots
            );
            Some(stats.available_slots as usize)
        }
        Ok(Err(e)) => {
            debug!(ray_id = %ray_id, "Fresh stats: gRPC error: {}", e);
            None
        }
        Err(_) => {
            debug!(ray_id = %ray_id, "Fresh stats: request timeout");
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

        let worker_discovery = if is_local_mode {
            info!("Running in LOCAL mode - using hardcoded worker endpoint");
            let local_discovery = LocalWorkerDiscovery::new().await?;
            local_discovery.start_discovery().await;
            WorkerDiscoveryImpl::Local(local_discovery)
        } else {
            info!("Running in KUBERNETES mode - using K8s API for worker discovery");
            let k8s_discovery = WorkerDiscovery::new().await?;
            k8s_discovery.start_discovery().await;
            WorkerDiscoveryImpl::Kubernetes(k8s_discovery)
        };

        let session_manager = Arc::new(SessionManager::default());
        let healthy_workers = Arc::new(RwLock::new(HashSet::new()));

        let service = Self {
            _config: config,
            worker_discovery,
            session_manager,
            active_requests: Arc::new(AtomicUsize::new(0)),
            cancellation_token: cancellation_token.clone(),
            healthy_workers: healthy_workers.clone(),
        };

        // Start health monitoring background task
        service.start_health_monitor();

        Ok(service)
    }

    /// Get the active requests counter for shutdown monitoring
    pub fn active_requests(&self) -> Arc<AtomicUsize> {
        self.active_requests.clone()
    }

    /// Start background task that monitors worker health every 1 second
    fn start_health_monitor(&self) {
        let worker_discovery = match &self.worker_discovery {
            WorkerDiscoveryImpl::Kubernetes(d) => d.get_workers(),
            WorkerDiscoveryImpl::Local(d) => d.get_workers(),
        };
        let healthy_workers = self.healthy_workers.clone();
        let cancellation_token = self.cancellation_token.clone();

        tokio::spawn(async move {
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
                                                warn!("Health check failed for worker {}: {}", worker_id, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to connect to worker {} at {}: {}", worker_id, endpoint, e);
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
        });
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

        // Track active request (automatically decrements on drop)
        let _active_guard = ActiveRequestGuard::new(self.active_requests.clone());

        let start_time = std::time::Instant::now();

        // Check if coordinator is terminating - return immediately
        if self.cancellation_token.is_cancelled() {
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
            ray_id = %ray_id,
            "Received scraping request for scope: {}, URL: {}, wait_timeout_ms: {}, wait_selector: {:?}, skip_selector: {:?}",
            req.scope_name, req.url, req.wait_timeout_ms,
            if req.wait_selector.is_empty() { None } else { Some(&req.wait_selector) },
            if req.skip_selector.is_empty() { None } else { Some(&req.skip_selector) }
        );

        let workers_guard = self.worker_discovery.get_workers();
        let workers = workers_guard.read().await;

        // If session_id is provided - use the stored worker
        let (worker_endpoint, worker_id) = if !req.session_id.is_empty() {
            debug!(ray_id = %ray_id, "Using existing session: {}", req.session_id);

            if let Some(session_info) = self.session_manager.get_session(&req.session_id).await {
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
                    warn!(ray_id = %ray_id, "Session worker not available, creating new session");
                    self.session_manager.remove_session(&req.session_id).await;

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
                    warn!(ray_id = %ray_id, "Scope not found: {}", req.scope_name);
                    let execution_time_ms = start_time.elapsed().as_millis() as u64;
                    return Ok(Response::new(ScrapePageResponse {
                        success: false,
                        status_code: 0,
                        content: String::new(),
                        error_message: format!("Scope not found: {}", req.scope_name),
                        error_code: browser_hive_proto::coordinator::ErrorCode::ScopeNotFound
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

            if scope_workers.is_empty() {
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
            let selected = select_best_worker(scope_workers, &healthy_guard);
            drop(healthy_guard);

            let selected = match selected {
                Some(s) => {
                    if s.used_fallback {
                        warn!(ray_id = %ray_id, "No healthy workers in health cache, using all discovered workers");
                    }
                    s
                }
                None => {
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

            // Cache says no slots available - but cache may be stale!
            // Fetch fresh stats before rejecting the request
            if best_worker.stats.available_slots == 0 {
                debug!(
                    ray_id = %ray_id,
                    "Cache shows 0 slots for worker {}, fetching fresh stats",
                    best_worker.pod_name
                );

                let fresh_slots = fetch_fresh_worker_stats(&endpoint, &ray_id).await;

                match fresh_slots {
                    Some(slots) if slots > 0 => {
                        // Cache was stale - worker actually has slots available
                        info!(
                            ray_id = %ray_id,
                            "Fresh stats show {} available slots (cache was stale), proceeding with request",
                            slots
                        );
                        // Continue with the request
                    }
                    _ => {
                        // Fresh stats confirm no slots, or fetch failed - reject
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

        // Calculate request deadline
        let request_deadline = start_time + Duration::from_secs(req.timeout_seconds as u64);
        let min_retry_time_remaining = Duration::from_secs(10);
        const MAX_RETRY_ATTEMPTS: u32 = 3;

        let mut excluded_workers = HashSet::new();
        let mut attempt = 0;
        let mut last_worker_id = worker_id.clone();
        let mut last_worker_endpoint = worker_endpoint.clone();

        // Retry loop for TERMINATING errors
        let worker_response = loop {
            attempt += 1;

            // Connect to worker
            let mut client = match WorkerServiceClient::connect(last_worker_endpoint.clone()).await
            {
                Ok(client) => client,
                Err(e) => {
                    return Err(Status::internal(format!(
                        "Failed to connect to worker: {}",
                        e
                    )));
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
                ray_id = %ray_id,
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
                Err(e) => return Err(e),
            };

            let worker_resp = response.into_inner();

            // Check if worker returned TERMINATING error
            let is_terminating = worker_resp.error_code
                == browser_hive_proto::coordinator::ErrorCode::Terminating as i32;

            if !is_terminating {
                // Success or non-retryable error - return response
                break worker_resp;
            }

            // Worker is terminating - check if we should retry
            warn!(
                ray_id = %ray_id,
                "Worker {} returned TERMINATING (attempt {})",
                last_worker_id, attempt
            );

            excluded_workers.insert(last_worker_id.clone());

            // Check if we have time and attempts left
            let now = std::time::Instant::now();
            let remaining_time = request_deadline.saturating_duration_since(now);

            if remaining_time < min_retry_time_remaining {
                warn!(ray_id = %ray_id, "Not enough time remaining for retry ({:?} < {:?}), returning TERMINATING to client", remaining_time, min_retry_time_remaining);
                break worker_resp;
            }

            if attempt >= MAX_RETRY_ATTEMPTS {
                warn!(
                    ray_id = %ray_id,
                    "Max retry attempts ({}) reached, returning TERMINATING to client",
                    MAX_RETRY_ATTEMPTS
                );
                break worker_resp;
            }

            // Try to find another worker
            debug!(ray_id = %ray_id, "Attempting to retry on another worker (remaining time: {:?})", remaining_time);

            let workers_guard = self.worker_discovery.get_workers();
            let workers = workers_guard.read().await;

            let scope_workers = match workers.get(&req.scope_name) {
                Some(w) => w,
                None => {
                    warn!(ray_id = %ray_id, "Scope {} not found during retry", req.scope_name);
                    break worker_resp;
                }
            };

            // Filter by healthy workers and exclude failed workers
            let healthy_guard = self.healthy_workers.read().await;
            let available_workers: Vec<_> = scope_workers
                .iter()
                .filter(|w| {
                    healthy_guard.contains(&w.pod_name) && !excluded_workers.contains(&w.pod_name)
                })
                .collect();
            drop(healthy_guard);

            if available_workers.is_empty() {
                warn!(ray_id = %ray_id, "No healthy workers available for retry");
                break worker_resp;
            }

            let best_worker = available_workers
                .iter()
                .max_by_key(|w| w.stats.available_slots)
                .unwrap();

            if best_worker.stats.available_slots == 0 {
                warn!(ray_id = %ray_id, "No available slots for retry");
                break worker_resp;
            }

            last_worker_id = best_worker.pod_name.clone();
            last_worker_endpoint = format!("http://{}:{}", best_worker.pod_ip, best_worker.port);

            drop(workers);

            debug!(ray_id = %ray_id, "Retrying on worker: {}", last_worker_id);
        };

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
        }
    }

    // ==================== select_best_worker Tests ====================

    #[test]
    fn test_select_best_worker_empty_list() {
        let workers: Vec<WorkerEndpoint> = vec![];
        let healthy = HashSet::new();

        let result = select_best_worker(&workers, &healthy);
        assert!(result.is_none());
    }

    #[test]
    fn test_select_best_worker_single_worker_healthy() {
        let workers = vec![make_worker("worker-1", 5)];
        let healthy: HashSet<String> = ["worker-1".to_string()].into_iter().collect();

        let result = select_best_worker(&workers, &healthy).unwrap();
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

        let result = select_best_worker(&workers, &healthy).unwrap();
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

        let result = select_best_worker(&workers, &healthy).unwrap();
        assert_eq!(result.worker.pod_name, "worker-3");
        assert_eq!(result.worker.stats.available_slots, 5);
        assert!(!result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_fallback_when_no_healthy() {
        let workers = vec![make_worker("worker-1", 2), make_worker("worker-2", 5)];
        // Empty healthy set - should fall back to all workers
        let healthy: HashSet<String> = HashSet::new();

        let result = select_best_worker(&workers, &healthy).unwrap();
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
        let result = select_best_worker(&workers, &healthy).unwrap();
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

        let result = select_best_worker(&workers, &healthy).unwrap();
        assert_eq!(result.worker.pod_name, "worker-1");
        assert_eq!(result.worker.stats.available_slots, 1);
        assert!(!result.used_fallback);
    }

    #[test]
    fn test_select_best_worker_tie_breaker() {
        // When multiple workers have same available_slots, max_by_key returns the last one
        let workers = vec![make_worker("worker-1", 5), make_worker("worker-2", 5)];
        let healthy: HashSet<String> = ["worker-1", "worker-2"]
            .into_iter()
            .map(String::from)
            .collect();

        let result = select_best_worker(&workers, &healthy).unwrap();
        // Both have 5 slots, either is acceptable
        assert!(result.worker.stats.available_slots == 5);
        assert!(!result.used_fallback);
    }
}
