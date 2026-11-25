//! Retry policy with exponential backoff and configurable behavior.
//!
//! This module provides a flexible, composable retry mechanism with manual control.
//! It supports:
//! - Exponential backoff with configurable multiplier and bounds
//! - Status code-based retry decisions (retryable vs permanent failures)
//! - Rate limit handling with explicit `Retry-After` headers
//! - Maximum retry attempts

use crate::config::RetryConfig;
use crate::error::{Error, Result};
use rand::RngExt;
use std::future::Future;
use std::time::Duration;
use tracing::debug;

/// Implements retry logic with exponential backoff and intelligent error handling.
///
/// The retry policy respects:
/// - `Retry-After` headers from the server (takes precedence)
/// - Exponential backoff for other transient errors
/// - Maximum retry count limits
/// - Status codes that are retryable (500, 502, 503, etc.)
///
/// # Example
/// ```no_run
/// use rustyschwab::Error;
/// use rustyschwab::config::RetryConfig;
/// use rustyschwab::retry::RetryPolicy;
/// use std::time::Duration;
///
/// # async fn run() {
/// let config = RetryConfig {
///     max_retries: 3,
///     initial_backoff: Duration::from_secs(1),
///     max_backoff: Duration::from_secs(30),
///     backoff_multiplier: 2.0,
///     retry_on_status: vec![500, 502, 503],
/// };
///
/// let policy = RetryPolicy::new(&config);
/// let result = policy
///     .execute(|| async {
///         // Operation that might fail
///         Ok::<_, Error>(42)
///     })
///     .await;
/// # let _ = result;
/// # }
/// ```
/// The least wait before honoring a `Retry-After` of zero or a past date.
const MIN_RETRY_AFTER: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    config: RetryConfig,
}

impl RetryPolicy {
    /// Creates a new retry policy with the given configuration.
    pub fn new(config: &RetryConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }

