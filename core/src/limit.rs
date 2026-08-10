use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "stores-redis")]
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

#[cfg(feature = "stores-redis")]
use sha2::{Digest, Sha256};

#[cfg(feature = "stores-redis")]
use crate::{RedisStore, RedisStoreError};

#[cfg(feature = "stores-redis")]
const TOKEN_BUCKET_SCRIPT: &str = r#"
local clock = redis.call('TIME')
local now = (clock[1] * 1000) + math.floor(clock[2] / 1000)
local rate = tonumber(ARGV[1])
local burst = tonumber(ARGV[2])
local permits = tonumber(ARGV[3])
local values = redis.call('HMGET', KEYS[1], 'tokens', 'updated')
local tokens = tonumber(values[1]) or burst
local updated = tonumber(values[2]) or now
if now > updated then
  tokens = math.min(burst, tokens + ((now - updated) * rate / 1000))
end
local retry = 0
local outcome = 0
if tokens >= permits then
  tokens = tokens - permits
  if tokens < 1 then outcome = 1 end
else
  outcome = 2
  retry = math.ceil((permits - tokens) * 1000 / rate)
end
redis.call('HSET', KEYS[1], 'tokens', tokens, 'updated', now)
redis.call('PEXPIRE', KEYS[1], math.max(1000, math.ceil(burst * 2000 / rate)))
return {outcome, retry}
"#;

