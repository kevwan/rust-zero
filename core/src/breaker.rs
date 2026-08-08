use std::{
    convert::Infallible,
    fmt,
    future::Future,
    sync::Arc,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Circuit-breaker behavior for an unreliable downstream dependency.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    pub max_failures: u32,
    pub reset_timeout: Duration,
    pub half_open_max_calls: u32,
}

impl CircuitBreakerConfig {
    pub fn new(max_failures: u32, reset_timeout: Duration) -> Self {
        assert!(
            max_failures > 0,
            "maximum failures must be greater than zero"
        );
        assert!(
            !reset_timeout.is_zero(),
            "reset timeout must be greater than zero"
        );

        Self {
            max_failures,
            reset_timeout,
            half_open_max_calls: 1,
        }
    }

    pub fn with_half_open_max_calls(mut self, calls: u32) -> Self {
        assert!(calls > 0, "half-open calls must be greater than zero");
        self.half_open_max_calls = calls;
        self
    }
}

/// Externally visible circuit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

enum State {
    Closed { consecutive_failures: u32 },
    Open { opened_at: Instant },
    HalfOpen { attempts: u32, successes: u32 },
}

impl State {
    fn status(&self) -> BreakerState {
        match self {
            Self::Closed { .. } => BreakerState::Closed,
            Self::Open { .. } => BreakerState::Open,
            Self::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }
}

/// Prevents calls to a dependency that has failed repeatedly.
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: Mutex<State>,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State::Closed {
                consecutive_failures: 0,
            }),
        }
    }

    pub fn state(&self) -> BreakerState {
        self.state
            .lock()
            .expect("circuit breaker state lock poisoned")
            .status()
    }

    /// Reserves a call for wrappers that cannot observe the result in one future.
    ///
    /// Protocol transports commonly receive the final outcome in response-body trailers. The
    /// returned permit can be held by that body and completed once the final status is known.
    /// Dropping an unfinished permit releases it as healthy, so caller cancellation does not trip
    /// the dependency circuit or leave a half-open circuit stuck forever. Wrappers should call
    /// [`CircuitBreakerPermit::finish`] explicitly for transport and protocol failures.
    pub fn acquire(self: &Arc<Self>) -> Option<CircuitBreakerPermit> {
        self.before_call::<Infallible>().ok()?;
        Some(CircuitBreakerPermit {
            breaker: Arc::clone(self),
            finished: false,
        })
    }

    /// Runs an operation if the circuit permits it.
    pub fn execute<T, E, F>(&self, operation: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        self.execute_with_accept(operation, Result::is_ok)
    }

    /// Runs an operation and uses `acceptable` to decide whether its result is
    /// healthy for circuit-breaker accounting.
    ///
    /// The original operation result is preserved. This is useful for protocols
    /// such as HTTP, where a 5xx response should trip the breaker while still
    /// being returned to the caller.
    pub fn execute_with_accept<T, E, F, A>(
        &self,
        operation: F,
        acceptable: A,
    ) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
        A: FnOnce(&Result<T, E>) -> bool,
    {
        self.before_call()?;
        let result = operation();
        self.record_acceptable(acceptable(&result));
        result.map_err(CircuitBreakerError::Operation)
    }

    /// Async counterpart to [`Self::execute`].
    pub async fn execute_async<T, E, F, Fut>(
        &self,
        operation: F,
    ) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.execute_async_with_accept(operation, Result::is_ok)
            .await
    }

    /// Async counterpart to [`Self::execute_with_accept`].
    pub async fn execute_async_with_accept<T, E, F, Fut, A>(
        &self,
        operation: F,
        acceptable: A,
    ) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        A: FnOnce(&Result<T, E>) -> bool,
    {
        self.before_call()?;
        let result = operation().await;
        self.record_acceptable(acceptable(&result));
        result.map_err(CircuitBreakerError::Operation)
    }

    fn record_acceptable(&self, acceptable: bool) {
        if acceptable {
            self.record_success();
        } else {
            self.record_failure();
        }
    }

    fn before_call<E>(&self) -> Result<(), CircuitBreakerError<E>> {
        let mut state = self
            .state
            .lock()
            .expect("circuit breaker state lock poisoned");

        if let State::Open { opened_at } = *state {
            if opened_at.elapsed() < self.config.reset_timeout {
                return Err(CircuitBreakerError::Open);
            }
            *state = State::HalfOpen {
                attempts: 0,
                successes: 0,
            };
        }

        if let State::HalfOpen { attempts, .. } = &mut *state {
            if *attempts >= self.config.half_open_max_calls {
                return Err(CircuitBreakerError::Open);
            }
            *attempts += 1;
        }

        Ok(())
    }

    fn record_success(&self) {
        let mut state = self
            .state
            .lock()
            .expect("circuit breaker state lock poisoned");
        match &mut *state {
            State::Closed {
                consecutive_failures,
            } => *consecutive_failures = 0,
            State::Open { .. } => {}
            State::HalfOpen {
                attempts: _,
                successes,
            } => {
                *successes += 1;
                if *successes == self.config.half_open_max_calls {
                    *state = State::Closed {
                        consecutive_failures: 0,
                    };
                }
            }
        }
    }

    fn record_failure(&self) {
        let mut state = self
            .state
            .lock()
            .expect("circuit breaker state lock poisoned");
        match &mut *state {
            State::Closed {
                consecutive_failures,
            } => {
                *consecutive_failures += 1;
                if *consecutive_failures >= self.config.max_failures {
                    *state = State::Open {
                        opened_at: Instant::now(),
                    };
                }
            }
            State::Open { .. } => {}
            State::HalfOpen { .. } => {
                *state = State::Open {
                    opened_at: Instant::now(),
                };
            }
        }
    }
}

