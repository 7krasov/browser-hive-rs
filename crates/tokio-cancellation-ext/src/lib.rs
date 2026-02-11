//! # Tokio Cancellation Extensions
//!
//! This crate provides extension traits and helpers for adding cancellation support to Tokio
//! futures and synchronous code in a clean and ergonomic way, eliminating the need for
//! repetitive `tokio::select!` boilerplate.
//!
//! ## Problem
//!
//! When working with async Rust and Tokio, adding cancellation support to futures often
//! requires repetitive `tokio::select!` boilerplate:
//!
//! ```ignore
//! // Repetitive boilerplate code
//! let result = tokio::select! {
//!     _ = cancellation_token.cancelled() => {
//!         return Err(MyError::Cancelled);
//!     }
//!     result = some_future => {
//!         result?
//!     }
//! };
//! ```
//!
//! ## Solution
//!
//! This crate provides a simple extension trait that eliminates this boilerplate:
//!
//! ```rust
//! use tokio_cancellation_ext::{CancellationExt, CancellationError, CancellationToken};
//!
//! #[derive(Debug)]
//! enum MyError {
//!     Cancelled,
//!     Database(String),
//! }
//!
//! impl From<CancellationError> for MyError {
//!     fn from(_: CancellationError) -> Self {
//!         MyError::Cancelled
//!     }
//! }
//!
//! # async fn fetch_data() -> Result<String, MyError> { Ok("data".to_string()) }
//! # #[tokio::main]
//! # async fn main() -> Result<(), MyError> {
//! let token = CancellationToken::new();
//!
//! // Clean, readable code without boilerplate
//! let result = fetch_data()
//!     .with_cancellation::<MyError>(&token, "fetch_data")
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Features
//!
//! - **Zero boilerplate**: Replace `tokio::select!` with simple `.with_cancellation()` calls
//! - **Type-safe**: Works with any error type that can convert from `CancellationError`
//! - **Thread-safe**: Fully `Send + Sync` compatible
//! - **Ergonomic**: Chainable API that feels natural
//! - **Logging support**: Built-in context logging for debugging
//! - **Sync support**: Helper function for synchronous code (loops, polling)
//! - **Minimal dependencies**: Only requires `tokio-util` and `log`
//!
//! ## Usage Examples
//!
//! ### Async Operations
//!
//! ```rust
//! use tokio_cancellation_ext::{CancellationExt, CancellationError, CancellationToken};
//!
//! #[derive(Debug)]
//! enum AppError {
//!     Cancelled,
//!     Network(String),
//!     Database(String),
//! }
//!
//! impl From<CancellationError> for AppError {
//!     fn from(_: CancellationError) -> Self {
//!         AppError::Cancelled
//!     }
//! }
//!
//! # async fn network_call() -> Result<String, AppError> { Ok("response".to_string()) }
//! # async fn database_query() -> Result<Vec<u8>, AppError> { Ok(vec![1, 2, 3]) }
//! # #[tokio::main]
//! # async fn main() -> Result<(), AppError> {
//! let token = CancellationToken::new();
//!
//! // Multiple cancellable operations
//! let response = network_call()
//!     .with_cancellation::<AppError>(&token, "network_call")
//!     .await?;
//!
//! let data = database_query()
//!     .with_cancellation::<AppError>(&token, "database_query")
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ### Synchronous Code (Loops and Polling)
//!
//! ```rust
//! use tokio_cancellation_ext::{check_cancellation, CancellationError, CancellationToken};
//!
//! #[derive(Debug)]
//! enum WorkerError {
//!     Cancelled,
//!     Other(String),
//! }
//!
//! impl From<CancellationError> for WorkerError {
//!     fn from(_: CancellationError) -> Self {
//!         WorkerError::Cancelled
//!     }
//! }
//!
//! fn polling_loop(token: &CancellationToken) -> Result<(), WorkerError> {
//!     loop {
//!         // Check for cancellation at the start of each iteration
//!         check_cancellation(token, "polling_loop")?;
//!
//!         // Do some work
//!         std::thread::sleep(std::time::Duration::from_millis(500));
//!
//!         // Check completion condition
//!         if is_done() {
//!             break;
//!         }
//!     }
//!     Ok(())
//! }
//!
//! fn is_done() -> bool { true }
//! ```

mod async_ext;
mod error;
mod sync_ext;

pub use async_ext::CancellationExt;
pub use error::CancellationError;
pub use sync_ext::check_cancellation;

// Re-export CancellationToken for convenience
pub use tokio_util::sync::CancellationToken;
