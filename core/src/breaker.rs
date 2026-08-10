use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Circuit-breaker behavior for an unreliable downstream dependency.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    pub max_failures: u32,
    pub reset_timeout: Duration,
    pub half_open_max_calls: u32,
    pub policy: CircuitBreakerPolicy,
}

impl CircuitBreakerConfig {
    /// Builds the original consecutive-failure policy.
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
            policy: CircuitBreakerPolicy::Consecutive,
        }
    }

    /// Builds a rolling adaptive breaker. Consecutive-policy fields remain available so callers
    /// can switch policies without rebuilding the rest of their client configuration.
    pub fn rolling(config: RollingCircuitBreakerConfig) -> Self {
        Self {
            max_failures: 5,
            reset_timeout: Duration::from_secs(30),
            half_open_max_calls: 1,
            policy: CircuitBreakerPolicy::Rolling(config),
        }
    }

    pub fn with_half_open_max_calls(mut self, calls: u32) -> Self {
        assert!(calls > 0, "half-open calls must be greater than zero");
        self.half_open_max_calls = calls;
        self
    }

    pub fn with_policy(mut self, policy: CircuitBreakerPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// Selects between the compatibility breaker and go-zero-style adaptive breaking.
#[derive(Debug, Clone, Copy)]
pub enum CircuitBreakerPolicy {
    /// Open after a fixed number of consecutive failures, then use bounded half-open probes.
    Consecutive,
    /// Probabilistically reject calls from recent accepted/total request history.
    Rolling(RollingCircuitBreakerConfig),
}

/// Settings for the rolling adaptive policy.
#[derive(Debug, Clone, Copy)]
pub struct RollingCircuitBreakerConfig {
    pub window: Duration,
    pub buckets: usize,
    pub sensitivity: f64,
    pub minimum_requests: u64,
    pub probe_interval: u64,
    pub random_seed: u64,
}

impl RollingCircuitBreakerConfig {
    /// Uses go-zero-equivalent defaults: a five-second, 40-bucket history, a 1.5 acceptance
    /// multiplier, and five protected observations before adaptive rejection starts.
    pub fn new() -> Self {
        Self {
            window: Duration::from_secs(5),
            buckets: 40,
            sensitivity: 1.5,
            minimum_requests: 5,
            probe_interval: 100,
            random_seed: 0,
        }
    }

    pub fn with_window(mut self, window: Duration, buckets: usize) -> Self {
        assert!(
            !window.is_zero(),
            "rolling window must be greater than zero"
        );
        assert!(
            buckets > 0,
            "rolling bucket count must be greater than zero"
        );
        assert!(
            window.as_nanos() >= buckets as u128,
            "rolling buckets must have non-zero width"
        );
        self.window = window;
        self.buckets = buckets;
        self
    }

    pub fn with_sensitivity(mut self, sensitivity: f64) -> Self {
        assert!(
            sensitivity.is_finite() && sensitivity > 0.0,
            "rolling sensitivity must be finite and greater than zero"
        );
        self.sensitivity = sensitivity;
        self
    }

    pub fn with_minimum_requests(mut self, requests: u64) -> Self {
        self.minimum_requests = requests;
        self
    }

    /// Bounds consecutive adaptive rejections so recovery always receives probe traffic.
    pub fn with_probe_interval(mut self, requests: u64) -> Self {
        assert!(requests > 0, "probe interval must be greater than zero");
        self.probe_interval = requests;
        self
    }

    /// Sets the deterministic PRNG seed. This is mainly useful for repeatable fault tests.
    pub fn with_random_seed(mut self, seed: u64) -> Self {
        self.random_seed = seed.max(1);
        self
    }

    fn validate(self) {
        assert!(
            !self.window.is_zero(),
            "rolling window must be greater than zero"
        );
        assert!(
            self.buckets > 0,
            "rolling bucket count must be greater than zero"
        );
        assert!(
            self.window.as_nanos() >= self.buckets as u128,
            "rolling buckets must have non-zero width"
        );
        assert!(
            self.sensitivity.is_finite() && self.sensitivity > 0.0,
            "rolling sensitivity must be finite and greater than zero"
        );
        assert!(
            self.probe_interval > 0,
            "probe interval must be greater than zero"
        );
    }
}

impl Default for RollingCircuitBreakerConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Externally visible circuit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// The observed completion of an admitted dependency call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitOutcome {
    Success,
    Failure,
    Cancellation,
}

