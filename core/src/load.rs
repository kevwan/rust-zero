use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Admission policy used by [`AdaptiveShedder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadShedderMode {
    /// Adjust a concurrency ceiling from batches of observed response times.
    Latency,
    /// Shed only during CPU pressure, using recent throughput and response-time capacity.
    CpuThroughput,
}

/// Settings for adaptive admission control.
#[derive(Debug, Clone, Copy)]
pub struct LoadShedderConfig {
    pub max_concurrency: usize,
    pub target_latency: Duration,
    pub sample_window: usize,
    pub mode: LoadShedderMode,
    pub cpu_threshold: f64,
    pub bucket_duration: Duration,
    pub bucket_count: usize,
    pub cooldown: Duration,
    pub in_flight_smoothing: f64,
}

impl LoadShedderConfig {
    /// Creates the original latency-adaptive limiter.
    pub fn new(max_concurrency: usize, target_latency: Duration) -> Self {
        assert!(
            max_concurrency > 0,
            "maximum concurrency must be greater than zero"
        );
        assert!(
            !target_latency.is_zero(),
            "target latency must be greater than zero"
        );

        Self {
            max_concurrency,
            target_latency,
            sample_window: 32,
            mode: LoadShedderMode::Latency,
            cpu_threshold: 0.9,
            bucket_duration: Duration::from_secs(1),
            bucket_count: 10,
            cooldown: Duration::from_secs(1),
            in_flight_smoothing: 0.5,
        }
    }

    /// Creates a CPU- and throughput-aware production shedder.
    pub fn production(max_concurrency: usize) -> Self {
        Self::new(max_concurrency, Duration::from_millis(1))
            .with_mode(LoadShedderMode::CpuThroughput)
    }

    pub fn with_mode(mut self, mode: LoadShedderMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_sample_window(mut self, sample_window: usize) -> Self {
        assert!(sample_window > 0, "sample window must be greater than zero");
        self.sample_window = sample_window;
        self
    }

    pub fn with_cpu_threshold(mut self, threshold: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&threshold) && threshold > 0.0,
            "CPU threshold must be in (0, 1]"
        );
        self.cpu_threshold = threshold;
        self
    }

    pub fn with_rolling_window(mut self, bucket_duration: Duration, bucket_count: usize) -> Self {
        assert!(
            !bucket_duration.is_zero(),
            "bucket duration must be greater than zero"
        );
        assert!(bucket_count > 0, "bucket count must be greater than zero");
        self.bucket_duration = bucket_duration;
        self.bucket_count = bucket_count;
        self
    }

    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        assert!(!cooldown.is_zero(), "cooldown must be greater than zero");
        self.cooldown = cooldown;
        self
    }

    pub fn with_in_flight_smoothing(mut self, smoothing: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&smoothing) && smoothing > 0.0,
            "in-flight smoothing must be in (0, 1]"
        );
        self.in_flight_smoothing = smoothing;
        self
    }
}

#[derive(Clone, Copy)]
struct Bucket {
    started_at: Instant,
    completed: usize,
    minimum_latency: Option<Duration>,
}

struct ShedderState {
    current_limit: usize,
    sample_count: usize,
    total_latency: Duration,
    buckets: VecDeque<Bucket>,
    smoothed_in_flight: f64,
    cooldown_until: Option<Instant>,
}

trait CpuSource: Send + Sync {
    fn usage(&self) -> f64;
}

struct Inner {
    config: LoadShedderConfig,
    in_flight: AtomicUsize,
    state: Mutex<ShedderState>,
    cpu: Arc<dyn CpuSource>,
}

/// Current production-shedder measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadShedderSnapshot {
    pub in_flight: usize,
    pub smoothed_in_flight: f64,
    pub current_limit: usize,
    pub maximum_throughput: f64,
    pub minimum_latency: Option<Duration>,
    pub cooling_down: bool,
}

/// Dynamically sheds requests when latency or process load indicates saturation.
#[derive(Clone)]
pub struct AdaptiveShedder {
    inner: Arc<Inner>,
}

impl AdaptiveShedder {
    pub fn new(config: LoadShedderConfig) -> Self {
        Self::with_cpu_source(config, Arc::new(ProcessCpuSource::new()))
    }

    fn with_cpu_source(config: LoadShedderConfig, cpu: Arc<dyn CpuSource>) -> Self {
        let now = Instant::now();
        Self {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(ShedderState {
                    current_limit: config.max_concurrency,
                    sample_count: 0,
                    total_latency: Duration::ZERO,
                    buckets: VecDeque::from([Bucket {
                        started_at: now,
                        completed: 0,
                        minimum_latency: None,
                    }]),
                    smoothed_in_flight: 0.0,
                    cooldown_until: None,
                }),
                in_flight: AtomicUsize::new(0),
                cpu,
            }),
        }
    }

    /// Attempts to reserve execution capacity. Dropping a successful permit records its latency.
    pub fn try_acquire(&self) -> Option<ShedPermit> {
        loop {
            let active = self.inner.in_flight.load(Ordering::Acquire);
            let now = Instant::now();
            let mut state = self
                .inner
                .state
                .lock()
                .expect("load shedder state lock poisoned");
            rotate_buckets(&mut state, &self.inner.config, now);

            let limit = match self.inner.config.mode {
                LoadShedderMode::Latency => state.current_limit,
                LoadShedderMode::CpuThroughput => production_limit(&state, &self.inner.config),
            };
            state.current_limit = limit;
            let smoothed = state.smoothed_in_flight;
            let cooling_down = state.cooldown_until.is_some_and(|until| now < until);
            let cpu_overloaded = self.inner.cpu.usage() >= self.inner.config.cpu_threshold;
            let fixed_capacity_exhausted = active >= self.inner.config.max_concurrency;
            let dynamically_overloaded = self.inner.config.mode == LoadShedderMode::CpuThroughput
                && (cpu_overloaded || cooling_down)
                && active >= limit
                && smoothed >= limit as f64;
            if fixed_capacity_exhausted
                || (self.inner.config.mode == LoadShedderMode::Latency && active >= limit)
                || dynamically_overloaded
            {
                if cpu_overloaded {
                    state.cooldown_until = Some(now + self.inner.config.cooldown);
                }
                return None;
            }

            drop(state);
            if self
                .inner
                .in_flight
                .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .expect("load shedder state lock poisoned");
                let alpha = self.inner.config.in_flight_smoothing;
                state.smoothed_in_flight = if state.smoothed_in_flight == 0.0 {
                    (active + 1) as f64
                } else {
                    alpha * (active + 1) as f64 + (1.0 - alpha) * state.smoothed_in_flight
                };
                return Some(ShedPermit {
                    inner: Arc::clone(&self.inner),
                    started_at: now,
                });
            }
        }
    }

    pub fn current_limit(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("load shedder state lock poisoned")
            .current_limit
    }

    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> LoadShedderSnapshot {
        let now = Instant::now();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("load shedder state lock poisoned");
        rotate_buckets(&mut state, &self.inner.config, now);
        let (maximum_throughput, minimum_latency) = rolling_capacity(&state, &self.inner.config);
        LoadShedderSnapshot {
            in_flight: self.in_flight(),
            smoothed_in_flight: state.smoothed_in_flight,
            current_limit: state.current_limit,
            maximum_throughput,
            minimum_latency,
            cooling_down: state.cooldown_until.is_some_and(|until| now < until),
        }
    }
}

/// A capacity reservation from [`AdaptiveShedder`].
pub struct ShedPermit {
    inner: Arc<Inner>,
    started_at: Instant,
}

impl Drop for ShedPermit {
    fn drop(&mut self) {
        let active = self
            .inner
            .in_flight
            .fetch_sub(1, Ordering::Release)
            .saturating_sub(1);
        let elapsed = self.started_at.elapsed();
        let now = Instant::now();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("load shedder state lock poisoned");

        if self.inner.config.mode == LoadShedderMode::CpuThroughput {
            rotate_buckets(&mut state, &self.inner.config, now);
            let bucket = state
                .buckets
                .back_mut()
                .expect("shedder always has a bucket");
            bucket.completed = bucket.completed.saturating_add(1);
            bucket.minimum_latency = Some(
                bucket
                    .minimum_latency
                    .map_or(elapsed, |old| old.min(elapsed)),
            );
            let alpha = self.inner.config.in_flight_smoothing;
            state.smoothed_in_flight =
                alpha * active as f64 + (1.0 - alpha) * state.smoothed_in_flight;
            state.current_limit = production_limit(&state, &self.inner.config);
            return;
        }

        state.sample_count += 1;
        state.total_latency += elapsed;
        if state.sample_count < self.inner.config.sample_window {
            return;
        }
        let average_latency = state.total_latency / state.sample_count as u32;
        if average_latency > self.inner.config.target_latency {
            state.current_limit = (state.current_limit * 9 / 10).max(1);
        } else {
            state.current_limit = (state.current_limit + 1).min(self.inner.config.max_concurrency);
        }
        state.sample_count = 0;
        state.total_latency = Duration::ZERO;
    }
}

fn rotate_buckets(state: &mut ShedderState, config: &LoadShedderConfig, now: Instant) {
    let latest = state
        .buckets
        .back()
        .expect("shedder always has a bucket")
        .started_at;
    let elapsed = now.saturating_duration_since(latest);
    let steps = (elapsed.as_nanos() / config.bucket_duration.as_nanos()) as usize;
    for step in 0..steps.min(config.bucket_count) {
        state.buckets.push_back(Bucket {
            started_at: latest + config.bucket_duration * (step as u32 + 1),
            completed: 0,
            minimum_latency: None,
        });
    }
    while state.buckets.len() > config.bucket_count {
        state.buckets.pop_front();
    }
    if steps >= config.bucket_count {
        state.buckets.clear();
        state.buckets.push_back(Bucket {
            started_at: now,
            completed: 0,
            minimum_latency: None,
        });
    }
}

fn rolling_capacity(state: &ShedderState, config: &LoadShedderConfig) -> (f64, Option<Duration>) {
    let maximum = state
        .buckets
        .iter()
        .map(|bucket| bucket.completed)
        .max()
        .unwrap_or(0);
    let throughput = maximum as f64 / config.bucket_duration.as_secs_f64();
    let minimum_latency = state
        .buckets
        .iter()
        .filter_map(|bucket| bucket.minimum_latency)
        .min();
    (throughput, minimum_latency)
}

fn production_limit(state: &ShedderState, config: &LoadShedderConfig) -> usize {
    let (throughput, minimum_latency) = rolling_capacity(state, config);
    let Some(minimum_latency) = minimum_latency else {
        return config.max_concurrency;
    };
    ((throughput * minimum_latency.as_secs_f64()).ceil().max(1.0) as usize)
        .min(config.max_concurrency)
}

struct ProcessCpuSource {
    state: Mutex<ProcessCpuState>,
}

struct ProcessCpuState {
    wall: Instant,
    cpu: Duration,
    usage: f64,
}

impl ProcessCpuSource {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProcessCpuState {
                wall: Instant::now(),
                cpu: process_cpu_time(),
                usage: 0.0,
            }),
        }
    }
}

impl CpuSource for ProcessCpuSource {
    fn usage(&self) -> f64 {
        let now = Instant::now();
        let mut state = self.state.lock().expect("CPU sampler lock poisoned");
        let wall = now.saturating_duration_since(state.wall);
        if wall < Duration::from_millis(100) {
            return state.usage;
        }
        let cpu = process_cpu_time();
        let used = cpu.saturating_sub(state.cpu).as_secs_f64();
        let cores = std::thread::available_parallelism().map_or(1, usize::from) as f64;
        state.usage = (used / wall.as_secs_f64() / cores).clamp(0.0, 1.0);
        state.wall = now;
        state.cpu = cpu;
        state.usage
    }
}

#[cfg(unix)]
fn process_cpu_time() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Duration::ZERO;
    }
    let usage = unsafe { usage.assume_init() };
    let seconds = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec).max(0) as u64;
    let micros = (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec).max(0) as u64;
    Duration::from_secs(seconds) + Duration::from_micros(micros)
}

#[cfg(not(unix))]
fn process_cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    struct TestCpu(AtomicU64);
    impl TestCpu {
        fn new(usage: f64) -> Self {
            Self(AtomicU64::new(usage.to_bits()))
        }
        fn set(&self, usage: f64) {
            self.0.store(usage.to_bits(), Ordering::Release);
        }
    }
    impl CpuSource for TestCpu {
        fn usage(&self) -> f64 {
            f64::from_bits(self.0.load(Ordering::Acquire))
        }
    }

    #[test]
    fn rejects_work_after_reaching_the_limit() {
        let shedder = AdaptiveShedder::new(LoadShedderConfig::new(1, Duration::from_secs(1)));
        let permit = shedder.try_acquire().expect("first request is admitted");
        assert!(shedder.try_acquire().is_none());
        assert_eq!(shedder.in_flight(), 1);
        drop(permit);
        assert_eq!(shedder.in_flight(), 0);
    }

    #[test]
    fn reduces_the_limit_when_latency_exceeds_the_target() {
        let shedder = AdaptiveShedder::new(
            LoadShedderConfig::new(10, Duration::from_millis(1)).with_sample_window(2),
        );
        for _ in 0..2 {
            let permit = shedder.try_acquire().unwrap();
            thread::sleep(Duration::from_millis(2));
            drop(permit);
        }
        assert!(shedder.current_limit() < 10);
    }

    #[test]
    fn production_mode_sheds_during_cpu_saturation_and_recovers_after_cooldown() {
        let cpu = Arc::new(TestCpu::new(0.1));
        let config = LoadShedderConfig::production(8)
            .with_cpu_threshold(0.8)
            .with_rolling_window(Duration::from_millis(10), 4)
            .with_cooldown(Duration::from_millis(15))
            .with_in_flight_smoothing(1.0);
        let shedder = AdaptiveShedder::with_cpu_source(config, cpu.clone());

        let warmup = shedder.try_acquire().unwrap();
        thread::sleep(Duration::from_millis(2));
        drop(warmup);
        let active = shedder.try_acquire().unwrap();
        cpu.set(0.95);
        assert!(
            shedder.try_acquire().is_none(),
            "CPU pressure should reject above learned capacity"
        );
        cpu.set(0.1);
        assert!(
            shedder.try_acquire().is_none(),
            "cooldown should prevent an immediate surge"
        );
        thread::sleep(Duration::from_millis(20));
        assert!(
            shedder.try_acquire().is_some(),
            "traffic should recover after cooldown"
        );
        drop(active);
    }

    #[test]
    fn production_mode_admits_sparse_traffic_and_completes_concurrent_permits() {
        let cpu = Arc::new(TestCpu::new(1.0));
        let config = LoadShedderConfig::production(4).with_in_flight_smoothing(1.0);
        let shedder = AdaptiveShedder::with_cpu_source(config, cpu);
        let permit = shedder
            .try_acquire()
            .expect("sparse traffic has no learned overload ceiling");
        drop(permit);
        assert_eq!(shedder.in_flight(), 0);

        let shedder = AdaptiveShedder::new(LoadShedderConfig::production(16));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let shedder = shedder.clone();
                thread::spawn(move || drop(shedder.try_acquire().unwrap()))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(shedder.in_flight(), 0);
        assert_eq!(shedder.snapshot().maximum_throughput, 8.0);
    }
}