#[cfg(feature = "stores-redis")]
const PERIOD_SCRIPT: &str = r#"
local clock = redis.call('TIME')
local now = (clock[1] * 1000) + math.floor(clock[2] / 1000)
local period = tonumber(ARGV[1])
local quota = tonumber(ARGV[2])
local boundary = now - (now % period) + period
local used = tonumber(redis.call('GET', KEYS[1])) or 0
if used >= quota then return {2, boundary - now} end
used = redis.call('INCR', KEYS[1])
if used == 1 then redis.call('PEXPIREAT', KEYS[1], boundary) end
if used == quota then return {1, 0} end
return {0, 0}
"#;

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
            if bucket.available < 1.0 {
                LimitDecision::HitQuota
            } else {
                LimitDecision::Allowed
            }
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
    window: u128,
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
        let now = unix_millis();
        let period = self.period.as_millis();
        let current_window = now / period;
        let retry_after = duration_from_millis_saturating(
            (current_window + 1)
                .saturating_mul(period)
                .saturating_sub(now),
        );
        let mut windows = self.windows.lock().expect("period limiter lock poisoned");
        let window = windows.entry(key).or_insert(PeriodWindow {
            used: 0,
            window: current_window,
        });
        if window.window != current_window {
            window.used = 0;
            window.window = current_window;
        }

        if window.used >= self.quota {
            return LimitDecision::OverQuota { retry_after };
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

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn duration_from_millis_saturating(millis: u128) -> Duration {
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// Observable state for a Redis-backed limiter and its process-local rescue path.
#[cfg(feature = "stores-redis")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedisLimiterSnapshot {
    pub backend_available: bool,
    pub backend_failures: u64,
    pub backend_recoveries: u64,
    pub redis_allowed: u64,
    pub redis_hit_quota: u64,
    pub redis_over_quota: u64,
    pub rescue_allowed: u64,
    pub rescue_hit_quota: u64,
    pub rescue_over_quota: u64,
}

#[cfg(feature = "stores-redis")]
struct RedisLimiterMonitor {
    // 0 = not observed, 1 = available, 2 = unavailable, 3 = recovery probe in flight.
    backend_state: AtomicU8,
    next_probe_at_ms: AtomicU64,
    backend_failures: AtomicU64,
    backend_recoveries: AtomicU64,
    redis_allowed: AtomicU64,
    redis_hit_quota: AtomicU64,
    redis_over_quota: AtomicU64,
    rescue_allowed: AtomicU64,
    rescue_hit_quota: AtomicU64,
    rescue_over_quota: AtomicU64,
}

#[cfg(feature = "stores-redis")]
impl Default for RedisLimiterMonitor {
    fn default() -> Self {
        Self {
            backend_state: AtomicU8::new(0),
            next_probe_at_ms: AtomicU64::new(0),
            backend_failures: AtomicU64::new(0),
            backend_recoveries: AtomicU64::new(0),
            redis_allowed: AtomicU64::new(0),
            redis_hit_quota: AtomicU64::new(0),
            redis_over_quota: AtomicU64::new(0),
            rescue_allowed: AtomicU64::new(0),
            rescue_hit_quota: AtomicU64::new(0),
            rescue_over_quota: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "stores-redis")]
impl RedisLimiterMonitor {
    fn should_attempt_redis(&self) -> bool {
        match self.backend_state.load(Ordering::Acquire) {
            0 | 1 => true,
            2 => {
                let now = unix_millis_u64();
                now >= self.next_probe_at_ms.load(Ordering::Acquire)
                    && self
                        .backend_state
                        .compare_exchange(2, 3, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
            }
            _ => false,
        }
    }

    fn redis_success(&self, decision: LimitDecision) {
        if matches!(self.backend_state.swap(1, Ordering::AcqRel), 2 | 3) {
            self.backend_recoveries.fetch_add(1, Ordering::Relaxed);
        }
        decision_counter(
            decision,
            &self.redis_allowed,
            &self.redis_hit_quota,
            &self.redis_over_quota,
        );
    }

    fn redis_failure(&self, probe_interval_ms: u64) {
        self.next_probe_at_ms.store(
            unix_millis_u64().saturating_add(probe_interval_ms),
            Ordering::Release,
        );
        self.backend_state.store(2, Ordering::Release);
        self.backend_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn rescue(&self, decision: LimitDecision) {
        decision_counter(
            decision,
            &self.rescue_allowed,
            &self.rescue_hit_quota,
            &self.rescue_over_quota,
        );
    }

    fn snapshot(&self) -> RedisLimiterSnapshot {
        RedisLimiterSnapshot {
            backend_available: self.backend_state.load(Ordering::Acquire) == 1,
            backend_failures: self.backend_failures.load(Ordering::Relaxed),
            backend_recoveries: self.backend_recoveries.load(Ordering::Relaxed),
            redis_allowed: self.redis_allowed.load(Ordering::Relaxed),
            redis_hit_quota: self.redis_hit_quota.load(Ordering::Relaxed),
            redis_over_quota: self.redis_over_quota.load(Ordering::Relaxed),
            rescue_allowed: self.rescue_allowed.load(Ordering::Relaxed),
            rescue_hit_quota: self.rescue_hit_quota.load(Ordering::Relaxed),
            rescue_over_quota: self.rescue_over_quota.load(Ordering::Relaxed),
        }
    }
}

#[cfg(feature = "stores-redis")]
fn decision_counter(
    decision: LimitDecision,
    allowed: &AtomicU64,
    hit: &AtomicU64,
    over: &AtomicU64,
) {
    match decision {
        LimitDecision::Allowed => allowed,
        LimitDecision::HitQuota => hit,
        LimitDecision::OverQuota { .. } => over,
    }
    .fetch_add(1, Ordering::Relaxed);
}

/// Atomic Redis token bucket with a process-local rescue bucket.
///
/// If Redis fails or times out, callers use the local bucket and one caller periodically probes
/// for recovery. Dropping the returned future cancels the caller's wait and never grants local
/// capacity speculatively.
#[cfg(feature = "stores-redis")]
#[derive(Clone)]
pub struct RedisTokenLimiter {
    store: RedisStore,
    key: String,
    rate_per_second: u32,
    burst: u32,
    rescue: TokenLimiter,
    recovery_probe_interval_ms: u64,
    monitor: Arc<RedisLimiterMonitor>,
}

#[cfg(feature = "stores-redis")]
impl RedisTokenLimiter {
    pub fn new(
        store: RedisStore,
        key: impl Into<String>,
        rate_per_second: u32,
        burst: u32,
    ) -> Self {
        assert!(rate_per_second > 0, "rate must be greater than zero");
        assert!(burst > 0, "burst must be greater than zero");
        let key = key.into();
        assert!(!key.is_empty(), "Redis limiter key must not be empty");
        Self {
            store,
            key,
            rate_per_second,
            burst,
            rescue: TokenLimiter::new(rate_per_second, burst),
            recovery_probe_interval_ms: 1_000,
            monitor: Arc::new(RedisLimiterMonitor::default()),
        }
    }

    /// Changes how often one caller probes Redis while the backend is unavailable.
    pub fn with_recovery_probe_interval(mut self, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "recovery probe interval must be positive"
        );
        self.recovery_probe_interval_ms =
            u64::try_from(interval.as_millis()).expect("recovery probe interval is too large");
        self
    }

    pub async fn take(&self, permits: u32) -> LimitDecision {
        assert!(permits > 0, "permit count must be greater than zero");
        if permits > self.burst {
            return LimitDecision::OverQuota {
                retry_after: Duration::MAX,
            };
        }
        if !self.monitor.should_attempt_redis() {
            let decision = self.rescue.take(permits);
            self.monitor.rescue(decision);
            return decision;
        }
        let arguments = [
            self.rate_per_second.to_string(),
            self.burst.to_string(),
            permits.to_string(),
        ];
        match self
            .store
            .eval::<Vec<i64>, _, _>(TOKEN_BUCKET_SCRIPT, &[&self.key], &arguments)
            .await
            .and_then(parse_redis_decision)
        {
            Ok(decision) => {
                self.monitor.redis_success(decision);
                decision
            }
            Err(_) => {
                self.monitor.redis_failure(self.recovery_probe_interval_ms);
                let decision = self.rescue.take(permits);
                self.monitor.rescue(decision);
                decision
            }
        }
    }

    pub async fn allow(&self) -> bool {
        self.take(1).await.is_allowed()
    }

    pub fn snapshot(&self) -> RedisLimiterSnapshot {
        self.monitor.snapshot()
    }
}

#[cfg(feature = "stores-redis")]
struct BoundedPeriodLimiter {
    period: Duration,
    quota: u32,
    max_keys: usize,
    windows: Mutex<HashMap<String, PeriodWindow>>,
}

#[cfg(feature = "stores-redis")]
impl BoundedPeriodLimiter {
    fn take(&self, key: &str) -> LimitDecision {
        let now = unix_millis();
        let period = self.period.as_millis();
        let current_window = now / period;
        let retry_after = duration_from_millis_saturating(
            (current_window + 1)
                .saturating_mul(period)
                .saturating_sub(now),
        );
        let mut windows = self.windows.lock().expect("period limiter lock poisoned");
        if !windows.contains_key(key) && windows.len() >= self.max_keys {
            windows.retain(|_, window| window.window == current_window);
            if windows.len() >= self.max_keys {
                return LimitDecision::OverQuota { retry_after };
            }
        }
        let window = windows.entry(key.to_owned()).or_insert(PeriodWindow {
            used: 0,
            window: current_window,
        });
        if window.window != current_window {
            window.used = 0;
            window.window = current_window;
        }
        if window.used >= self.quota {
            return LimitDecision::OverQuota { retry_after };
        }
        window.used += 1;
        if window.used == self.quota {
            LimitDecision::HitQuota
        } else {
            LimitDecision::Allowed
        }
    }
}

/// Atomic Redis fixed-window limiter keyed by an application identity.
///
/// Windows are aligned to Redis server-time boundaries. Application keys are SHA-256 hashed before
/// use, avoiding accidental disclosure and keeping Redis keys bounded. During Redis failures the
/// rescue map rejects new identities after `max_rescue_keys` rather than evicting active quotas.
#[cfg(feature = "stores-redis")]
#[derive(Clone)]
pub struct RedisPeriodLimiter {
    store: RedisStore,
    namespace: String,
    period: Duration,
    quota: u32,
    rescue: Arc<BoundedPeriodLimiter>,
    recovery_probe_interval_ms: u64,
    monitor: Arc<RedisLimiterMonitor>,
}

#[cfg(feature = "stores-redis")]
impl RedisPeriodLimiter {
    pub fn new(
        store: RedisStore,
        namespace: impl Into<String>,
        period: Duration,
        quota: u32,
        max_rescue_keys: usize,
    ) -> Self {
        assert!(!period.is_zero(), "period must be greater than zero");
        assert!(period.as_millis() <= u64::MAX.into(), "period is too large");
        assert!(quota > 0, "quota must be greater than zero");
        assert!(max_rescue_keys > 0, "rescue key capacity must be positive");
        let namespace = namespace.into();
        assert!(
            !namespace.is_empty(),
            "Redis limiter namespace must not be empty"
        );
        Self {
            store,
            namespace,
            period,
            quota,
            rescue: Arc::new(BoundedPeriodLimiter {
                period,
                quota,
                max_keys: max_rescue_keys,
                windows: Mutex::new(HashMap::new()),
            }),
            recovery_probe_interval_ms: 1_000,
            monitor: Arc::new(RedisLimiterMonitor::default()),
        }
    }

    /// Changes how often one caller probes Redis while the backend is unavailable.
    pub fn with_recovery_probe_interval(mut self, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "recovery probe interval must be positive"
        );
        self.recovery_probe_interval_ms =
            u64::try_from(interval.as_millis()).expect("recovery probe interval is too large");
        self
    }

    pub async fn take(&self, key: &str) -> LimitDecision {
        if !self.monitor.should_attempt_redis() {
            let rescue_key = hex_digest(key);
            let decision = self.rescue.take(&rescue_key);
            self.monitor.rescue(decision);
            return decision;
        }
        let redis_key = format!("{}:{}", self.namespace, hex_digest(key));
        let arguments = [self.period.as_millis().to_string(), self.quota.to_string()];
        match self
            .store
            .eval::<Vec<i64>, _, _>(PERIOD_SCRIPT, &[redis_key], &arguments)
            .await
            .and_then(parse_redis_decision)
        {
            Ok(decision) => {
                self.monitor.redis_success(decision);
                decision
            }
            Err(_) => {
                self.monitor.redis_failure(self.recovery_probe_interval_ms);
                let rescue_key = hex_digest(key);
                let decision = self.rescue.take(&rescue_key);
                self.monitor.rescue(decision);
                decision
            }
        }
    }

    pub fn snapshot(&self) -> RedisLimiterSnapshot {
        self.monitor.snapshot()
    }
}

#[cfg(feature = "stores-redis")]
fn parse_redis_decision(response: Vec<i64>) -> Result<LimitDecision, RedisStoreError> {
    let [outcome, retry_after] = response.as_slice() else {
        return Err(RedisStoreError::UnexpectedResponse(format!(
            "limiter script returned {response:?}"
        )));
    };
    match outcome {
        0 => Ok(LimitDecision::Allowed),
        1 => Ok(LimitDecision::HitQuota),
        2 if *retry_after >= 0 => Ok(LimitDecision::OverQuota {
            retry_after: Duration::from_millis(*retry_after as u64),
        }),
        _ => Err(RedisStoreError::UnexpectedResponse(format!(
            "limiter script returned {response:?}"
        ))),
    }
}

#[cfg(feature = "stores-redis")]
fn hex_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(feature = "stores-redis")]
fn unix_millis_u64() -> u64 {
    u64::try_from(unix_millis()).unwrap_or(u64::MAX)
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

    #[cfg(feature = "stores-redis")]
    #[tokio::test]
    async fn redis_limiter_rescue_is_bounded_and_observable() {
        let store = RedisStore::new(
            crate::RedisStoreConfig::new("redis://127.0.0.1:1/")
                .with_operation_timeout(Duration::from_millis(20)),
        )
        .unwrap();
        let token = RedisTokenLimiter::new(store.clone(), "unavailable-token", 1, 2);
        assert_eq!(token.take(1).await, LimitDecision::Allowed);
        assert_eq!(token.take(1).await, LimitDecision::HitQuota);
        assert!(matches!(
            token.take(1).await,
            LimitDecision::OverQuota { .. }
        ));
        assert_eq!(token.snapshot().backend_failures, 1);
        assert_eq!(token.snapshot().rescue_over_quota, 1);

        let period =
            RedisPeriodLimiter::new(store, "unavailable-period", Duration::from_secs(60), 2, 1);
        assert_eq!(period.take("known").await, LimitDecision::Allowed);
        assert_eq!(period.take("known").await, LimitDecision::HitQuota);
        assert!(matches!(
            period.take("new-identity").await,
            LimitDecision::OverQuota { .. }
        ));

        let monitor = RedisLimiterMonitor::default();
        monitor.redis_failure(1_000);
        assert!(!monitor.should_attempt_redis());
        monitor.next_probe_at_ms.store(0, Ordering::Release);
        assert!(monitor.should_attempt_redis());
        monitor.redis_success(LimitDecision::Allowed);
        assert!(monitor.snapshot().backend_available);
        assert_eq!(monitor.snapshot().backend_recoveries, 1);
    }

    #[cfg(feature = "stores-redis")]
    async fn exercise_real_redis_limiters(store: RedisStore, namespace: &str) {
        let token_key = format!("{namespace}:token");
        let period_namespace = format!("{namespace}:period");
        let period_key = format!("{}:{}", period_namespace, hex_digest("shared-user"));
        let concurrent_namespace = format!("{namespace}:concurrent");
        let concurrent_key = format!(
            "{}:{}",
            concurrent_namespace,
            hex_digest("shared-concurrent-user")
        );
        store
            .delete(&[&token_key, &period_key, &concurrent_key])
            .await
            .unwrap();

        let token_a = RedisTokenLimiter::new(store.clone(), &token_key, 1, 2);
        let token_b = RedisTokenLimiter::new(store.clone(), &token_key, 1, 2);
        assert_eq!(token_a.take(1).await, LimitDecision::Allowed);
        assert_eq!(token_b.take(1).await, LimitDecision::HitQuota);
        assert!(matches!(
            token_a.take(1).await,
            LimitDecision::OverQuota { .. }
        ));
        assert!(token_a.snapshot().backend_available);

        let period_a = RedisPeriodLimiter::new(
            store.clone(),
            &period_namespace,
            Duration::from_secs(60),
            2,
            16,
        );
        let period_b =
            RedisPeriodLimiter::new(store, &period_namespace, Duration::from_secs(60), 2, 16);
        assert_eq!(period_a.take("shared-user").await, LimitDecision::Allowed);
        assert_eq!(period_b.take("shared-user").await, LimitDecision::HitQuota);
        let rejected = period_a.take("shared-user").await;
        assert!(matches!(
            rejected,
            LimitDecision::OverQuota { retry_after } if !retry_after.is_zero()
        ));

        let concurrent = RedisPeriodLimiter::new(
            period_b.store.clone(),
            concurrent_namespace,
            Duration::from_secs(60),
            8,
            16,
        );
        let decisions = futures::future::join_all((0..32).map(|_| {
            let limiter = concurrent.clone();
            async move { limiter.take("shared-concurrent-user").await }
        }))
        .await;
        assert_eq!(
            decisions
                .iter()
                .filter(|decision| decision.is_allowed())
                .count(),
            8
        );
    }

    #[cfg(feature = "stores-redis")]
    #[tokio::test]
    async fn redis_limiter_integration_shares_standalone_quotas() {
        let Ok(url) = std::env::var("RUST_ZERO_REDIS_URL") else {
            return;
        };
        let namespace = format!("rust-zero-limit:{}", std::process::id());
        let store = RedisStore::new(
            crate::RedisStoreConfig::new(url).with_key_prefix(format!("{namespace}:")),
        )
        .unwrap();
        exercise_real_redis_limiters(store, "standalone").await;
    }

    #[cfg(feature = "stores-redis")]
    #[tokio::test]
    async fn redis_cluster_integration_shares_limiter_quotas() {
        let Ok(nodes) = std::env::var("RUST_ZERO_REDIS_CLUSTER_URLS") else {
            return;
        };
        let store = RedisStore::new(
            crate::RedisStoreConfig::cluster(
                nodes
                    .split(',')
                    .map(str::trim)
                    .filter(|node| !node.is_empty()),
            )
            .with_key_prefix(format!("rust-zero-limit-cluster:{}:", std::process::id())),
        )
        .unwrap();
        exercise_real_redis_limiters(store, "cluster").await;
    }
}
