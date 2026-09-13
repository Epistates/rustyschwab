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
use parking_lot::Mutex;
use std::future::Future;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

struct State {
    kind: CircuitState,
    generation: u64,
    failures: u32,
    successes: u32,
    probes: u32,
    opened: Option<Instant>,
}

pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: Mutex<State>,
}

// Returning or cancelling the future releases its probe. Old completions
// cannot modify a newer recovery generation.
struct Permit<'a> {
    breaker: &'a CircuitBreaker,
    generation: u64,
    probe: bool,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut s = self.breaker.state.lock();
        if self.probe && s.generation == self.generation {
            s.probes = s.probes.saturating_sub(1);
        }
    }
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State {
                kind: CircuitState::Closed,
                generation: 0,
                failures: 0,
                successes: 0,
                probes: 0,
                opened: None,
            }),
        }
    }
    pub fn state(&self) -> CircuitState {
        self.state.lock().kind
    }
    pub fn failure_count(&self) -> u32 {
        self.state.lock().failures
    }
    fn transition(s: &mut State, next: CircuitState) {
        let from = s.kind;
        s.kind = next;
        s.generation = s.generation.wrapping_add(1);
        s.probes = 0;
        s.successes = 0;
        s.opened = if next == CircuitState::Open {
            Some(Instant::now())
        } else {
            None
        };
        if next == CircuitState::Closed {
            s.failures = 0;
        }
        if from != next {
            record_transition(from, next);
        }
    }
    fn ready(&self, s: &mut State) -> bool {
        if !self.config.enabled {
            return true;
        }
        if s.kind == CircuitState::Open
            && s.opened
                .is_some_and(|t| t.elapsed() >= self.config.open_duration)
        {
            Self::transition(s, CircuitState::HalfOpen);
        }
        s.kind == CircuitState::Closed
            || (s.kind == CircuitState::HalfOpen
                && s.probes < self.config.half_open_max_requests.max(1))
    }
    /// Advisory check; does not consume a probe. `call` atomically reserves it.
    pub fn is_allowed(&self) -> bool {
        self.ready(&mut self.state.lock())
    }
    pub async fn call<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let permit = {
            let mut s = self.state.lock();
            if !self.ready(&mut s) {
                return Err(Error::CircuitOpen);
            }
            let probe = self.config.enabled && s.kind == CircuitState::HalfOpen;
            if probe {
                s.probes += 1;
            }
            Permit {
                breaker: self,
                generation: s.generation,
                probe,
            }
        };
        let result = f().await;
        // A permanent HTTP rejection is evidence that the service responded.
        // Local validation/auth failures are neutral; the permit still releases.
        let success = result.is_ok()
            || result.as_ref().err().is_some_and(|e| {
                e.http_status().is_some_and(|s| s.is_client_error()) && !e.is_retryable()
            });
        let failure = result.as_ref().err().is_some_and(Error::is_retryable);
        self.record(permit.generation, success, failure);
        result
    }
    fn record(&self, generation: u64, success: bool, failure: bool) {
        if !self.config.enabled {
            return;
        }
        let mut s = self.state.lock();
        if s.generation != generation {
            return;
        }
        match s.kind {
            CircuitState::Closed if failure => {
                s.failures += 1;
                if s.failures >= self.config.failure_threshold.max(1) {
                    Self::transition(&mut s, CircuitState::Open);
                }
            }
            CircuitState::Closed if success => s.failures = 0,
            CircuitState::HalfOpen if failure => Self::transition(&mut s, CircuitState::Open),
            CircuitState::HalfOpen if success => {
                s.successes += 1;
                if s.successes >= self.config.success_threshold.max(1) {
                    Self::transition(&mut s, CircuitState::Closed);
                }
            }
            _ => {}
        }
    }
    #[cfg(test)]
    fn record_failure(&self) {
        let g = self.state.lock().generation;
        self.record(g, false, true);
    }
    #[cfg(test)]
    fn record_success(&self) {
        let g = self.state.lock().generation;
        self.record(g, true, false);
    }
    pub fn reset(&self) {
        Self::transition(&mut self.state.lock(), CircuitState::Closed);
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
        assert!(matches!(result, Err(Error::CircuitOpen)));
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