    /// Executes the given operation with exponential backoff retry logic.
    ///
    /// # Arguments
    /// * `f` - Async function that returns `Result<T>`
    ///
    /// # Returns
    /// * `Ok(T)` - If operation succeeds
    /// * `Err(Error)` - If all retries exhausted or error is non-retryable
    pub async fn execute<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.execute_until(None, f).await
    }

    /// As [`execute`](Self::execute), but never waits past `deadline`: a
    /// retry that could not start before it returns the error that asked
    /// for it. A rate limit whose `Retry-After` outlasts the caller's
    /// deadline is then reported as the rate limit it is, not as the
    /// timeout the wait would have become.
    pub async fn execute_until<F, Fut, T>(
        &self,
        deadline: Option<tokio::time::Instant>,
        f: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.execute_until_recording(deadline, &parking_lot::Mutex::new(None), f)
            .await
    }

    /// As [`execute_until`](Self::execute_until), and each error it retries
    /// over is moved into `last_failure` before the wait, replacing the one
    /// before. A caller that drops this future at its deadline can then
    /// still say what the last finished attempt failed with.
    pub(crate) async fn execute_until_recording<F, Fut, T>(
        &self,
        deadline: Option<tokio::time::Instant>,
        last_failure: &parking_lot::Mutex<Option<Error>>,
        mut f: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut retries = 0;

        loop {
            match f().await {
                Ok(result) => {
                    if retries > 0 {
                        metrics::counter!(
                            crate::metrics::RETRY_ATTEMPTS_TOTAL,
                            "outcome" => "succeeded",
                        )
                        .increment(retries as u64);
                    }
                    return Ok(result);
                }
                Err(error) => {
                    if !self.should_retry(&error, retries) {
                        debug!(
                            "Not retrying (attempt {} of {})",
                            retries, self.config.max_retries
                        );
                        // Attribute any retries we *did* make to the
                        // exhausted bucket — if `retries == 0` this is
                        // a first-attempt non-retryable error and we
                        // record nothing.
                        if retries > 0 {
                            metrics::counter!(
                                crate::metrics::RETRY_ATTEMPTS_TOTAL,
                                "outcome" => "exhausted",
                            )
                            .increment(retries as u64);
                        }
                        return Err(error);
                    }

                    // Check for explicit Retry-After header (takes precedence).
                    // A zero or past value still waits a moment, so a
                    // server that says "now" cannot be hammered.
                    let delay = if let Some(retry_after) = error.retry_after() {
                        debug!(
                            "Rate limited by server, waiting {}s (attempt {}/{})",
                            retry_after.as_secs(),
                            retries + 1,
                            self.config.max_retries
                        );
                        retry_after.max(MIN_RETRY_AFTER)
                    } else {
                        let delay = self.calculate_backoff(retries);
                        debug!(
                            "Retrying in {:?} (attempt {}/{})",
                            delay,
                            retries + 1,
                            self.config.max_retries
                        );
                        delay
                    };

                    if deadline.is_some_and(|d| tokio::time::Instant::now() + delay >= d) {
                        debug!("Retry would outlast the caller's deadline; giving up");
                        return Err(error);
                    }
                    *last_failure.lock() = Some(error);
                    tokio::time::sleep(delay).await;
                    retries += 1;
                }
            }
        }
    }

    /// Determines if an error should be retried based on error type and attempt count.
    fn should_retry(&self, error: &Error, retries: u32) -> bool {
        // Check retry budget first
        if retries >= self.config.max_retries {
            return false;
        }

        match error {
            // Retry on specific HTTP status codes
            Error::Http { status, .. } | Error::Broker { status, .. } => {
                self.config.retry_on_status.contains(&status.as_u16())
            }
            // Use generic retryability for other error types
            _ => error.is_retryable(),
        }
    }

    /// Calculates the exponential backoff delay with full jitter for the given retry attempt.
    ///
    /// Uses the "Full Jitter" algorithm recommended by AWS:
    /// `sleep = random_between(0, min(cap, base * 2^attempt))`
    ///
    /// This provides better spread of retry storms compared to equal jitter or no jitter.
    /// See: https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/
    ///
    /// # Arguments
    /// * `attempt` - The zero-indexed retry attempt number
    fn calculate_backoff(&self, attempt: u32) -> Duration {
        let multiplier = self.config.backoff_multiplier;
        let initial = self.config.initial_backoff.as_secs_f64();

        // Calculate exponential: initial * (multiplier ^ attempt)
        let exp = multiplier.powi(attempt as i32);
        let base_delay = initial * exp;

        // Cap at max_backoff
        let max_secs = self.config.max_backoff.as_secs_f64();
        let capped_delay = base_delay.min(max_secs);

        // Apply full jitter: random value between 0 and calculated delay
        // This prevents thundering herd by spreading retries across the interval
        let mut rng = rand::rng();
        let jittered_secs = rng.random_range(0.0..=capped_delay);

        Duration::from_secs_f64(jittered_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetryConfig;

    /// Test successful retry after transient failures.
    #[tokio::test]
    async fn test_retry_on_retryable_error() {
        let config = RetryConfig {
            max_retries: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            retry_on_status: vec![500, 502, 503],
        };

        let policy = RetryPolicy::new(&config);
        let mut attempts = 0;

        let result: Result<i32> = policy
            .execute(|| {
                attempts += 1;
                async move {
                    if attempts < 3 {
                        // Use ConnectionClosed which is a retryable error
                        Err(Error::ConnectionClosed)
                    } else {
                        Ok(42)
                    }
                }
            })
            .await;

        assert!(result.is_ok(), "Should succeed after retries");
        assert_eq!(attempts, 3, "Should attempt 3 times");
    }

    /// Test that non-retryable errors fail immediately.
    #[tokio::test]
    async fn test_no_retry_on_non_retryable_error() {
        let config = RetryConfig::default();
        let policy = RetryPolicy::new(&config);
        let mut attempts = 0;

        let result: Result<i32> = policy
            .execute(|| {
                attempts += 1;
                async move { Err(Error::InvalidParameter("bad param".to_string())) }
            })
            .await;

        assert!(
            result.is_err(),
            "Should fail immediately for invalid parameter"
        );
        assert_eq!(attempts, 1, "Should only attempt once");
    }

    /// Test that max retries are respected.
    #[tokio::test]
    async fn test_max_retries_respected() {
        let config = RetryConfig {
            max_retries: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            retry_on_status: vec![500, 502, 503],
        };

        let policy = RetryPolicy::new(&config);
        let mut attempts = 0;

        let result: Result<i32> = policy
            .execute(|| {
                attempts += 1;
                async move {
                    // Always fail with a retryable error
                    Err(Error::ConnectionClosed)
                }
            })
            .await;

        assert!(result.is_err(), "Should fail after max retries");
        assert_eq!(attempts, 3, "Should attempt initial + 2 retries = 3 total");
    }

    fn http(attempt: u32) -> Error {
        Error::Http {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            message: format!("attempt {attempt}"),
        }
    }

    /// Each error retried over is handed to the slot, the latest replacing
    /// the one before, so a caller that drops the call at its deadline can
    /// still read the last attempt that finished.
    #[tokio::test(start_paused = true)]
    async fn test_retried_errors_are_recorded_for_the_deadline() {
        let policy = RetryPolicy::new(&RetryConfig {
            max_retries: 5,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 1.0,
            retry_on_status: vec![503],
        });
        let last_failure = parking_lot::Mutex::new(None);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut attempts = 0;

        let result = tokio::time::timeout_at(
            deadline,
            policy.execute_until_recording(Some(deadline), &last_failure, || {
                attempts += 1;
                let attempt = attempts;
                async move {
                    if attempt < 3 {
                        Err::<i32, _>(http(attempt))
                    } else {
                        std::future::pending().await
                    }
                }
            }),
        )
        .await;

        assert!(result.is_err(), "the third attempt is still in flight");
        let recorded = last_failure.lock().take();
        assert!(
            matches!(&recorded, Some(Error::Http { message, .. }) if message == "attempt 2"),
            "{recorded:?}"
        );
    }

    /// A retry that would outlast the deadline is not started: the error
    /// that asked for it comes back itself, and the slot stays empty.
    #[tokio::test(start_paused = true)]
    async fn test_giving_up_before_the_deadline_returns_the_error_itself() {
        let policy = RetryPolicy::new(&RetryConfig::default());
        let last_failure = parking_lot::Mutex::new(None);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

        let result: Result<i32> = policy
            .execute_until_recording(Some(deadline), &last_failure, || async {
                Err(Error::RateLimit { retry_after: 60 })
            })
            .await;

        assert!(matches!(result, Err(Error::RateLimit { retry_after: 60 })));
        assert!(last_failure.lock().is_none());
    }
}
