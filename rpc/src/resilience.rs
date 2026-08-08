use std::{future::Future, sync::Arc};

use rust_zero_core::{
    AdaptiveShedder, BreakerState, CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError,
    LoadShedderConfig,
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
            .execute_async_with_accept(operation, |result| {
                result
                    .as_ref()
                    .map(|_| true)
                    .unwrap_or_else(acceptable_status)
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
