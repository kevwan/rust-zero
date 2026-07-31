use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Time-windowed numeric observations split into fixed-duration buckets.
pub struct RollingWindow {
    bucket_count: usize,
    bucket_width: Duration,
    state: Mutex<WindowState>,
}

impl RollingWindow {
    pub fn new(bucket_count: usize, bucket_width: Duration) -> Self {
        assert!(bucket_count > 0, "bucket count must be greater than zero");
        assert!(
            !bucket_width.is_zero(),
            "bucket width must be greater than zero"
        );

        let now = Instant::now();
        Self {
            bucket_count,
            bucket_width,
            state: Mutex::new(WindowState {
                current_started: now,
                buckets: VecDeque::from([Bucket::default()]),
            }),
        }
    }

    pub fn record(&self, value: f64) {
        assert!(value.is_finite(), "rolling-window values must be finite");
        let mut state = self.state.lock().expect("rolling window mutex poisoned");
        self.rotate(&mut state, Instant::now());
        state
            .buckets
            .back_mut()
            .expect("rolling window always contains its current bucket")
            .record(value);
    }

    pub fn snapshot(&self) -> RollingSnapshot {
        let mut state = self.state.lock().expect("rolling window mutex poisoned");
        self.rotate(&mut state, Instant::now());

        let mut snapshot = RollingSnapshot::default();
        for bucket in &state.buckets {
            snapshot.count += bucket.count;
            snapshot.sum += bucket.sum;
            if let Some(minimum) = bucket.minimum {
                snapshot.minimum =
                    Some(snapshot.minimum.map_or(minimum, |value| value.min(minimum)));
            }
            if let Some(maximum) = bucket.maximum {
                snapshot.maximum =
                    Some(snapshot.maximum.map_or(maximum, |value| value.max(maximum)));
            }
        }
        snapshot
    }

    pub fn reset(&self) {
        let mut state = self.state.lock().expect("rolling window mutex poisoned");
        state.current_started = Instant::now();
        state.buckets.clear();
        state.buckets.push_back(Bucket::default());
    }

    fn rotate(&self, state: &mut WindowState, now: Instant) {
        let elapsed = now.saturating_duration_since(state.current_started);
        let elapsed_buckets_u128 = elapsed.as_nanos() / self.bucket_width.as_nanos();
        let elapsed_buckets = usize::try_from(elapsed_buckets_u128).unwrap_or(self.bucket_count);
        if elapsed_buckets == 0 {
            return;
        }

        if elapsed_buckets >= self.bucket_count {
            state.buckets.clear();
            state.buckets.push_back(Bucket::default());
            state.current_started = now;
            return;
        }

        for _ in 0..elapsed_buckets {
            state.buckets.push_back(Bucket::default());
            if state.buckets.len() > self.bucket_count {
                state.buckets.pop_front();
            }
        }
        state.current_started += self.bucket_width * elapsed_buckets as u32;
    }
}

struct WindowState {
    current_started: Instant,
    buckets: VecDeque<Bucket>,
}

#[derive(Default)]
struct Bucket {
    count: u64,
    sum: f64,
    minimum: Option<f64>,
    maximum: Option<f64>,
}

impl Bucket {
    fn record(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        self.minimum = Some(self.minimum.map_or(value, |minimum| minimum.min(value)));
        self.maximum = Some(self.maximum.map_or(value, |maximum| maximum.max(value)));
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RollingSnapshot {
    pub count: u64,
    pub sum: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}

impl RollingSnapshot {
    pub fn average(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn aggregates_observations_in_the_active_window() {
        let window = RollingWindow::new(3, Duration::from_secs(1));
        window.record(2.0);
        window.record(4.0);

        assert_eq!(
            window.snapshot(),
            RollingSnapshot {
                count: 2,
                sum: 6.0,
                minimum: Some(2.0),
                maximum: Some(4.0),
            }
        );
        assert_eq!(window.snapshot().average(), Some(3.0));
    }

    #[test]
    fn expires_old_buckets() {
        let window = RollingWindow::new(2, Duration::from_millis(5));
        window.record(10.0);
        thread::sleep(Duration::from_millis(15));

        assert_eq!(window.snapshot(), RollingSnapshot::default());
    }
}
