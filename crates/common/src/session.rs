use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Session ID format: {worker_id}:{context_id}
/// Example: "worker-abc123:ctx-def456"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionId {
    pub worker_id: String,
    pub context_id: String,
}

impl SessionId {
    pub fn new(worker_id: String, context_id: String) -> Self {
        Self {
            worker_id,
            context_id,
        }
    }

    pub fn from_string(session_str: &str) -> Option<Self> {
        let parts: Vec<&str> = session_str.split(':').collect();
        if parts.len() == 2 {
            Some(Self {
                worker_id: parts[0].to_string(),
                context_id: parts[1].to_string(),
            })
        } else {
            None
        }
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.worker_id, self.context_id)
    }
}

/// Information about an active session
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub scope_name: String,
    pub worker_endpoint: String, // http://pod-ip:port
    pub created_at: Instant,
    pub last_used_at: Instant,
    pub request_count: u64,
}

/// Session Manager for coordinator - stores mapping between session_id and worker
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, SessionInfo>>>,
    session_ttl: Duration,
}

impl SessionManager {
    pub fn new(session_ttl: Duration) -> Self {
        let manager = Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            session_ttl,
        };

        // Start cleanup task
        manager.start_cleanup_task();

        manager
    }

    /// Create a new session
    pub async fn create_session(
        &self,
        scope_name: String,
        worker_id: String,
        context_id: String,
        worker_endpoint: String,
    ) -> SessionId {
        let session_id = SessionId::new(worker_id, context_id);
        let session_info = SessionInfo {
            session_id: session_id.clone(),
            scope_name,
            worker_endpoint,
            created_at: Instant::now(),
            last_used_at: Instant::now(),
            request_count: 1,
        };

        self.sessions
            .write()
            .await
            .insert(session_id.to_string(), session_info);

        session_id
    }

    /// Get session information
    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let mut sessions = self.sessions.write().await;

        if let Some(session) = sessions.get_mut(session_id) {
            // Update last used time
            session.last_used_at = Instant::now();
            session.request_count += 1;
            Some(session.clone())
        } else {
            None
        }
    }

    /// Remove session (if worker is unavailable or context recycled)
    pub async fn remove_session(&self, session_id: &str) {
        self.sessions.write().await.remove(session_id);
    }

    /// Cleanup task for removing old sessions
    fn start_cleanup_task(&self) {
        let sessions = self.sessions.clone();
        let ttl = self.session_ttl;

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;

                let mut sessions_guard = sessions.write().await;
                let now = Instant::now();

                // Remove sessions older than TTL
                sessions_guard.retain(|_id, info| now.duration_since(info.last_used_at) < ttl);
            }
        });
    }

    /// Get session statistics
    pub async fn get_stats(&self) -> SessionStats {
        let sessions = self.sessions.read().await;

        SessionStats {
            total_sessions: sessions.len(),
            sessions_by_scope: sessions.values().fold(HashMap::new(), |mut acc, session| {
                *acc.entry(session.scope_name.clone()).or_insert(0) += 1;
                acc
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub total_sessions: usize,
    pub sessions_by_scope: HashMap<String, usize>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new(Duration::from_secs(30 * 60)) // 30 minutes TTL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== SessionId Tests ====================

    #[test]
    fn test_session_id_new() {
        let session_id = SessionId::new("worker-1".to_string(), "ctx-abc".to_string());
        assert_eq!(session_id.worker_id, "worker-1");
        assert_eq!(session_id.context_id, "ctx-abc");
    }

    #[test]
    fn test_session_id_from_string_valid() {
        let session_id = SessionId::from_string("worker-1:ctx-abc").unwrap();
        assert_eq!(session_id.worker_id, "worker-1");
        assert_eq!(session_id.context_id, "ctx-abc");
    }

    #[test]
    fn test_session_id_from_string_invalid() {
        // No colon
        assert!(SessionId::from_string("invalid").is_none());
        // Too many colons - takes first two parts
        assert!(SessionId::from_string("a:b:c").is_none());
        // Empty string
        assert!(SessionId::from_string("").is_none());
    }

    #[test]
    fn test_session_id_display() {
        let session_id = SessionId::new("worker-1".to_string(), "ctx-abc".to_string());
        assert_eq!(session_id.to_string(), "worker-1:ctx-abc");
    }

    #[test]
    fn test_session_id_roundtrip() {
        let original = SessionId::new("my-worker".to_string(), "my-context".to_string());
        let as_string = original.to_string();
        let parsed = SessionId::from_string(&as_string).unwrap();

        assert_eq!(original.worker_id, parsed.worker_id);
        assert_eq!(original.context_id, parsed.context_id);
    }

    // ==================== SessionManager Tests ====================

    #[tokio::test]
    async fn test_create_session() {
        let manager = SessionManager::new(Duration::from_secs(300));

        let session_id = manager
            .create_session(
                "test_scope".to_string(),
                "worker-1".to_string(),
                "ctx-123".to_string(),
                "http://10.0.0.1:50052".to_string(),
            )
            .await;

        assert_eq!(session_id.worker_id, "worker-1");
        assert_eq!(session_id.context_id, "ctx-123");

        // Verify session is stored
        let retrieved = manager.get_session(&session_id.to_string()).await;
        assert!(retrieved.is_some());

        let info = retrieved.unwrap();
        assert_eq!(info.scope_name, "test_scope");
        assert_eq!(info.worker_endpoint, "http://10.0.0.1:50052");
    }

    #[tokio::test]
    async fn test_get_session_updates_stats() {
        let manager = SessionManager::new(Duration::from_secs(300));

        let session_id = manager
            .create_session(
                "scope".to_string(),
                "worker".to_string(),
                "ctx".to_string(),
                "http://localhost:50052".to_string(),
            )
            .await;

        // First get - request_count should be 2 (1 from create + 1 from get)
        let info1 = manager.get_session(&session_id.to_string()).await.unwrap();
        assert_eq!(info1.request_count, 2);

        // Second get - request_count should be 3
        let info2 = manager.get_session(&session_id.to_string()).await.unwrap();
        assert_eq!(info2.request_count, 3);
    }

    #[tokio::test]
    async fn test_get_session_nonexistent() {
        let manager = SessionManager::new(Duration::from_secs(300));

        let result = manager.get_session("nonexistent:session").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_remove_session() {
        let manager = SessionManager::new(Duration::from_secs(300));

        let session_id = manager
            .create_session(
                "scope".to_string(),
                "worker".to_string(),
                "ctx".to_string(),
                "http://localhost:50052".to_string(),
            )
            .await;

        // Verify exists
        assert!(manager.get_session(&session_id.to_string()).await.is_some());

        // Remove
        manager.remove_session(&session_id.to_string()).await;

        // Verify removed
        assert!(manager.get_session(&session_id.to_string()).await.is_none());
    }

    #[tokio::test]
    async fn test_get_stats() {
        let manager = SessionManager::new(Duration::from_secs(300));

        // Create sessions in different scopes
        manager
            .create_session(
                "scope_a".to_string(),
                "w1".to_string(),
                "c1".to_string(),
                "http://a".to_string(),
            )
            .await;
        manager
            .create_session(
                "scope_a".to_string(),
                "w2".to_string(),
                "c2".to_string(),
                "http://b".to_string(),
            )
            .await;
        manager
            .create_session(
                "scope_b".to_string(),
                "w3".to_string(),
                "c3".to_string(),
                "http://c".to_string(),
            )
            .await;

        let stats = manager.get_stats().await;

        assert_eq!(stats.total_sessions, 3);
        assert_eq!(stats.sessions_by_scope.get("scope_a"), Some(&2));
        assert_eq!(stats.sessions_by_scope.get("scope_b"), Some(&1));
    }

    #[tokio::test]
    async fn test_multiple_sessions_same_worker() {
        let manager = SessionManager::new(Duration::from_secs(300));

        // Same worker, different contexts
        let s1 = manager
            .create_session(
                "scope".to_string(),
                "worker-1".to_string(),
                "ctx-1".to_string(),
                "http://w1".to_string(),
            )
            .await;
        let s2 = manager
            .create_session(
                "scope".to_string(),
                "worker-1".to_string(),
                "ctx-2".to_string(),
                "http://w1".to_string(),
            )
            .await;

        // Both should exist independently
        assert!(manager.get_session(&s1.to_string()).await.is_some());
        assert!(manager.get_session(&s2.to_string()).await.is_some());

        // Stats should show 2 sessions
        let stats = manager.get_stats().await;
        assert_eq!(stats.total_sessions, 2);
    }
}
