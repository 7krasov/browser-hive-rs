use crate::error::CancellationError;
use log::info;
use tokio_util::sync::CancellationToken;

/// Checks if cancellation was requested and returns an error if so
///
/// This is a helper function for synchronous code that needs to check for cancellation
/// at specific points (e.g., at the start of a loop iteration).
///
/// # Arguments
///
/// * `token` - The cancellation token to check
/// * `context` - A string describing where the check is being performed (for logging)
///
/// # Returns
///
/// * `Ok(())` - If cancellation was not requested
/// * `Err(CancellationError)` - If cancellation was requested
///
/// # Examples
///
/// ## Basic Usage in a Loop
///
/// ```rust
/// use tokio_cancellation_ext::{check_cancellation, CancellationError, CancellationToken};
///
/// #[derive(Debug)]
/// enum WorkerError {
///     Cancelled,
///     Other(String),
/// }
///
/// impl From<CancellationError> for WorkerError {
///     fn from(_: CancellationError) -> Self {
///         WorkerError::Cancelled
///     }
/// }
///
/// fn process_items(token: &CancellationToken, items: &[i32]) -> Result<Vec<i32>, WorkerError> {
///     let mut results = Vec::new();
///
///     for item in items {
///         // Check for cancellation at the start of each iteration
///         check_cancellation(token, "process_items_loop")?;
///
///         // Process the item
///         results.push(item * 2);
///
///         // Simulate some work
///         std::thread::sleep(std::time::Duration::from_millis(100));
///     }
///
///     Ok(results)
/// }
/// ```
///
/// ## Usage with Custom Context
///
/// ```rust
/// use tokio_cancellation_ext::{check_cancellation, CancellationError, CancellationToken};
///
/// fn wait_for_completion(token: &CancellationToken) -> Result<(), CancellationError> {
///     loop {
///         check_cancellation(token, "wait_for_completion")?;
///
///         // Check some condition
///         if is_complete() {
///             break;
///         }
///
///         std::thread::sleep(std::time::Duration::from_millis(500));
///     }
///     Ok(())
/// }
///
/// fn is_complete() -> bool {
///     // Check completion logic
///     true
/// }
/// ```
pub fn check_cancellation(
    token: &CancellationToken,
    context: &str,
) -> Result<(), CancellationError> {
    if token.is_cancelled() {
        info!("{}: cancellation detected", context);
        Err(CancellationError)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_cancelled() {
        let token = CancellationToken::new();
        let result = check_cancellation(&token, "test");
        assert!(result.is_ok());
    }

    #[test]
    fn test_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        let result = check_cancellation(&token, "test");
        assert!(result.is_err());
    }

    #[test]
    fn test_multiple_checks() {
        let token = CancellationToken::new();

        // First check - not cancelled
        assert!(check_cancellation(&token, "check1").is_ok());
        assert!(check_cancellation(&token, "check2").is_ok());

        // Cancel
        token.cancel();

        // Subsequent checks - cancelled
        assert!(check_cancellation(&token, "check3").is_err());
        assert!(check_cancellation(&token, "check4").is_err());
    }
}
