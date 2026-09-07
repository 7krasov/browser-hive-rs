use anyhow::Result;
use browser_hive_common::WorkerEndpoint;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use kube::{api::ListParams, Api, Client};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, Instrument};

/// What the K8s pod list says about a scope, regardless of whether any of its pods can serve.
///
/// This exists because the routable map alone cannot tell "there is no such scope" from "the
/// scope is restarting". A scope enters that map only when one of its pods answered `GetStats`
/// (see [`WorkerDiscovery::discover_workers`]), so a whole scope disappears from it while its
/// pods are booting — a worker's gRPC server starts only after its browser pool is up, which
/// takes seconds — and a request arriving in that window was answered `SCOPE_NOT_FOUND`, i.e.
/// "fix your configuration", for what is really "come back shortly".
///
/// The `scope` **label of a pod** is the honest answer to "does this scope exist in the
/// cluster": it is set by the deployment, is present from the moment the pod object is created,
/// and is already in the list the discovery loop fetches — so this costs no extra API call.
///
/// The counts are mutually exclusive; together they equal `pods_total`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopePresence {
    /// Pods carrying this scope label, in any phase.
    pub pods_total: usize,
    /// Pods that answered `GetStats` and are therefore routable.
    pub pods_reachable: usize,
    /// Pods not in phase `Running` — starting up, or stuck.
    pub pods_not_running: usize,
    /// Running pods whose `deletionTimestamp` is set and that no longer answer.
    pub pods_terminating: usize,
    /// Running, not terminating, but unreachable — the booting-worker case.
    pub pods_unreachable: usize,
}

impl ScopePresence {
    /// Human-readable breakdown for the client-facing error message. The client cannot query
    /// K8s, so the pod states are the only way it learns whether waiting will help.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.pods_not_running > 0 {
            parts.push(format!("{} not running", self.pods_not_running));
        }
        if self.pods_unreachable > 0 {
            parts.push(format!(
                "{} starting up or unreachable",
                self.pods_unreachable
            ));
        }
        if self.pods_terminating > 0 {
            parts.push(format!("{} terminating", self.pods_terminating));
        }
        let detail = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };
        format!(
            "{} pod(s), {} reachable{}",
            self.pods_total, self.pods_reachable, detail
        )
    }
}

pub struct WorkerDiscovery {
    kube_client: Client,
    workers: Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>>,
    /// Every scope the cluster has pods for, routable or not. See [`ScopePresence`].
    known_scopes: Arc<RwLock<HashMap<String, ScopePresence>>>,
}

