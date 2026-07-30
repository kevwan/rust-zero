use std::{future::Future, time::Duration};

/// Exponential-backoff settings for a retryable operation.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub fn new(max_attempts: usize, initial_delay: Duration) -> Self {
        assert!(
            max_attempts > 0,
            "maximum attempts must be greater than zero"
        );
        assert!(
            !initial_delay.is_zero(),
            "initial delay must be greater than zero"
        );
        Self {
            max_attempts,
            initial_delay,
            max_delay: Duration::from_secs(30),
        }
    }

    pub fn with_max_delay(mut self, max_delay: Duration) -> Self {
        assert!(
            !max_delay.is_zero(),
            "maximum delay must be greater than zero"
        );
        self.max_delay = max_delay;
        self
    }
}

/// Retries an async operation with bounded exponential backoff.
pub async fn retry<T, E, F, Fut>(policy: RetryPolicy, mut operation: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut delay = policy.initial_delay;

    for attempt in 1..=policy.max_attempts {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt == policy.max_attempts => return Err(error),
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(policy.max_delay);
            }
        }
    }

    unreachable!("a retry policy always has at least one attempt")
}

/// Runs an async operation with a hard deadline.
pub async fn timeout<T, Fut>(
    duration: Duration,
    operation: Fut,
) -> Result<T, tokio::time::error::Elapsed>
where
    Fut: Future<Output = T>,
{
    tokio::time::timeout(duration, operation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn retries_until_the_operation_succeeds() {
        let attempts = AtomicUsize::new(0);
        let result = retry(RetryPolicy::new(3, Duration::from_millis(1)), || async {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                Err("unavailable")
            } else {
                Ok("connected")
            }
        })
        .await;

        assert_eq!(result, Ok("connected"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn deadline_cancels_slow_work() {
        let result = timeout(Duration::from_millis(1), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        })
        .await;

        assert!(result.is_err());
    }
}
