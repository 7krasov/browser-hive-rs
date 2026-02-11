/// Error type representing a cancellation request
///
/// This error is returned when an operation is cancelled via a `CancellationToken`.
/// It implements the standard `Error` trait and can be converted to other error types
/// via the `From`/`Into` traits.
///
/// # Examples
///
/// ## Converting to Custom Error Types
///
/// ```rust
/// use tokio_cancellation_ext::CancellationError;
///
/// #[derive(Debug)]
/// enum MyError {
///     Cancelled,
///     Network(String),
///     Database(String),
/// }
///
/// impl From<CancellationError> for MyError {
///     fn from(_: CancellationError) -> Self {
///         MyError::Cancelled
///     }
/// }
///
/// // Now CancellationError can be automatically converted to MyError
/// fn handle_error(err: CancellationError) -> MyError {
///     err.into() // Automatically converts to MyError::Cancelled
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancellationError;

impl std::fmt::Display for CancellationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Operation was cancelled")
    }
}

impl std::error::Error for CancellationError {}
