use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Settings for an adaptive concurrency limiter.
#[derive(Debug, Clone, Copy)]
pub struct LoadShedderConfig {
    pub max_concurrency: usize,
    pub target_latency: Duration,
    pub sample_window: usize,
}

impl LoadShedderConfig {
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
        }
    }

    pub fn with_sample_window(mut self, sample_window: usize) -> Self {
        assert!(sample_window > 0, "sample window must be greater than zero");
        self.sample_window = sample_window;
        self
    }
}

struct ShedderState {
    current_limit: usize,
    sample_count: usize,
    total_latency: Duration,
}

struct Inner {
    config: LoadShedderConfig,
    in_flight: AtomicUsize,
    state: Mutex<ShedderState>,
}

/// Dynamically sheds requests when observed latency indicates downstream saturation.
#[derive(Clone)]
pub struct AdaptiveShedder {
    inner: Arc<Inner>,
}

impl AdaptiveShedder {
    pub fn new(config: LoadShedderConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                in_flight: AtomicUsize::new(0),
                state: Mutex::new(ShedderState {
                    current_limit: config.max_concurrency,
                    sample_count: 0,
                    total_latency: Duration::ZERO,
                }),
            }),
        }
    }

    /// Attempts to reserve execution capacity. Dropping a successful permit records its latency.
    pub fn try_acquire(&self) -> Option<ShedPermit> {
        loop {
            let active = self.inner.in_flight.load(Ordering::Acquire);
            let limit = self
                .inner
                .state
                .lock()
                .expect("load shedder state lock poisoned")
                .current_limit;
            if active >= limit {
                return None;
            }

            if self
                .inner
                .in_flight
                .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(ShedPermit {
                    inner: Arc::clone(&self.inner),
                    started_at: Instant::now(),
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
}

/// A capacity reservation from [`AdaptiveShedder`].
pub struct ShedPermit {
    inner: Arc<Inner>,
    started_at: Instant,
}

impl Drop for ShedPermit {
    fn drop(&mut self) {
        self.inner.in_flight.fetch_sub(1, Ordering::Release);
        let elapsed = self.started_at.elapsed();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("load shedder state lock poisoned");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
}
