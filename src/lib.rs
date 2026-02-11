//! # Browser Hive
//!
//! Browser Hive is a distributed web scraping system built in Rust that uses
//! browser automation with intelligent session management and proxy rotation.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use browser_hive::prelude::*;
//! use browser_hive::worker::run_worker;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // 1. Implement your custom proxy provider
//!     struct MyProxyProvider { /* your credentials */ }
//!
//!     impl ProxyProvider for MyProxyProvider {
//!         fn build_config(&self) -> anyhow::Result<ProxyConfig> {
//!             Ok(ProxyConfig {
//!                 proxy_url: None,
//!                 scheme: ProxyScheme::Http,
//!                 address: Some("proxy.example.com".to_string()),
//!                 port: Some(8080),
//!                 username: Some("user".to_string()),
//!                 password: Some("pass".to_string()),
//!             })
//!         }
//!
//!         fn name(&self) -> &str { "my_provider" }
//!
//!         fn clone_box(&self) -> Box<dyn ProxyProvider> {
//!             Box::new(self.clone())
//!         }
//!     }
//!
//!     // 2. Create worker configuration
//!     let provider = Box::new(MyProxyProvider { /* ... */ });
//!     let config = WorkerConfig {
//!         scope: ScopeConfig {
//!             name: "my_scope".to_string(),
//!             proxy_provider: provider,
//!             min_contexts: 2,
//!             max_contexts: 10,
//!             session_mode: SessionMode::Reusable, // Reusable contexts (recycled by lifecycle)
//!             headless: true,
//!             lifecycle: Default::default(),
//!             enable_browser_diagnostics: false, // Disable for faster response
//!             binary_params_middlewares: vec![],
//!             tab_init_middlewares: vec![],
//!             context_isolation: ContextIsolation::Isolated, // Each context has isolated cookies/storage
//!         },
//!         grpc_port: 50052,
//!         pod_name: "worker-1".to_string(),
//!         pod_ip: "0.0.0.0".to_string(),
//!     };
//!
//!     // 3. Run the worker
//!     run_worker(config).await
//! }
//! ```
//!
//! ## Architecture
//!
//! Browser Hive consists of:
//! - **Worker**: Manages browser contexts and executes scraping requests
//! - **Coordinator**: Routes requests to workers (optional, for distributed setup)
//! - **Common**: Shared types, traits, and utilities
//! - **Proto**: gRPC protocol definitions
//!
//! ## Features
//!
//! - `worker` (default): Include worker functionality
//! - `coordinator` (default): Include coordinator functionality
//!
//! Use `default-features = false` to include only what you need.

/// Re-export of common types and traits
pub use browser_hive_common as common;

/// Re-export of worker functionality
pub use browser_hive_worker as worker;

/// Re-export of protocol definitions
pub use browser_hive_proto as proto;

/// Convenient re-exports of commonly used types
pub mod prelude {
    // Core traits and proxy types
    pub use crate::common::{ProxyConfig, ProxyParams, ProxyProvider, ProxyScheme};

    // Configuration types
    pub use crate::common::{
        ContextIsolation, ContextLifecycleConfig, CoordinatorConfig, RotationStrategy, ScopeConfig,
        SessionMode, WorkerConfig,
    };

    // Worker functionality
    pub use crate::worker::{run_worker, BrowserPool, Metrics, WorkerService};

    // Built-in proxy providers
    pub use crate::worker::providers::{GenericProxyProvider, NoProxyProvider};

    // Session management
    pub use crate::common::SessionManager;

    // Wait strategy types
    pub use crate::common::{
        NetworkIdleStrategy, TimeoutStrategy, WaitStrategy, WaitStrategyRegistry,
        DEFAULT_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS,
    };
}
