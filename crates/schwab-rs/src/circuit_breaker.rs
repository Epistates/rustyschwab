//! Circuit breaker pattern for cascading failure prevention.
//!
//! This module implements a three-state circuit breaker following the standard pattern:
//!
//! ```text
//!                    ┌───────────┐
//!            success │           │ failure threshold
//!       ┌────────────┤  Closed   ├─────────────┐
//!       │            │           │             │
//!       │            └───────────┘             │
//!       │                                      ▼
//!  ┌────┴────┐                           ┌─────────┐
//!  │         │                           │         │
//!  │ Closed  │◄──success threshold───────┤  Open   │
//!  │         │                           │         │
//!  └────┬────┘                           └────┬────┘
//!       │                                     │
//!       │ failure       ┌───────────┐         │ timeout
//!       └──────────────►│ Half-Open │◄────────┘
//!                       └───────────┘
//! ```
//!
//! # States
//!
//! - **Closed**: Normal operation. Requests pass through. Failures are counted.
//! - **Open**: Circuit is tripped. Requests fail immediately without execution.
//! - **Half-Open**: Testing state. Limited requests allowed to probe recovery.
//!
//! # Example
//!
//! ```ignore
//! use schwab_rs::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
//!
//! let config = CircuitBreakerConfig::default();
//! let breaker = CircuitBreaker::new(config);
//!
//! // Execute with circuit breaker protection
//! let result = breaker.call(|| async {
//!     // Your fallible operation here
//!     Ok::<_, Error>(response)
//! }).await;
//! ```

use crate::error::{Error, Result};
use parking_lot::RwLock;
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Whether the circuit breaker is enabled. Default: true
    pub enabled: bool,
    /// Number of failures before opening the circuit. Default: 5
    pub failure_threshold: u32,
    /// Number of successes in half-open state to close the circuit. Default: 3
    pub success_threshold: u32,
    /// Duration to wait in open state before transitioning to half-open. Default: 30s
    pub open_duration: Duration,
    /// Maximum number of requests to allow through in half-open state. Default: 1
    pub half_open_max_requests: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            success_threshold: 3,
            open_duration: Duration::from_secs(30),
            half_open_max_requests: 1,
        }
    }
}

/// The current state of the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation - requests pass through
    Closed,
    /// Circuit is open - requests fail immediately
    Open,
    /// Testing recovery - limited requests allowed
    HalfOpen,
}

/// Thread-safe circuit breaker with atomic operations.
///
/// The circuit breaker protects downstream services from cascading failures
/// by failing fast when the service is unhealthy.
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: RwLock<CircuitState>,
    failure_count: AtomicU32,
    success_count: AtomicU32,
    half_open_requests: AtomicU32,
    last_failure_time: AtomicU64,
}

