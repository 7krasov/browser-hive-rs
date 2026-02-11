use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::proxy::ProxyConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEndpoint {
    pub pod_name: String,
    pub pod_ip: String,
    pub port: u16,
    pub scope_name: String,
    pub stats: WorkerStats,
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
    pub primary_domains: Arc<RwLock<HashSet<String>>>,
    pub is_busy: Arc<AtomicBool>, // True when processing a request
    /// Context-specific proxy config (overrides global scope proxy if set)
    /// Used for providers that need per-context proxy assignment (e.g., Oxylabs DC with proxy pool)
    pub assigned_proxy_config: Option<ProxyConfig>,
}

impl Default for BrowserContextMetadata {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            created_at: Instant::now(),
            last_used_at: Arc::new(Mutex::new(Instant::now())),
            total_requests: Arc::new(AtomicU64::new(0)),
            cache_size_mb: Arc::new(AtomicU64::new(0)),
            primary_domains: Arc::new(RwLock::new(HashSet::new())),
            is_busy: Arc::new(AtomicBool::new(false)),
            assigned_proxy_config: None,
        }
    }
}

impl BrowserContextMetadata {
    pub fn new() -> Self {
        Self::default()
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
