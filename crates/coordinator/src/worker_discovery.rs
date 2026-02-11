use anyhow::Result;
use browser_hive_common::WorkerEndpoint;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use kube::{api::ListParams, Api, Client};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::error;

pub struct WorkerDiscovery {
    kube_client: Client,
    workers: Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>>,
}

impl WorkerDiscovery {
    pub async fn new() -> Result<Self> {
        let client = Client::try_default().await?;
        Ok(Self {
            kube_client: client,
            workers: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn get_workers(&self) -> Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>> {
        self.workers.clone()
    }

    pub async fn start_discovery(&self) {
        let pods: Api<Pod> = Api::default_namespaced(self.kube_client.clone());
        let workers = self.workers.clone();

        tokio::spawn(async move {
            loop {
                match Self::discover_workers(&pods).await {
                    Ok(discovered_workers) => {
                        let total_workers: usize =
                            discovered_workers.values().map(|v| v.len()).sum();
                        tracing::debug!(
                            "Discovered {} worker(s) across {} scope(s)",
                            total_workers,
                            discovered_workers.len()
                        );

                        // Log details for each scope
                        for (scope, workers_list) in &discovered_workers {
                            let available_slots: usize =
                                workers_list.iter().map(|w| w.stats.available_slots).sum();
                            tracing::debug!(
                                "Scope '{}': {} worker(s), {} available slot(s)",
                                scope,
                                workers_list.len(),
                                available_slots
                            );
                        }

                        *workers.write().await = discovered_workers;
                    }
                    Err(e) => {
                        error!("Failed to discover workers: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }

    async fn discover_workers(pods: &Api<Pod>) -> Result<HashMap<String, Vec<WorkerEndpoint>>> {
        // List all pods with label app=browser-hive-worker
        let list_params = ListParams::default().labels("app=browser-hive-worker");
        let pod_list = pods.list(&list_params).await?;

        let mut workers_by_scope: HashMap<String, Vec<WorkerEndpoint>> = HashMap::new();

        for pod in pod_list.items {
            // Skip pods that are not running
            if !Self::is_pod_running(&pod.status) {
                continue;
            }

            // Extract pod metadata
            let pod_name = pod
                .metadata
                .name
                .ok_or_else(|| anyhow::anyhow!("Pod missing name"))?;

            let pod_ip = pod
                .status
                .as_ref()
                .and_then(|s| s.pod_ip.as_ref())
                .ok_or_else(|| anyhow::anyhow!("Pod {} missing IP", pod_name))?;

            // Get scope from pod labels
            let scope_name = pod
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("scope"))
                .ok_or_else(|| anyhow::anyhow!("Pod {} missing scope label", pod_name))?
                .clone();

            // Get worker gRPC port (default 50052)
            let port = 50052u16;

            // Try to connect to worker and get stats
            match Self::fetch_worker_stats(pod_ip, port).await {
                Ok(stats) => {
                    let endpoint = WorkerEndpoint {
                        pod_name: pod_name.clone(),
                        pod_ip: pod_ip.clone(),
                        port,
                        scope_name: scope_name.clone(),
                        stats,
                    };

                    workers_by_scope
                        .entry(scope_name)
                        .or_default()
                        .push(endpoint);

                    tracing::debug!("Discovered worker: {} ({}:{})", pod_name, pod_ip, port);
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to get stats from worker {} ({}:{}): {}",
                        pod_name,
                        pod_ip,
                        port,
                        e
                    );
                    // Continue to next pod instead of failing entire discovery
                    continue;
                }
            }
        }

        Ok(workers_by_scope)
    }

    fn is_pod_running(status: &Option<PodStatus>) -> bool {
        status
            .as_ref()
            .and_then(|s| s.phase.as_ref())
            .map(|phase| phase == "Running")
            .unwrap_or(false)
    }

    async fn fetch_worker_stats(
        pod_ip: &str,
        port: u16,
    ) -> Result<browser_hive_common::WorkerStats> {
        use browser_hive_proto::worker::worker_service_client::WorkerServiceClient;

        let endpoint = format!("http://{}:{}", pod_ip, port);

        // Set a timeout for connection
        let mut client = tokio::time::timeout(
            Duration::from_secs(3),
            WorkerServiceClient::connect(endpoint.clone()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Connection timeout to {}", endpoint))??;

        // Get stats from worker
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            client.get_stats(tonic::Request::new(())),
        )
        .await
        .map_err(|_| anyhow::anyhow!("GetStats timeout for {}", endpoint))??;

        let stats_proto = response.into_inner();

        // Convert proto stats to common WorkerStats
        Ok(browser_hive_common::WorkerStats {
            scope_name: stats_proto.scope_name,
            pod_name: stats_proto.pod_name,
            pod_ip: stats_proto.pod_ip,
            total_contexts: stats_proto.total_contexts as usize,
            available_slots: stats_proto.available_slots as usize,
            active_requests: stats_proto.active_requests as usize,
            total_requests: stats_proto.total_requests,
            total_contexts_created: stats_proto.total_contexts_created,
            total_contexts_recycled: stats_proto.total_contexts_recycled,
            success_rate: stats_proto.success_rate,
        })
    }
}