/// Current rolling history and lifetime outcome counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CircuitBreakerSnapshot {
    pub accepted: u64,
    pub total: u64,
    pub drop_ratio: f64,
    pub successes: u64,
    pub failures: u64,
    pub rejections: u64,
    pub cancellations: u64,
}

enum ConsecutiveState {
    Closed { consecutive_failures: u32 },
    Open { opened_at: Instant },
    HalfOpen { attempts: u32, successes: u32 },
}

impl ConsecutiveState {
    fn status(&self) -> BreakerState {
        match self {
            Self::Closed { .. } => BreakerState::Closed,
            Self::Open { .. } => BreakerState::Open,
            Self::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }
}

enum BreakerMode {
    Consecutive(Mutex<ConsecutiveState>),
    Rolling(RollingBreaker),
}

#[derive(Default)]
struct OutcomeCounters {
    successes: AtomicU64,
    failures: AtomicU64,
    rejections: AtomicU64,
    cancellations: AtomicU64,
}

/// Prevents calls to a dependency according to the configured policy.
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    mode: BreakerMode,
    counters: OutcomeCounters,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        let mode = match config.policy {
            CircuitBreakerPolicy::Consecutive => {
                BreakerMode::Consecutive(Mutex::new(ConsecutiveState::Closed {
                    consecutive_failures: 0,
                }))
            }
            CircuitBreakerPolicy::Rolling(rolling) => {
                rolling.validate();
                BreakerMode::Rolling(RollingBreaker::new(rolling))
            }
        };
        Self {
            config,
            mode,
            counters: OutcomeCounters::default(),
        }
    }

    pub fn state(&self) -> BreakerState {
        match &self.mode {
            BreakerMode::Consecutive(state) => state
                .lock()
                .expect("circuit breaker state lock poisoned")
                .status(),
            BreakerMode::Rolling(rolling) => {
                if rolling.history().2 > 0.0 {
                    BreakerState::Open
                } else {
                    BreakerState::Closed
                }
            }
        }
    }

    pub fn snapshot(&self) -> CircuitBreakerSnapshot {
        let (accepted, total, drop_ratio) = match &self.mode {
            BreakerMode::Consecutive(_) => (0, 0, 0.0),
            BreakerMode::Rolling(rolling) => rolling.history(),
        };
        CircuitBreakerSnapshot {
            accepted,
            total,
            drop_ratio,
            successes: self.counters.successes.load(Ordering::Relaxed),
            failures: self.counters.failures.load(Ordering::Relaxed),
            rejections: self.counters.rejections.load(Ordering::Relaxed),
            cancellations: self.counters.cancellations.load(Ordering::Relaxed),
        }
    }

    /// Reserves a call for wrappers whose final outcome arrives in response trailers.
    /// Dropping an unfinished permit records caller cancellation without changing breaker health.
    pub fn acquire(self: &Arc<Self>) -> Option<CircuitBreakerPermit> {
        if self.before_call::<Infallible>().is_err() {
            return None;
        }
        Some(CircuitBreakerPermit {
            breaker: Arc::clone(self),
            finished: false,
        })
    }

    pub fn execute<T, E, F>(&self, operation: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        self.execute_with_accept(operation, Result::is_ok)
    }

    pub fn execute_with_accept<T, E, F, A>(
        &self,
        operation: F,
        acceptable: A,
    ) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
        A: FnOnce(&Result<T, E>) -> bool,
    {
        self.execute_with_outcome(operation, |result| {
            if acceptable(result) {
                CircuitOutcome::Success
            } else {
                CircuitOutcome::Failure
            }
        })
    }

    pub fn execute_with_outcome<T, E, F, A>(
        &self,
        operation: F,
        outcome: A,
    ) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
        A: FnOnce(&Result<T, E>) -> CircuitOutcome,
    {
        self.before_call()?;
        let mut completion = CompletionGuard::new(self);
        let result = operation();
        completion.finish(outcome(&result));
        result.map_err(CircuitBreakerError::Operation)
    }

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
        self.execute_async_with_outcome(operation, |result| {
            if acceptable(result) {
                CircuitOutcome::Success
            } else {
                CircuitOutcome::Failure
            }
        })
        .await
    }

    pub async fn execute_async_with_outcome<T, E, F, Fut, A>(
        &self,
        operation: F,
        outcome: A,
    ) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        A: FnOnce(&Result<T, E>) -> CircuitOutcome,
    {
        self.before_call()?;
        let mut completion = CompletionGuard::new(self);
        let result = operation().await;
        completion.finish(outcome(&result));
        result.map_err(CircuitBreakerError::Operation)
    }

    fn before_call<E>(&self) -> Result<(), CircuitBreakerError<E>> {
        let admitted = match &self.mode {
            BreakerMode::Consecutive(state) => self.before_consecutive(state),
            BreakerMode::Rolling(rolling) => !rolling.should_drop(),
        };
        if admitted {
            Ok(())
        } else {
            self.counters.rejections.fetch_add(1, Ordering::Relaxed);
            Err(CircuitBreakerError::Open)
        }
    }

    fn before_consecutive(&self, state: &Mutex<ConsecutiveState>) -> bool {
        let mut state = state.lock().expect("circuit breaker state lock poisoned");
        if let ConsecutiveState::Open { opened_at } = *state {
            if opened_at.elapsed() < self.config.reset_timeout {
                return false;
            }
            *state = ConsecutiveState::HalfOpen {
                attempts: 0,
                successes: 0,
            };
        }
        if let ConsecutiveState::HalfOpen { attempts, .. } = &mut *state {
            if *attempts >= self.config.half_open_max_calls {
                return false;
            }
            *attempts += 1;
        }
        true
    }

    fn record_outcome(&self, outcome: CircuitOutcome) {
        match outcome {
            CircuitOutcome::Success => {
                self.counters.successes.fetch_add(1, Ordering::Relaxed);
            }
            CircuitOutcome::Failure => {
                self.counters.failures.fetch_add(1, Ordering::Relaxed);
            }
            CircuitOutcome::Cancellation => {
                self.counters.cancellations.fetch_add(1, Ordering::Relaxed);
            }
        }
        match &self.mode {
            BreakerMode::Consecutive(state) => self.record_consecutive(state, outcome),
            BreakerMode::Rolling(rolling) => rolling.record(outcome),
        }
    }

    fn record_consecutive(&self, state: &Mutex<ConsecutiveState>, outcome: CircuitOutcome) {
        let mut state = state.lock().expect("circuit breaker state lock poisoned");
        match outcome {
            CircuitOutcome::Success => match &mut *state {
                ConsecutiveState::Closed {
                    consecutive_failures,
                } => *consecutive_failures = 0,
                ConsecutiveState::Open { .. } => {}
                ConsecutiveState::HalfOpen { successes, .. } => {
                    *successes += 1;
                    if *successes == self.config.half_open_max_calls {
                        *state = ConsecutiveState::Closed {
                            consecutive_failures: 0,
                        };
                    }
                }
            },
            CircuitOutcome::Failure => match &mut *state {
                ConsecutiveState::Closed {
                    consecutive_failures,
                } => {
                    *consecutive_failures += 1;
                    if *consecutive_failures >= self.config.max_failures {
                        *state = ConsecutiveState::Open {
                            opened_at: Instant::now(),
                        };
                    }
                }
                ConsecutiveState::Open { .. } => {}
                ConsecutiveState::HalfOpen { .. } => {
                    *state = ConsecutiveState::Open {
                        opened_at: Instant::now(),
                    };
                }
            },
            CircuitOutcome::Cancellation => {
                if let ConsecutiveState::HalfOpen { attempts, .. } = &mut *state {
                    *attempts = attempts.saturating_sub(1);
                }
            }
        }
    }
}

