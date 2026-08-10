use std::{future::Future, sync::Arc};

use rust_zero_core::{
    AdaptiveShedder, BreakerState, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError,
    CircuitBreakerSnapshot, CircuitOutcome, LoadShedderConfig,
};
use tonic::{Code, Status};

/// Returns whether a gRPC result should be treated as healthy by a circuit breaker.
///
/// Client mistakes and domain failures do not indicate an unhealthy dependency. Transport and
/// server failures do, matching go-zero's zrpc outcome classification.
pub fn acceptable_status(status: &Status) -> bool {
    !matches!(
        status.code(),
        Code::DeadlineExceeded
            | Code::Internal
            | Code::Unavailable
            | Code::DataLoss
            | Code::Unimplemented
            | Code::ResourceExhausted
    )
}

/// Maps a completed gRPC call to breaker health without treating caller cancellation as either a
/// dependency success or failure.
pub fn circuit_outcome(status: &Status) -> CircuitOutcome {
    if status.code() == Code::Cancelled {
        CircuitOutcome::Cancellation
    } else if acceptable_status(status) {
        CircuitOutcome::Success
    } else {
        CircuitOutcome::Failure
    }
}

/// Protocol-aware circuit breaking for unary Tonic client calls.
#[derive(Clone)]
pub struct RpcCircuitBreaker {
    breaker: Arc<CircuitBreaker>,
}

impl RpcCircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            breaker: Arc::new(CircuitBreaker::new(config)),
        }
    }

    pub fn state(&self) -> BreakerState {
        self.breaker.state()
    }

    pub fn snapshot(&self) -> CircuitBreakerSnapshot {
        self.breaker.snapshot()
    }

    /// Runs a unary call when the circuit permits it.
    ///
    /// Infrastructure failures count against the circuit. Statuses such as `InvalidArgument`,
    /// `NotFound`, and `PermissionDenied` are returned without marking the dependency unhealthy.
    pub async fn call<T, F, Fut>(&self, operation: F) -> Result<T, Status>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, Status>>,
    {
        match self
            .breaker
            .execute_async_with_outcome(operation, |result| {
                result
                    .as_ref()
                    .map_or_else(circuit_outcome, |_| CircuitOutcome::Success)
            })
            .await
        {
            Ok(value) => Ok(value),
            Err(CircuitBreakerError::Operation(status)) => Err(status),
            Err(CircuitBreakerError::Open) => {
                Err(Status::unavailable("gRPC dependency circuit is open"))
            }
        }
    }
}

/// Adaptive admission control for unary Tonic server handlers.
#[derive(Clone)]
pub struct RpcLoadShedder {
    shedder: AdaptiveShedder,
}

impl RpcLoadShedder {
    pub fn new(config: LoadShedderConfig) -> Self {
        Self {
            shedder: AdaptiveShedder::new(config),
        }
    }

    pub fn current_limit(&self) -> usize {
        self.shedder.current_limit()
    }

    pub fn in_flight(&self) -> usize {
        self.shedder.in_flight()
    }

    /// Admits and observes a unary handler, or rejects it with `ResourceExhausted`.
    pub async fn call<T, F, Fut>(&self, operation: F) -> Result<T, Status>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, Status>>,
    {
        let _permit = self
            .shedder
            .try_acquire()
            .ok_or_else(|| Status::resource_exhausted("gRPC server is overloaded"))?;
        operation().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    use tokio::sync::Notify;

    #[test]
    fn classifies_application_and_infrastructure_statuses() {
        for code in [
            Code::DeadlineExceeded,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
            Code::Unimplemented,
            Code::ResourceExhausted,
        ] {
            assert!(!acceptable_status(&Status::new(code, "failure")));
        }

        for code in [
            Code::InvalidArgument,
            Code::NotFound,
            Code::AlreadyExists,
            Code::PermissionDenied,
            Code::Unauthenticated,
        ] {
            assert!(acceptable_status(&Status::new(code, "application error")));
        }

        assert_eq!(
            circuit_outcome(&Status::cancelled("caller left")),
            CircuitOutcome::Cancellation
        );
    }

    #[tokio::test]
    async fn circuit_opens_only_for_infrastructure_failures() {
        let breaker = RpcCircuitBreaker::new(CircuitBreakerConfig::new(2, Duration::from_secs(30)));

        for _ in 0..3 {
            let error = breaker
                .call(|| async { Err::<(), _>(Status::invalid_argument("bad request")) })
                .await
                .unwrap_err();
            assert_eq!(error.code(), Code::InvalidArgument);
        }
        assert_eq!(breaker.state(), BreakerState::Closed);

        for _ in 0..2 {
            let error = breaker
                .call(|| async { Err::<(), _>(Status::unavailable("offline")) })
                .await
                .unwrap_err();
            assert_eq!(error.message(), "offline");
        }
        assert_eq!(breaker.state(), BreakerState::Open);

        let invoked = AtomicBool::new(false);
        let error = breaker
            .call(|| async {
                invoked.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::Unavailable);
        assert!(!invoked.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn rolling_breaker_tracks_protocol_outcomes_and_rejects_faulty_traffic() {
        use rust_zero_core::RollingCircuitBreakerConfig;

        let breaker = RpcCircuitBreaker::new(CircuitBreakerConfig::rolling(
            RollingCircuitBreakerConfig::new()
                .with_minimum_requests(1)
                .with_sensitivity(0.1)
                .with_random_seed(3),
        ));
        let cancelled = breaker
            .call(|| async { Err::<(), _>(Status::cancelled("caller left")) })
            .await
            .unwrap_err();
        assert_eq!(cancelled.code(), Code::Cancelled);

        for _ in 0..2 {
            let _ = breaker
                .call(|| async { Err::<(), _>(Status::unavailable("offline")) })
                .await;
        }
        let snapshot = breaker.snapshot();
        assert_eq!(snapshot.cancellations, 1);
        assert_eq!(snapshot.failures, 2);
        assert_eq!(snapshot.total, 2);
        assert!(snapshot.drop_ratio > 0.0);

        for _ in 0..20 {
            let _ = breaker.call(|| async { Ok::<_, Status>(()) }).await;
        }
        assert!(breaker.snapshot().rejections > 0);
    }

    #[tokio::test]
    async fn load_shedder_rejects_work_beyond_the_current_limit() {
        let shedder = RpcLoadShedder::new(LoadShedderConfig::new(1, Duration::from_secs(1)));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task_shedder = shedder.clone();
        let task_entered = Arc::clone(&entered);
        let task_release = Arc::clone(&release);
        let active = tokio::spawn(async move {
            task_shedder
                .call(|| async move {
                    task_entered.notify_one();
                    task_release.notified().await;
                    Ok(())
                })
                .await
        });

        entered.notified().await;
        assert_eq!(shedder.in_flight(), 1);
        let error = shedder.call(|| async { Ok(()) }).await.unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);

        release.notify_one();
        active.await.unwrap().unwrap();
        assert_eq!(shedder.in_flight(), 0);
    }
}
