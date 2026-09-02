use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::proxy::ProxyConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEndpoint {
    pub pod_name: String,
    pub pod_ip: String,
    pub port: u16,
    pub scope_name: String,
    pub stats: WorkerStats,
    /// True when the pod is terminating (K8s `deletionTimestamp` is set) as of the
    /// last discovery round. Used only to downgrade routine connect/stats-error logs
    /// during graceful shutdown; never affects routing or health decisions. May be
    /// up to one discovery interval stale, which is harmless for a log-level gate.
    #[serde(default)]
    pub is_terminating: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerStats {
    pub scope_name: String,
    pub pod_name: String,
    pub pod_ip: String,
    pub total_contexts: usize,
    pub available_slots: usize,
    pub active_requests: usize,
    pub total_requests: u64,
    pub total_contexts_created: u64,
    pub total_contexts_recycled: u64,
    pub success_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeStats {
    pub name: String,
    pub total_workers: usize,
    pub total_slots: usize,
    pub available_slots: usize,
    pub active_requests: usize,
    pub success_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStats {
    pub scopes: HashMap<String, ScopeStats>,
    pub total_requests: u64,
    pub active_jobs: u64,
}

// Browser context lifecycle tracking
#[derive(Debug)]
pub struct BrowserContextMetadata {
    pub id: Uuid,
    pub created_at: Instant,
    pub last_used_at: Arc<Mutex<Instant>>,
    pub total_requests: Arc<AtomicU64>,
    pub cache_size_mb: Arc<AtomicU64>,
    pub is_busy: Arc<AtomicBool>, // True when processing a request
    /// Context-specific proxy config (overrides global scope proxy if set)
    /// Used for providers that need per-context proxy assignment (e.g. a datacenter pool where
    /// each context gets its own exit IP)
    pub assigned_proxy_config: Option<ProxyConfig>,
    /// Origins this context is currently refused by, each with the instant its quarantine ends.
    ///
    /// A block is a property of the **pair** (exit IP, origin), not of the context: the same
    /// context is dead for one site and perfectly warm — cookies and all — for every other one.
    /// So a block is recorded here rather than by destroying the context, and only context
    /// selection for that origin skips it. See [`SessionMode::Reusable`] in `config.rs`.
    ///
    /// A `std::sync::Mutex` (not tokio's) on purpose: it is read inside the synchronous context
    /// selection, and every critical section here is a few map operations with no `.await`.
    pub blocked_origins: Arc<StdMutex<HashMap<String, Instant>>>,
}

/// How many distinct origins one context remembers a block for.
///
/// The map is per context and dies with it, but a `reusable` context can outlive thousands of
/// requests across many hosts, so it still needs a bound. Expired entries are dropped first;
/// only if all of them are live does the soonest-expiring one make room.
const MAX_BLOCKED_ORIGINS: usize = 32;

impl Default for BrowserContextMetadata {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            created_at: Instant::now(),
            last_used_at: Arc::new(Mutex::new(Instant::now())),
            total_requests: Arc::new(AtomicU64::new(0)),
            cache_size_mb: Arc::new(AtomicU64::new(0)),
            is_busy: Arc::new(AtomicBool::new(false)),
            assigned_proxy_config: None,
            blocked_origins: Arc::new(StdMutex::new(HashMap::new())),
        }
    }
}

impl BrowserContextMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `origin` refused this context, and keep it out of that origin's selection
    /// until `now + cooldown`.
    ///
    /// Re-blocking simply re-stamps the deadline; there is no escalating backoff, because the
    /// quarantine costs nothing to hold — the context keeps serving every other origin — and an
    /// escalation would need failure history the pool has no other use for.
    pub fn quarantine_origin(&self, origin: &str, cooldown: Duration) {
        let Ok(mut blocked) = self.blocked_origins.lock() else {
            return;
        };
        let now = Instant::now();
        let until = now + cooldown;

        if !blocked.contains_key(origin) && blocked.len() >= MAX_BLOCKED_ORIGINS {
            blocked.retain(|_, deadline| *deadline > now);
            if blocked.len() >= MAX_BLOCKED_ORIGINS {
                if let Some(earliest) = blocked
                    .iter()
                    .min_by_key(|(_, deadline)| **deadline)
                    .map(|(origin, _)| origin.clone())
                {
                    blocked.remove(&earliest);
                }
            }
        }

        blocked.insert(origin.to_string(), until);
    }

    /// When this context's quarantine for `origin` ends, or `None` if it may serve it now.
    ///
    /// Expired entries are removed as they are found, so the map self-prunes on the read path
    /// and needs no sweeper of its own.
    pub fn quarantined_until(&self, origin: &str) -> Option<Instant> {
        let mut blocked = self.blocked_origins.lock().ok()?;
        match blocked.get(origin) {
            Some(deadline) if *deadline > Instant::now() => Some(*deadline),
            Some(_) => {
                blocked.remove(origin);
                None
            }
            None => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapingRequest {
    pub url: String,
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapingResponse {
    pub success: bool,
    pub status_code: u32,
    pub content: String,
    pub error_message: String,
    pub response_headers: HashMap<String, String>,
    pub execution_time_ms: u64,
}
