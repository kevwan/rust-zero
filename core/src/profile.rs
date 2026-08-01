use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// A timing point returned by [`Profiler::start`].
#[derive(Debug)]
pub struct ProfilePoint {
    started_at: Option<Instant>,
}

impl ProfilePoint {
    /// Returns the elapsed time when profiling was enabled when this point was created.
    pub fn elapsed(&self) -> Option<Duration> {
        self.started_at.map(|started_at| started_at.elapsed())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProfileSlot {
    lifetime_count: u64,
    lifetime_duration: Duration,
    interval_count: u64,
    interval_duration: Duration,
}

/// An immutable aggregate for one named profiling operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub name: String,
    pub lifetime_count: u64,
    pub lifetime_duration: Duration,
    pub interval_count: u64,
    pub interval_duration: Duration,
}

impl ProfileSnapshot {
    pub fn lifetime_average(&self) -> Option<Duration> {
        average(self.lifetime_duration, self.lifetime_count)
    }

    pub fn interval_average(&self) -> Option<Duration> {
        average(self.interval_duration, self.interval_count)
    }
}

/// Low-overhead, named duration profiling.
///
/// Profiling starts disabled, matching go-zero's opt-in profiler. Calling [`snapshot`] returns
/// lifetime aggregates and resets only the interval aggregates.
#[derive(Debug, Default)]
pub struct Profiler {
    enabled: AtomicBool,
    slots: Mutex<BTreeMap<String, ProfileSlot>>,
}

impl Profiler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn start(&self) -> ProfilePoint {
        ProfilePoint {
            started_at: self.is_enabled().then(Instant::now),
        }
    }

    pub fn report(&self, name: impl Into<String>, point: ProfilePoint) {
        let Some(duration) = point.elapsed() else {
            return;
        };
        self.record(name, duration);
    }

    pub fn record(&self, name: impl Into<String>, duration: Duration) {
        if !self.is_enabled() {
            return;
        }

        let mut slots = self.slots.lock().expect("profiler mutex poisoned");
        let slot = slots.entry(name.into()).or_default();
        slot.lifetime_count = slot.lifetime_count.saturating_add(1);
        slot.lifetime_duration = slot.lifetime_duration.saturating_add(duration);
        slot.interval_count = slot.interval_count.saturating_add(1);
        slot.interval_duration = slot.interval_duration.saturating_add(duration);
    }

    /// Takes a report snapshot and begins a fresh reporting interval.
    pub fn snapshot(&self) -> Vec<ProfileSnapshot> {
        let mut slots = self.slots.lock().expect("profiler mutex poisoned");
        slots
            .iter_mut()
            .map(|(name, slot)| {
                let snapshot = ProfileSnapshot {
                    name: name.clone(),
                    lifetime_count: slot.lifetime_count,
                    lifetime_duration: slot.lifetime_duration,
                    interval_count: slot.interval_count,
                    interval_duration: slot.interval_duration,
                };
                slot.interval_count = 0;
                slot.interval_duration = Duration::ZERO;
                snapshot
            })
            .collect()
    }

    /// Renders the same lifetime/last-interval view as go-zero's profiler report.
    pub fn render_report(&self) -> String {
        let mut output = String::from(
            "Profiling report\nOPERATION,LIFETIME_COUNT,LIFETIME_AVERAGE,INTERVAL_COUNT,INTERVAL_AVERAGE\n",
        );
        for snapshot in self.snapshot() {
            let _ = writeln!(
                output,
                "{},{},{},{},{}",
                snapshot.name,
                snapshot.lifetime_count,
                display_average(snapshot.lifetime_average()),
                snapshot.interval_count,
                display_average(snapshot.interval_average()),
            );
        }
        output
    }
}

fn average(total: Duration, count: u64) -> Option<Duration> {
    (count > 0).then(|| Duration::from_secs_f64(total.as_secs_f64() / count as f64))
}

fn display_average(duration: Option<Duration>) -> String {
    duration
        .map(|duration| format!("{:.6}s", duration.as_secs_f64()))
        .unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_profiler_has_negligible_noop_points() {
        let profiler = Profiler::new();
        let point = profiler.start();
        assert_eq!(point.elapsed(), None);
        profiler.report("ignored", point);
        assert!(profiler.snapshot().is_empty());
    }

    #[test]
    fn snapshots_keep_lifetime_and_reset_interval_values() {
        let profiler = Profiler::new();
        profiler.enable();
        profiler.record("database", Duration::from_millis(10));
        profiler.record("database", Duration::from_millis(30));

        let first = profiler.snapshot();
        assert_eq!(first[0].lifetime_count, 2);
        assert_eq!(first[0].lifetime_average(), Some(Duration::from_millis(20)));
        assert_eq!(first[0].interval_count, 2);

        let second = profiler.snapshot();
        assert_eq!(second[0].lifetime_count, 2);
        assert_eq!(second[0].interval_count, 0);
        assert_eq!(second[0].interval_average(), None);
    }

    #[test]
    fn report_is_stable_and_human_readable() {
        let profiler = Profiler::new();
        profiler.enable();
        profiler.record("cache", Duration::from_millis(5));

        let report = profiler.render_report();
        assert!(report.contains("OPERATION,LIFETIME_COUNT"));
        assert!(report.contains("cache,1,0.005000s,1,0.005000s"));
    }
}
