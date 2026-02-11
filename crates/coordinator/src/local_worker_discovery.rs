use anyhow::Result;
use browser_hive_common::{WorkerEndpoint, WorkerStats};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// Simple worker discovery for local development
/// Discovers workers by environment variables with format:
/// WORKER_ENDPOINTS=scope1:host1:port1,scope2:host2:port2,...
/// Falls back to legacy single worker format: WORKER_ENDPOINT + WORKER_SCOPE_NAME
pub struct LocalWorkerDiscovery {
    workers: Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>>,
}

impl LocalWorkerDiscovery {
    pub async fn new() -> Result<Self> {
        let mut workers_map = HashMap::new();

        // Try new multi-worker format first
        if let Ok(endpoints_str) = env::var("WORKER_ENDPOINTS") {
            info!("Local mode: parsing multiple worker endpoints from WORKER_ENDPOINTS");

            for endpoint_spec in endpoints_str.split(',') {
                let parts: Vec<&str> = endpoint_spec.trim().split(':').collect();
                if parts.len() < 3 {
                    tracing::warn!(
                        "Invalid endpoint spec (expected scope:host:port): {}",
                        endpoint_spec
                    );
                    continue;
                }

                let scope_name = parts[0].to_string();
                let pod_ip = parts[1].to_string();
                let port = parts[2].parse::<u16>().unwrap_or(50052);

                let endpoint = WorkerEndpoint {
                    pod_name: format!("worker-{}", scope_name),
                    pod_ip: pod_ip.clone(),
                    port,
                    scope_name: scope_name.clone(),
                    stats: WorkerStats {
                        scope_name: scope_name.clone(),
                        pod_name: format!("worker-{}", scope_name),
                        pod_ip: pod_ip.clone(),
                        total_contexts: 3,  // Assume default
                        available_slots: 3, // Assume all available initially
                        active_requests: 0,
                        total_requests: 0,
                        total_contexts_created: 0,
                        total_contexts_recycled: 0,
                        success_rate: 1.0,
                    },
                };

                workers_map
                    .entry(scope_name.clone())
                    .or_insert_with(Vec::new)
                    .push(endpoint);

                info!(
                    "Registered worker: scope={}, endpoint={}:{}",
                    scope_name, pod_ip, port
                );
            }
        } else {
            // Fallback to legacy single-worker format
            let worker_endpoint =
                env::var("WORKER_ENDPOINT").unwrap_or_else(|_| "worker:50052".to_string());

            let worker_scope =
                env::var("WORKER_SCOPE_NAME").unwrap_or_else(|_| "local_dev".to_string());

            info!(
                "Local mode: using single worker endpoint: {} (scope: {})",
                worker_endpoint, worker_scope
            );

            // Parse endpoint
            let parts: Vec<&str> = worker_endpoint.split(':').collect();
            let pod_ip = parts
                .first()
                .ok_or_else(|| anyhow::anyhow!("Invalid worker endpoint"))?
                .to_string();
            let port = parts
                .get(1)
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(50052);

            // Create single worker endpoint
            let endpoint = WorkerEndpoint {
                pod_name: "worker-local".to_string(),
                pod_ip: pod_ip.clone(),
                port,
                scope_name: worker_scope.clone(),
                stats: WorkerStats {
                    scope_name: worker_scope.clone(),
                    pod_name: "worker-local".to_string(),
                    pod_ip: pod_ip.clone(),
                    total_contexts: 3,  // Assume default
                    available_slots: 3, // Assume all available initially
                    active_requests: 0,
                    total_requests: 0,
                    total_contexts_created: 0,
                    total_contexts_recycled: 0,
                    success_rate: 1.0,
                },
            };

            workers_map.insert(worker_scope, vec![endpoint]);
        }

        Ok(Self {
            workers: Arc::new(RwLock::new(workers_map)),
        })
    }

    pub fn get_workers(&self) -> Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>> {
        self.workers.clone()
    }

    pub async fn start_discovery(&self) {
        info!("Local worker discovery - using static configuration (no periodic updates)");
        // In local mode, we don't need periodic discovery
        // Worker endpoint is hardcoded and never changes
    }
}
