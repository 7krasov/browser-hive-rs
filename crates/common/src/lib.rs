pub mod browser_middleware;
pub mod config;
pub mod proxy;
pub mod request_context;
pub mod session;
pub mod types;
pub mod utils;
pub mod wait_strategy;

pub use browser_middleware::{
    BraveBinaryParamsMiddleware, BrowserBinaryParamsMiddleware, DefaultBinaryParamsMiddleware,
    DefaultTabInitMiddleware, TabInitMiddleware,
};
pub use config::*;
pub use proxy::{ProxyConfig, ProxyProvider, ProxyScheme};
pub use request_context::{ProxyParams, RequestContext, ScrapeParams};
pub use session::*;
pub use types::*;
pub use wait_strategy::{
    effective_timeout, validate_timeout, NetworkIdleStrategy, TimeoutStrategy, WaitResult,
    WaitStrategy, WaitStrategyRegistry, DEFAULT_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS,
};