struct CompletionGuard<'a> {
    breaker: &'a CircuitBreaker,
    finished: bool,
}

impl<'a> CompletionGuard<'a> {
    fn new(breaker: &'a CircuitBreaker) -> Self {
        Self {
            breaker,
            finished: false,
        }
    }

    fn finish(&mut self, outcome: CircuitOutcome) {
        self.breaker.record_outcome(outcome);
        self.finished = true;
    }
}

impl Drop for CompletionGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker.record_outcome(CircuitOutcome::Cancellation);
        }
    }
}

struct RollingBreaker {
    config: RollingCircuitBreakerConfig,
    bucket_width: Duration,
    state: Mutex<RollingState>,
    random: AtomicU64,
    drop_sequence: AtomicU64,
}

impl RollingBreaker {
    fn new(config: RollingCircuitBreakerConfig) -> Self {
        static NEXT_SEED: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
        let bucket_width = Duration::from_nanos(
            u64::try_from(config.window.as_nanos() / config.buckets as u128)
                .unwrap_or(u64::MAX)
                .max(1),
        );
        Self {
            config,
            bucket_width,
            state: Mutex::new(RollingState {
                current_started: Instant::now(),
                buckets: VecDeque::from([OutcomeBucket::default()]),
            }),
            random: AtomicU64::new(if config.random_seed == 0 {
                NEXT_SEED.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            } else {
                config.random_seed
            }),
            drop_sequence: AtomicU64::new(0),
        }
    }

