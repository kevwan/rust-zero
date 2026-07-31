use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Result of requesting capacity from a rate limiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitDecision {
    Allowed,
    HitQuota,
    OverQuota { retry_after: Duration },
}

impl LimitDecision {
    pub fn is_allowed(self) -> bool {
        !matches!(self, Self::OverQuota { .. })
    }
}

struct Bucket {
    available: f64,
    last_refill: Instant,
}

/// A process-local token-bucket limiter with burst support.
#[derive(Clone)]
pub struct TokenLimiter {
    rate_per_second: f64,
    burst: u32,
    bucket: Arc<Mutex<Bucket>>,
}

impl TokenLimiter {
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        assert!(rate_per_second > 0, "rate must be greater than zero");
        assert!(burst > 0, "burst must be greater than zero");
        Self {
            rate_per_second: f64::from(rate_per_second),
            burst,
            bucket: Arc::new(Mutex::new(Bucket {
                available: f64::from(burst),
                last_refill: Instant::now(),
            })),
        }
    }

    pub fn allow(&self) -> bool {
        self.take(1).is_allowed()
    }

    pub fn take(&self, permits: u32) -> LimitDecision {
        assert!(permits > 0, "permit count must be greater than zero");
        if permits > self.burst {
            return LimitDecision::OverQuota {
                retry_after: Duration::MAX,
            };
        }

        let mut bucket = self.bucket.lock().expect("token limiter lock poisoned");
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.available =
            (bucket.available + elapsed * self.rate_per_second).min(f64::from(self.burst));
        bucket.last_refill = now;

        if bucket.available >= f64::from(permits) {
            bucket.available -= f64::from(permits);
            LimitDecision::Allowed
        } else {
            LimitDecision::OverQuota {
                retry_after: Duration::from_secs_f64(
                    (f64::from(permits) - bucket.available) / self.rate_per_second,
                ),
            }
        }
    }
}

struct PeriodWindow {
    used: u32,
    expires_at: Instant,
}

/// A keyed fixed-window quota limiter.
///
/// A distributed backend can wrap this API, while this implementation provides go-zero's
/// in-process rescue behavior when no Redis deployment is available.
pub struct PeriodLimiter<K> {
    period: Duration,
    quota: u32,
    windows: Mutex<HashMap<K, PeriodWindow>>,
}

impl<K> PeriodLimiter<K>
where
    K: Eq + Hash,
{
    pub fn new(period: Duration, quota: u32) -> Self {
        assert!(!period.is_zero(), "period must be greater than zero");
        assert!(quota > 0, "quota must be greater than zero");
        Self {
            period,
            quota,
            windows: Mutex::new(HashMap::new()),
        }
    }

    pub fn take(&self, key: K) -> LimitDecision {
        let now = Instant::now();
        let mut windows = self.windows.lock().expect("period limiter lock poisoned");
        let window = windows.entry(key).or_insert(PeriodWindow {
            used: 0,
            expires_at: now + self.period,
        });
        if window.expires_at <= now {
            window.used = 0;
            window.expires_at = now + self.period;
        }

        if window.used >= self.quota {
            return LimitDecision::OverQuota {
                retry_after: window.expires_at.saturating_duration_since(now),
            };
        }

        window.used += 1;
        if window.used == self.quota {
            LimitDecision::HitQuota
        } else {
            LimitDecision::Allowed
        }
    }

    pub fn clear(&self, key: &K) {
        self.windows
            .lock()
            .expect("period limiter lock poisoned")
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_limiter_honors_burst_capacity() {
        let limiter = TokenLimiter::new(1, 2);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(matches!(limiter.take(1), LimitDecision::OverQuota { .. }));
    }

    #[test]
    fn period_limiter_reports_the_quota_boundary() {
        let limiter = PeriodLimiter::new(Duration::from_secs(1), 2);
        assert_eq!(limiter.take("user"), LimitDecision::Allowed);
        assert_eq!(limiter.take("user"), LimitDecision::HitQuota);
        assert!(matches!(
            limiter.take("user"),
            LimitDecision::OverQuota { .. }
        ));
        assert_eq!(limiter.take("other"), LimitDecision::Allowed);
    }
}