impl WorkerDiscovery {
    pub async fn new() -> Result<Self> {
        let client = Client::try_default().await?;
        Ok(Self {
            kube_client: client,
            workers: Arc::new(RwLock::new(HashMap::new())),
            known_scopes: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn get_workers(&self) -> Arc<RwLock<HashMap<String, Vec<WorkerEndpoint>>>> {
        self.workers.clone()
    }

    /// Scopes the cluster has pods for, whether or not any of them can currently serve.
    pub fn get_known_scopes(&self) -> Arc<RwLock<HashMap<String, ScopePresence>>> {
        self.known_scopes.clone()
    }

    pub async fn start_discovery(&self) {
        let pods: Api<Pod> = Api::default_namespaced(self.kube_client.clone());
        let workers = self.workers.clone();
        let known_scopes = self.known_scopes.clone();

        // Whether to log "failed to get stats" warnings for terminating pods.
        // Terminating pods (deletionTimestamp set) routinely fail GetStats during
        // graceful shutdown / rollout churn, producing expected, non-actionable
        // noise. Read once at startup (env vars don't change at runtime).
        let log_terminating_pod_warnings =
            std::env::var("COORDINATOR_ENABLE_TERMINATING_POD_WARNINGS")
                .map(|v| v == "true")
                .unwrap_or(false);

        // Background task: no request span reaches it, so it opens its own with a sentinel
        // ray_id (see the worker's lifecycle monitor for the same pattern). Scope is per
        // discovered worker, not per task, so it stays in the messages.
        let span = tracing::info_span!("worker_discovery", ray_id = "worker-discovery");

        let discovery = async move {
            loop {
                match Self::discover_workers(&pods, log_terminating_pod_warnings).await {
                    Ok((discovered_workers, presence)) => {
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

                        // Both maps are replaced together: the presence map is what tells a
                        // request for a scope missing from `workers` whether the scope exists at
                        // all, so a stale one would mislabel a genuinely unknown scope.
                        *workers.write().await = discovered_workers;
                        *known_scopes.write().await = presence;
                    }
                    Err(e) => {
                        error!("Failed to discover workers: {}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        };

        tokio::spawn(discovery.instrument(span));
    }

    /// Returns the routable workers **and** what the pod list says about every scope.
    ///
    /// The two are deliberately separate: a scope belongs in the second map from the moment a pod
    /// carrying its label exists, and in the first only once one of those pods answers `GetStats`.
    /// See [`ScopePresence`] for why the difference is what the client is told.
    async fn discover_workers(
        pods: &Api<Pod>,
        log_terminating_pod_warnings: bool,
    ) -> Result<(
        HashMap<String, Vec<WorkerEndpoint>>,
        HashMap<String, ScopePresence>,
    )> {
        // List all pods with label app=browser-hive-worker
        let list_params = ListParams::default().labels("app=browser-hive-worker");
        let pod_list = pods.list(&list_params).await?;

        let mut workers_by_scope: HashMap<String, Vec<WorkerEndpoint>> = HashMap::new();
        let mut presence_by_scope: HashMap<String, ScopePresence> = HashMap::new();

        for pod in pod_list.items {
            // The scope label is read FIRST, before any phase or reachability check: it is the
            // only evidence that a scope exists in the cluster, and it must survive a pod that
            // cannot serve. A pod without it is ignored rather than aborting the round — one
            // mislabelled pod must not erase every scope from both maps (the failure this whole
            // presence map exists to prevent).
            let scope_name = match pod
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("scope"))
            {
                Some(scope) => scope.clone(),
                None => {
                    tracing::warn!(
                        "Pod {} has no 'scope' label, ignoring",
                        pod.metadata.name.as_deref().unwrap_or("<unnamed>")
                    );
                    continue;
                }
            };

            let presence = presence_by_scope.entry(scope_name.clone()).or_default();
            presence.pods_total += 1;

            // Skip pods that are not running
            if !Self::is_pod_running(&pod.status) {
                presence.pods_not_running += 1;
                continue;
            }

            // A pod with deletionTimestamp set is terminating (SIGTERM in flight),
            // but K8s can still report phase=Running until the process exits. Such
            // pods frequently fail GetStats because their gRPC server is already
            // gone. We keep discovering them (they may still serve until removed on
            // the next round), but downgrade the stats-error log to debug so routine
            // shutdown churn does not spam warnings. Set
            // COORDINATOR_ENABLE_TERMINATING_POD_WARNINGS=true to surface these
            // warnings for debugging.
            let is_terminating = pod.metadata.deletion_timestamp.is_some();

            // Extract pod metadata. Same rule as the scope label above: a single unusable pod
            // is skipped, never propagated as an error — aborting the round would empty both
            // maps and turn one odd pod into a fleet-wide "scope not found".
            let pod_name = match pod.metadata.name {
                Some(name) => name,
                None => {
                    tracing::warn!("Pod in scope {} has no name, ignoring", scope_name);
                    presence.pods_not_running += 1;
                    continue;
                }
            };

            let pod_ip = match pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()) {
                Some(ip) => ip,
                None => {
                    tracing::warn!("Pod {} has no IP yet, ignoring", pod_name);
                    presence.pods_not_running += 1;
                    continue;
                }
            };

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
                        is_terminating,
                    };

                    presence.pods_reachable += 1;

                    workers_by_scope
                        .entry(scope_name)
                        .or_default()
                        .push(endpoint);

                    tracing::debug!("Discovered worker: {} ({}:{})", pod_name, pod_ip, port);
                }
                Err(e) => {
                    // Unreachable, and the reason matters to the client: a terminating pod means
                    // a rollout, a running one means a worker still booting its browser. Both are
                    // "retry shortly", but only the breakdown says so out loud.
                    if is_terminating {
                        presence.pods_terminating += 1;
                    } else {
                        presence.pods_unreachable += 1;
                    }

                    // Suppress the warning for terminating pods (expected churn),
                    // unless explicitly enabled for debugging via the env var.
                    if is_terminating && !log_terminating_pod_warnings {
                        tracing::debug!(
                            "Failed to get stats from terminating worker {} ({}:{}): {}",
                            pod_name,
                            pod_ip,
                            port,
                            e
                        );
                    } else {
                        tracing::warn!(
                            "Failed to get stats from worker {} ({}:{}): {}",
                            pod_name,
                            pod_ip,
                            port,
                            e
                        );
                    }
                    // Continue to next pod instead of failing entire discovery
                    continue;
                }
            }
        }

        Ok((workers_by_scope, presence_by_scope))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The breakdown is client-facing text: it must name the pod states, because they are the
    /// difference between "wait a few seconds" (booting) and "a rollout is in progress".
    #[test]
    fn describe_names_every_non_serving_state() {
        let presence = ScopePresence {
            pods_total: 4,
            pods_reachable: 1,
            pods_not_running: 1,
            pods_terminating: 1,
            pods_unreachable: 1,
        };

        let described = presence.describe();

        assert_eq!(
            described,
            "4 pod(s), 1 reachable (1 not running, 1 starting up or unreachable, 1 terminating)"
        );
    }

    /// A fully healthy scope never reaches the client-facing path, but the text must not read as
    /// if something were wrong when it is used in a log line.
    #[test]
    fn describe_omits_empty_buckets() {
        let presence = ScopePresence {
            pods_total: 2,
            pods_reachable: 2,
            ..Default::default()
        };

        assert_eq!(presence.describe(), "2 pod(s), 2 reachable");
    }
}