    fn should_drop(&self) -> bool {
        let (_, _, ratio) = self.history();
        if ratio <= 0.0 {
            return false;
        }
        let sequence = self.drop_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if sequence.is_multiple_of(self.config.probe_interval) {
            return false;
        }
        self.random_unit() < ratio
    }

    fn record(&self, outcome: CircuitOutcome) {
        if outcome == CircuitOutcome::Cancellation {
            return;
        }
        let mut state = self.state.lock().expect("rolling breaker mutex poisoned");
        self.rotate(&mut state, Instant::now());
        let bucket = state
            .buckets
            .back_mut()
            .expect("rolling breaker always has a current bucket");
        bucket.total = bucket.total.saturating_add(1);
        if outcome == CircuitOutcome::Success {
            bucket.accepted = bucket.accepted.saturating_add(1);
        }
    }

    fn history(&self) -> (u64, u64, f64) {
        let mut state = self.state.lock().expect("rolling breaker mutex poisoned");
        self.rotate(&mut state, Instant::now());
        let (accepted, total) = state.buckets.iter().fold((0_u64, 0_u64), |sum, bucket| {
            (
                sum.0.saturating_add(bucket.accepted),
                sum.1.saturating_add(bucket.total),
            )
        });
        let unprotected = total.saturating_sub(self.config.minimum_requests) as f64;
        let weighted_accepted = self.config.sensitivity * accepted as f64;
        let drop_ratio = ((unprotected - weighted_accepted) / (total as f64 + 1.0)).max(0.0);
        (accepted, total, drop_ratio)
    }

    fn rotate(&self, state: &mut RollingState, now: Instant) {
        let elapsed = now.saturating_duration_since(state.current_started);
        let elapsed_buckets = usize::try_from(elapsed.as_nanos() / self.bucket_width.as_nanos())
            .unwrap_or(self.config.buckets);
        if elapsed_buckets == 0 {
            return;
        }
        if elapsed_buckets >= self.config.buckets {
            state.buckets.clear();
            state.buckets.push_back(OutcomeBucket::default());
            state.current_started = now;
            return;
        }
        for _ in 0..elapsed_buckets {
            state.buckets.push_back(OutcomeBucket::default());
            if state.buckets.len() > self.config.buckets {
                state.buckets.pop_front();
            }
        }
        state.current_started += self.bucket_width * elapsed_buckets as u32;
    }

    fn random_unit(&self) -> f64 {
        let mut current = self.random.load(Ordering::Relaxed);
        loop {
            let mut next = current;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            next = next.max(1);
            match self.random.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return ((next >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0),
                Err(observed) => current = observed,
            }
        }
    }
}