impl CircuitBreaker {
    /// Creates a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: RwLock::new(CircuitState::Closed),
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            half_open_requests: AtomicU32::new(0),
            last_failure_time: AtomicU64::new(0),
        }
    }

    /// Returns the current state of the circuit breaker.
    pub fn state(&self) -> CircuitState {
        *self.state.read()
    }

    /// Returns the current failure count.
    pub fn failure_count(&self) -> u32 {
        self.failure_count.load(Ordering::Relaxed)
    }

    /// Returns whether the circuit breaker allows requests.
    ///
    /// This method handles state transitions automatically:
    /// - Open → HalfOpen after timeout
    pub fn is_allowed(&self) -> bool {
        if !self.config.enabled {
            return true;
        }

        let state = *self.state.read();
        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if we should transition to half-open
                let elapsed = self.elapsed_since_failure();
                if elapsed >= self.config.open_duration {
                    self.transition_to_half_open();
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                // Allow limited requests in half-open state
                let current = self.half_open_requests.fetch_add(1, Ordering::SeqCst);
                current < self.config.half_open_max_requests
            }
        }
    }

    /// Executes the given future with circuit breaker protection.
    ///
    /// If the circuit is open, returns an error immediately without executing the future.
    /// Records success/failure and manages state transitions.
    pub async fn call<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if !self.is_allowed() {
            return Err(Error::Http {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                message: "Circuit breaker is open".to_string(),
            });
        }

        match f().await {
            Ok(result) => {
                self.record_success();
                Ok(result)
            }
            Err(e) => {
                if e.is_retryable() {
                    self.record_failure();
                }
                Err(e)
            }
        }
    }

    /// Records a successful operation.
    fn record_success(&self) {
        if !self.config.enabled {
            return;
        }

        let state = *self.state.read();
        match state {
            CircuitState::Closed => {
                // Reset failure count on success
                self.failure_count.store(0, Ordering::Relaxed);
            }
            CircuitState::HalfOpen => {
                let count = self.success_count.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.config.success_threshold {
                    self.transition_to_closed();
                }
            }
            CircuitState::Open => {
                // Shouldn't happen, but handle gracefully
            }
        }
    }

    /// Records a failed operation.
    fn record_failure(&self) {
        if !self.config.enabled {
            return;
        }

        let state = *self.state.read();
        match state {
            CircuitState::Closed => {
                let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
                if count >= self.config.failure_threshold {
                    self.transition_to_open();
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open goes back to open
                self.transition_to_open();
            }
            CircuitState::Open => {
                // Already open, update timestamp
                self.update_failure_time();
            }
        }
    }

    fn transition_to_open(&self) {
        let mut state = self.state.write();
        *state = CircuitState::Open;
        self.update_failure_time();
        log::warn!(
            "Circuit breaker opened after {} failures",
            self.failure_count.load(Ordering::Relaxed)
        );
    }

    fn transition_to_half_open(&self) {
        let mut state = self.state.write();
        if *state == CircuitState::Open {
            *state = CircuitState::HalfOpen;
            self.half_open_requests.store(0, Ordering::Relaxed);
            self.success_count.store(0, Ordering::Relaxed);
            log::info!("Circuit breaker transitioned to half-open");
        }
    }

    fn transition_to_closed(&self) {
        let mut state = self.state.write();
        *state = CircuitState::Closed;
        self.failure_count.store(0, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        self.half_open_requests.store(0, Ordering::Relaxed);
        log::info!("Circuit breaker closed - service recovered");
    }

    fn update_failure_time(&self) {
        let now = Instant::now().elapsed().as_secs();
        self.last_failure_time.store(now, Ordering::Relaxed);
    }

    fn elapsed_since_failure(&self) -> Duration {
        // Use a more reliable timestamp approach
        let last_failure = self.last_failure_time.load(Ordering::Relaxed);
        if last_failure == 0 {
            return Duration::ZERO;
        }
        // For simplicity, we track elapsed seconds since program start
        // A production implementation might use SystemTime
        Duration::from_secs(Instant::now().elapsed().as_secs().saturating_sub(last_failure))
    }

    /// Manually resets the circuit breaker to closed state.
    ///
    /// Use this when you know the downstream service has recovered.
    pub fn reset(&self) {
        let mut state = self.state.write();
        *state = CircuitState::Closed;
        self.failure_count.store(0, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        self.half_open_requests.store(0, Ordering::Relaxed);
        self.last_failure_time.store(0, Ordering::Relaxed);
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_starts_closed() {
        let breaker = CircuitBreaker::default();
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert!(breaker.is_allowed());
    }

    #[test]
    fn test_circuit_opens_after_threshold() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        // Record failures up to threshold
        for _ in 0..3 {
            breaker.record_failure();
        }

        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(!breaker.is_allowed());
    }

    #[test]
    fn test_success_resets_failure_count() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        // Record some failures
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.failure_count(), 2);

        // Success should reset
        breaker.record_success();
        assert_eq!(breaker.failure_count(), 0);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn test_disabled_breaker_always_allows() {
        let config = CircuitBreakerConfig {
            enabled: false,
            failure_threshold: 1,
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        // Even with failures, should allow
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.is_allowed());
    }

    #[test]
    fn test_manual_reset() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        // Open the circuit
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);

        // Manual reset
        breaker.reset();
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.failure_count(), 0);
        assert!(breaker.is_allowed());
    }

    #[tokio::test]
    async fn test_call_records_success() {
        let breaker = CircuitBreaker::default();

        let result: Result<i32> = breaker.call(|| async { Ok(42) }).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(breaker.failure_count(), 0);
    }

    #[tokio::test]
    async fn test_call_records_failure() {
        let config = CircuitBreakerConfig {
            failure_threshold: 5,
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        let result: Result<i32> = breaker
            .call(|| async { Err(Error::ConnectionClosed) })
            .await;

        assert!(result.is_err());
        assert_eq!(breaker.failure_count(), 1);
    }

    #[tokio::test]
    async fn test_open_circuit_fails_fast() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            open_duration: Duration::from_secs(60), // Long timeout for test
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        // Open the circuit
        breaker.record_failure();
        breaker.record_failure();

        // Should fail immediately without executing
        let result: Result<i32> = breaker.call(|| async { Ok(42) }).await;

        assert!(result.is_err());
        if let Err(Error::Http { status, message }) = result {
            assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
            assert!(message.contains("Circuit breaker is open"));
        } else {
            panic!("Expected Http error");
        }
    }
}