/// An admitted circuit-breaker call whose outcome may arrive later.
pub struct CircuitBreakerPermit {
    breaker: Arc<CircuitBreaker>,
    finished: bool,
}

impl CircuitBreakerPermit {
    /// Records whether the dependency outcome was healthy.
    pub fn finish(mut self, acceptable: bool) {
        self.breaker.record_acceptable(acceptable);
        self.finished = true;
    }
}

impl Drop for CircuitBreakerPermit {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker.record_success();
        }
    }
}

/// An operation error or the rejection produced by an open circuit.
#[derive(Debug, PartialEq, Eq)]
pub enum CircuitBreakerError<E> {
    Open,
    Operation(E),
}

impl<E: fmt::Display> fmt::Display for CircuitBreakerError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => formatter.write_str("circuit breaker is open"),
            Self::Operation(error) => write!(formatter, "protected operation failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for CircuitBreakerError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open => None,
            Self::Operation(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn opens_after_the_configured_failure_threshold() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig::new(2, Duration::from_secs(1)));

        assert_eq!(
            breaker.execute(|| Err::<(), _>("first")),
            Err(CircuitBreakerError::Operation("first"))
        );
        assert_eq!(
            breaker.execute(|| Err::<(), _>("second")),
            Err(CircuitBreakerError::Operation("second"))
        );
        assert_eq!(breaker.state(), BreakerState::Open);
        assert_eq!(
            breaker.execute(|| Ok::<_, ()>(())),
            Err(CircuitBreakerError::Open)
        );
    }

    #[test]
    fn closes_after_a_successful_half_open_probe() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig::new(1, Duration::from_millis(5)));

        let _ = breaker.execute(|| Err::<(), _>("unavailable"));
        thread::sleep(Duration::from_millis(10));

        assert_eq!(breaker.execute(|| Ok::<_, ()>(42)), Ok(42));
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[tokio::test]
    async fn async_acceptance_can_reject_a_successful_result() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig::new(1, Duration::from_secs(1)));

        let response = breaker
            .execute_async_with_accept(
                || async { Ok::<_, ()>(503) },
                |result| result.as_ref().is_ok_and(|status| *status < 500),
            )
            .await;

        assert_eq!(response, Ok(503));
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    #[test]
    fn dropped_transport_permit_does_not_treat_caller_cancellation_as_failure() {
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::new(
            1,
            Duration::from_secs(1),
        )));

        drop(breaker.acquire().unwrap());

        assert_eq!(breaker.state(), BreakerState::Closed);
        breaker.acquire().unwrap().finish(false);
        assert_eq!(breaker.state(), BreakerState::Open);
    }
}
