use crate::error::CancellationError;
use log::info;
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Extension trait for adding cancellation support to any Future
///
/// This trait allows you to easily add cancellation support to any future that returns
/// a `Result`, eliminating the need for repetitive `tokio::select!` boilerplate code.
///
/// The trait is implemented for all futures that return `Result<T, E>` where both
/// the original error `E` and `CancellationError` can be converted to the target error type.
///
/// # Type Parameters
///
/// - `T`: The success type of the future's result
///
/// # Examples
///
/// ## Basic Usage
///
/// ```rust
/// use tokio_cancellation_ext::{CancellationExt, CancellationError};
/// use tokio_util::sync::CancellationToken;
///
/// #[derive(Debug)]
/// enum MyError {
///     Cancelled,
///     Other(String),
/// }
///
/// impl From<CancellationError> for MyError {
///     fn from(_: CancellationError) -> Self {
///         MyError::Cancelled
///     }
/// }
///
/// impl From<std::io::Error> for MyError {
///     fn from(e: std::io::Error) -> Self {
///         MyError::Other(e.to_string())
///     }
/// }
///
/// # async fn some_io_operation() -> Result<String, std::io::Error> {
/// #     Ok("success".to_string())
/// # }
/// # #[tokio::main]
/// # async fn main() -> Result<(), MyError> {
/// let token = CancellationToken::new();
///
/// let result = some_io_operation()
///     .with_cancellation::<MyError>(&token, "io_operation")
///     .await?;
///
/// println!("Got result: {}", result);
/// # Ok(())
/// # }
/// ```
///
/// ## Chaining Multiple Operations
///
/// ```rust
/// use tokio_cancellation_ext::{CancellationExt, CancellationError};
/// use tokio_util::sync::CancellationToken;
///
/// #[derive(Debug)]
/// enum AppError {
///     Cancelled,
///     Network(String),
///     Database(String),
/// }
///
/// impl From<CancellationError> for AppError {
///     fn from(_: CancellationError) -> Self {
///         AppError::Cancelled
///     }
/// }
///
/// # async fn fetch_user(id: u32) -> Result<String, AppError> { Ok("user".to_string()) }
/// # async fn update_cache(data: &str) -> Result<(), AppError> { Ok(()) }
/// # #[tokio::main]
/// # async fn main() -> Result<(), AppError> {
/// let token = CancellationToken::new();
/// let user_id = 42;
///
/// // Both operations can be cancelled
/// let user = fetch_user(user_id)
///     .with_cancellation::<AppError>(&token, "fetch_user")
///     .await?;
///
/// update_cache(&user)
///     .with_cancellation::<AppError>(&token, "update_cache")
///     .await?;
/// # Ok(())
/// # }
/// ```
pub trait CancellationExt<T> {
    /// The original error type from the future
    ///
    /// This associated type represents the error type that the original future
    /// can return. It must be convertible to the target error type `E`.
    type OriginalError;

    /// Adds cancellation support to a future
    ///
    /// This method wraps the future with cancellation logic, allowing it to be
    /// interrupted when the provided `CancellationToken` is cancelled.
    ///
    /// # Arguments
    ///
    /// * `token` - The cancellation token to listen for cancellation signals
    /// * `context` - A string describing the operation for logging purposes.
    ///   This will be included in log messages when cancellation occurs.
    ///
    /// # Returns
    ///
    /// Returns a new future that will either:
    /// - Complete with `Ok(T)` if the original future completes successfully
    /// - Complete with `Err(E)` if the original future returns an error (converted via `Into`)
    /// - Complete with `Err(E)` if cancellation is requested (from `CancellationError`)
    ///
    /// # Type Constraints
    ///
    /// - `CancellationError: Into<E>` - The cancellation error must be convertible to `E`
    /// - `Self::OriginalError: Into<E>` - The original error must be convertible to `E`
    /// - `Self: 'a` - The future must live at least as long as the references
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio_cancellation_ext::{CancellationExt, CancellationError};
    /// use tokio_util::sync::CancellationToken;
    ///
    /// #[derive(Debug)]
    /// enum DatabaseError {
    ///     Cancelled,
    ///     Connection(String),
    ///     Query(String),
    /// }
    ///
    /// impl From<CancellationError> for DatabaseError {
    ///     fn from(_: CancellationError) -> Self {
    ///         DatabaseError::Cancelled
    ///     }
    /// }
    ///
    /// # async fn execute_query() -> Result<Vec<String>, DatabaseError> {
    /// #     Ok(vec!["row1".to_string(), "row2".to_string()])
    /// # }
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), DatabaseError> {
    /// let token = CancellationToken::new();
    ///
    /// let rows = execute_query()
    ///     .with_cancellation::<DatabaseError>(&token, "execute_query")
    ///     .await?;
    ///
    /// println!("Got {} rows", rows.len());
    /// # Ok(())
    /// # }
    /// ```
    fn with_cancellation<'a, E>(
        self,
        token: &'a CancellationToken,
        context: &'a str,
    ) -> impl Future<Output = Result<T, E>> + Send + 'a
    where
        CancellationError: Into<E>,
        Self::OriginalError: Into<E>,
        Self: 'a;
}

#[allow(clippy::manual_async_fn)] // Complex lifetime bounds make async fn impractical here
impl<F, T, OriginalError> CancellationExt<T> for F
where
    F: Future<Output = Result<T, OriginalError>> + Send,
{
    type OriginalError = OriginalError;

    fn with_cancellation<'a, E>(
        self,
        token: &'a CancellationToken,
        context: &'a str,
    ) -> impl Future<Output = Result<T, E>> + Send + 'a
    where
        CancellationError: Into<E>,
        OriginalError: Into<E>,
        F: 'a,
    {
        async move {
            let context_owned = context.to_string();
            tokio::select! {
                _ = token.cancelled() => {
                    info!("{}: cancellation signal received", context_owned);
                    Err(CancellationError.into())
                }
                result = self => {
                    result.map_err(Into::into)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Debug, PartialEq)]
    enum TestError {
        Cancelled,
        Custom(String),
    }

    impl From<CancellationError> for TestError {
        fn from(_: CancellationError) -> Self {
            TestError::Cancelled
        }
    }

    impl From<std::io::Error> for TestError {
        fn from(e: std::io::Error) -> Self {
            TestError::Custom(e.to_string())
        }
    }

    #[tokio::test]
    async fn test_successful_operation() {
        let token = CancellationToken::new();

        async fn success_operation() -> Result<String, std::io::Error> {
            Ok("success".to_string())
        }

        let result: Result<String, TestError> =
            success_operation().with_cancellation(&token, "test").await;

        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_cancellation() {
        let token = CancellationToken::new();

        async fn long_operation() -> Result<String, std::io::Error> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok("should not reach here".to_string())
        }

        // Cancel the token immediately
        token.cancel();

        let result: Result<String, TestError> = long_operation()
            .with_cancellation(&token, "test_cancellation")
            .await;

        assert_eq!(result.unwrap_err(), TestError::Cancelled);
    }

    #[tokio::test]
    async fn test_original_error_propagation() {
        let token = CancellationToken::new();

        async fn failing_operation() -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "test error"))
        }

        let result: Result<String, TestError> =
            failing_operation().with_cancellation(&token, "test").await;

        match result.unwrap_err() {
            TestError::Custom(msg) => assert!(msg.contains("test error")),
            _ => panic!("Expected Custom error"),
        }
    }
}