struct RollingState {
    current_started: Instant,
    buckets: VecDeque<OutcomeBucket>,
}

#[derive(Default)]
struct OutcomeBucket {
    accepted: u64,
    total: u64,
}

/// An admitted circuit-breaker call whose outcome may arrive later.
pub struct CircuitBreakerPermit {
    breaker: Arc<CircuitBreaker>,
    finished: bool,
}

impl CircuitBreakerPermit {
    pub fn finish(self, acceptable: bool) {
        self.finish_with_outcome(if acceptable {
            CircuitOutcome::Success
        } else {
            CircuitOutcome::Failure
        });
    }

    pub fn finish_with_outcome(mut self, outcome: CircuitOutcome) {
        self.breaker.record_outcome(outcome);
        self.finished = true;
    }
}

impl Drop for CircuitBreakerPermit {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker.record_outcome(CircuitOutcome::Cancellation);
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
        let breaker = CircuitBreaker::new(
            CircuitBreakerConfig::new(2, Duration::from_secs(1))
                .with_policy(CircuitBreakerPolicy::Consecutive),
        );
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

    #[tokio::test]
    async fn cancelled_async_call_is_neutral_to_rolling_health() {
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::rolling(
            RollingCircuitBreakerConfig::new(),
        )));
        let task_breaker = Arc::clone(&breaker);
        let task = tokio::spawn(async move {
            task_breaker
                .execute_async(std::future::pending::<Result<(), ()>>)
                .await
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let snapshot = breaker.snapshot();
        assert_eq!(snapshot.cancellations, 1);
        assert_eq!((snapshot.accepted, snapshot.total), (0, 0));
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn dropped_transport_permit_records_cancellation_without_healing() {
        let breaker = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig::new(1, Duration::from_millis(5)).with_half_open_max_calls(2),
        ));
        breaker.acquire().unwrap().finish(false);
        thread::sleep(Duration::from_millis(10));
        drop(breaker.acquire().unwrap());
        assert_eq!(breaker.state(), BreakerState::HalfOpen);
        assert_eq!(breaker.snapshot().cancellations, 1);
        breaker.acquire().unwrap().finish(true);
        assert_eq!(breaker.state(), BreakerState::HalfOpen);
    }

    #[test]
    fn rolling_fault_pattern_uses_recent_accepted_and_total_counts() {
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::rolling(
            RollingCircuitBreakerConfig::new()
                .with_window(Duration::from_millis(40), 4)
                .with_minimum_requests(2)
                .with_random_seed(7),
        )));
        for _ in 0..3 {
            breaker.acquire().unwrap().finish(false);
        }
        let snapshot = breaker.snapshot();
        assert_eq!((snapshot.accepted, snapshot.total), (0, 3));
        assert!(snapshot.drop_ratio > 0.0);
        assert_eq!(breaker.state(), BreakerState::Open);

        thread::sleep(Duration::from_millis(50));
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert_eq!(breaker.snapshot().total, 0);
    }

    #[test]
    fn rolling_breaker_guarantees_bounded_probe_traffic() {
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::rolling(
            RollingCircuitBreakerConfig::new()
                .with_sensitivity(0.000_001)
                .with_minimum_requests(0)
                .with_probe_interval(4)
                .with_random_seed(1),
        )));
        for _ in 0..20 {
            if let Some(permit) = breaker.acquire() {
                permit.finish(false);
            }
        }
        assert!(breaker.snapshot().rejections > 0);
        let admitted = (0..4).filter(|_| breaker.acquire().is_some()).count();
        assert!(admitted >= 1);
    }

    #[test]
    fn rolling_accounting_is_exact_under_concurrent_completion() {
        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::rolling(
            RollingCircuitBreakerConfig::new().with_minimum_requests(1_000),
        )));
        let workers: Vec<_> = (0..32)
            .map(|_| {
                let breaker = Arc::clone(&breaker);
                thread::spawn(move || breaker.acquire().unwrap().finish(true))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let snapshot = breaker.snapshot();
        assert_eq!((snapshot.accepted, snapshot.total), (32, 32));
        assert_eq!(snapshot.successes, 32);
    }
}
