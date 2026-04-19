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

use crate::config::CircuitBreakerConfig;
use crate::error::{Error, Result};
use parking_lot::RwLock;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

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
    /// Wall-clock moment of the most recent failure, or `None` if the
    /// circuit has never tripped. `Instant` is monotonic and the natural
    /// fit for "how long ago did we fail" math — prior implementations
    /// used `AtomicU64` with `Instant::now().elapsed()`, which always
    /// returned ~0 and left the breaker stuck open forever.
    last_failure_time: RwLock<Option<Instant>>,
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
            last_failure_time: RwLock::new(None),
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
        let from = *state;
        *state = CircuitState::Open;
        self.update_failure_time();
        tracing::warn!(
            "Circuit breaker opened after {} failures",
            self.failure_count.load(Ordering::Relaxed)
        );
        record_transition(from, CircuitState::Open);
    }

    fn transition_to_half_open(&self) {
        let mut state = self.state.write();
        if *state == CircuitState::Open {
            *state = CircuitState::HalfOpen;
            self.half_open_requests.store(0, Ordering::Relaxed);
            self.success_count.store(0, Ordering::Relaxed);
            tracing::info!("Circuit breaker transitioned to half-open");
            record_transition(CircuitState::Open, CircuitState::HalfOpen);
        }
    }

    fn transition_to_closed(&self) {
        let mut state = self.state.write();
        let from = *state;
        *state = CircuitState::Closed;
        self.failure_count.store(0, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        self.half_open_requests.store(0, Ordering::Relaxed);
        tracing::info!("Circuit breaker closed - service recovered");
        record_transition(from, CircuitState::Closed);
    }

    fn update_failure_time(&self) {
        *self.last_failure_time.write() = Some(Instant::now());
    }

    fn elapsed_since_failure(&self) -> Duration {
        match *self.last_failure_time.read() {
            Some(ts) => ts.elapsed(),
            None => Duration::ZERO,
        }
    }

    /// Manually resets the circuit breaker to closed state.
    ///
    /// Use this when you know the downstream service has recovered.
    pub fn reset(&self) {
        let mut state = self.state.write();
        let from = *state;
        *state = CircuitState::Closed;
        self.failure_count.store(0, Ordering::Relaxed);
        self.success_count.store(0, Ordering::Relaxed);
        self.half_open_requests.store(0, Ordering::Relaxed);
        *self.last_failure_time.write() = None;
        if from != CircuitState::Closed {
            record_transition(from, CircuitState::Closed);
        }
    }
}

/// Emit the transition counter + state gauge. Split out so the
/// transition_to_* methods don't each duplicate the metric glue.
fn record_transition(from: CircuitState, to: CircuitState) {
    metrics::counter!(
        crate::metrics::CIRCUIT_TRANSITIONS_TOTAL,
        "from" => crate::metrics::circuit_state_name(from),
        "to" => crate::metrics::circuit_state_name(to),
    )
    .increment(1);
    metrics::gauge!(crate::metrics::CIRCUIT_STATE).set(crate::metrics::circuit_state_value(to));
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

    /// Regression: the previous `update_failure_time` implementation
    /// stored `Instant::now().elapsed()` (always ~0), which left the
    /// breaker stuck Open forever. Verify a short `open_duration`
    /// elapses into HalfOpen and allows a probe request through.
    #[tokio::test]
    async fn test_open_transitions_to_half_open_after_timeout() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration: Duration::from_millis(50),
            ..Default::default()
        };
        let breaker = CircuitBreaker::new(config);

        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(!breaker.is_allowed());

        tokio::time::sleep(Duration::from_millis(80)).await;

        // First is_allowed() call after timeout should transition to
        // HalfOpen and allow exactly `half_open_max_requests` probes.
        assert!(
            breaker.is_allowed(),
            "breaker should flip to HalfOpen once open_duration elapses"
        );
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
    }
}
