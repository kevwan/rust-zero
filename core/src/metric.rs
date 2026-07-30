use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex},
};

/// Prometheus-compatible metrics registry.
#[derive(Default)]
pub struct Metrics {
    families: Mutex<BTreeMap<String, Arc<MetricFamily>>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a monotonically increasing counter vector.
    pub fn counter_vec(&self, options: VectorOptions) -> Result<CounterVec, MetricsError> {
        let family = self.register(options, MetricKind::Counter)?;
        Ok(CounterVec { family })
    }

    /// Registers a gauge vector.
    pub fn gauge_vec(&self, options: VectorOptions) -> Result<GaugeVec, MetricsError> {
        let family = self.register(options, MetricKind::Gauge)?;
        Ok(GaugeVec { family })
    }

    /// Registers a histogram vector with the supplied upper-bound buckets.
    pub fn histogram_vec(&self, options: HistogramOptions) -> Result<HistogramVec, MetricsError> {
        let name = options.vector.name();
        validate_options(&options.vector, &name)?;

        let mut buckets = options.buckets;
        if buckets.is_empty() {
            buckets = DEFAULT_HISTOGRAM_BUCKETS.to_vec();
        }
        if buckets
            .iter()
            .any(|bucket| !bucket.is_finite() || *bucket <= 0.0)
            || buckets.windows(2).any(|window| window[0] >= window[1])
        {
            return Err(MetricsError::InvalidHistogramBuckets(name));
        }

        let family = Arc::new(MetricFamily {
            name: name.clone(),
            help: options.vector.help,
            labels: options.vector.labels,
            kind: MetricKind::Histogram,
            values: Mutex::new(MetricValues::Histogram {
                buckets,
                observations: BTreeMap::new(),
            }),
        });
        self.insert(name, Arc::clone(&family))?;
        Ok(HistogramVec { family })
    }

    /// Encodes all registered metrics using the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let families = self
            .families
            .lock()
            .expect("metrics registry mutex poisoned");
        let mut output = String::new();

        for family in families.values() {
            output.push_str("# HELP ");
            output.push_str(&family.name);
            output.push(' ');
            output.push_str(&escape_help(&family.help));
            output.push('\n');
            output.push_str("# TYPE ");
            output.push_str(&family.name);
            output.push(' ');
            output.push_str(family.kind.prometheus_name());
            output.push('\n');

            let values = family.values.lock().expect("metric values mutex poisoned");
            match &*values {
                MetricValues::Counter(values) | MetricValues::Gauge(values) => {
                    for (labels, value) in values {
                        write_sample(&mut output, &family.name, &family.labels, labels, *value);
                    }
                }
                MetricValues::Histogram {
                    buckets,
                    observations,
                } => {
                    for (labels, observation) in observations {
                        let mut count = 0_u64;
                        for bucket in buckets {
                            count += observation
                                .values
                                .iter()
                                .filter(|value| **value <= *bucket)
                                .count() as u64;
                            write_histogram_bucket(
                                &mut output,
                                &family.name,
                                &family.labels,
                                labels,
                                *bucket,
                                count,
                            );
                            count = 0;
                        }
                        write_histogram_bucket(
                            &mut output,
                            &family.name,
                            &family.labels,
                            labels,
                            f64::INFINITY,
                            observation.values.len() as u64,
                        );
                        write_sample(
                            &mut output,
                            &format!("{}_sum", family.name),
                            &family.labels,
                            labels,
                            observation.values.iter().sum(),
                        );
                        write_sample(
                            &mut output,
                            &format!("{}_count", family.name),
                            &family.labels,
                            labels,
                            observation.values.len() as f64,
                        );
                    }
                }
            }
        }

        output
    }

    fn register(
        &self,
        options: VectorOptions,
        kind: MetricKind,
    ) -> Result<Arc<MetricFamily>, MetricsError> {
        let name = options.name();
        validate_options(&options, &name)?;
        let family = Arc::new(MetricFamily {
            name: name.clone(),
            help: options.help,
            labels: options.labels,
            kind,
            values: Mutex::new(match kind {
                MetricKind::Counter => MetricValues::Counter(BTreeMap::new()),
                MetricKind::Gauge => MetricValues::Gauge(BTreeMap::new()),
                MetricKind::Histogram => unreachable!("histograms use histogram_vec"),
            }),
        });
        self.insert(name, Arc::clone(&family))?;
        Ok(family)
    }

    fn insert(&self, name: String, family: Arc<MetricFamily>) -> Result<(), MetricsError> {
        let mut families = self
            .families
            .lock()
            .expect("metrics registry mutex poisoned");
        if families.contains_key(&name) {
            return Err(MetricsError::DuplicateMetric(name));
        }
        families.insert(name, family);
        Ok(())
    }
}

/// Shared options for counter and gauge vectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorOptions {
    pub namespace: String,
    pub subsystem: String,
    pub name: String,
    pub help: String,
    pub labels: Vec<String>,
}

impl VectorOptions {
    pub fn new(name: impl Into<String>, help: impl Into<String>) -> Self {
        Self {
            namespace: String::new(),
            subsystem: String::new(),
            name: name.into(),
            help: help.into(),
            labels: Vec::new(),
        }
    }

    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    pub fn with_subsystem(mut self, subsystem: impl Into<String>) -> Self {
        self.subsystem = subsystem.into();
        self
    }

    pub fn with_labels<I, S>(mut self, labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.labels = labels.into_iter().map(Into::into).collect();
        self
    }

    fn name(&self) -> String {
        [&self.namespace, &self.subsystem, &self.name]
            .into_iter()
            .filter(|part| !part.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("_")
    }
}

/// Options for a histogram vector.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramOptions {
    pub vector: VectorOptions,
    pub buckets: Vec<f64>,
}

impl HistogramOptions {
    pub fn new(name: impl Into<String>, help: impl Into<String>) -> Self {
        Self {
            vector: VectorOptions::new(name, help),
            buckets: DEFAULT_HISTOGRAM_BUCKETS.to_vec(),
        }
    }

    pub fn with_vector_options(mut self, options: VectorOptions) -> Self {
        self.vector = options;
        self
    }

    pub fn with_buckets(mut self, buckets: impl Into<Vec<f64>>) -> Self {
        self.buckets = buckets.into();
        self
    }
}

/// A registered counter vector.
#[derive(Clone)]
pub struct CounterVec {
    family: Arc<MetricFamily>,
}

impl CounterVec {
    pub fn inc(&self, labels: &[&str]) -> Result<(), MetricsError> {
        self.add(1.0, labels)
    }

    pub fn add(&self, value: f64, labels: &[&str]) -> Result<(), MetricsError> {
        if !value.is_finite() || value < 0.0 {
            return Err(MetricsError::InvalidCounterValue(value));
        }
        let labels = self.family.validate_labels(labels)?;
        let mut values = self
            .family
            .values
            .lock()
            .expect("metric values mutex poisoned");
        let MetricValues::Counter(values) = &mut *values else {
            unreachable!("counter vector must contain counter values");
        };
        *values.entry(labels).or_default() += value;
        Ok(())
    }
}

/// A registered gauge vector.
#[derive(Clone)]
pub struct GaugeVec {
    family: Arc<MetricFamily>,
}

impl GaugeVec {
    pub fn set(&self, value: f64, labels: &[&str]) -> Result<(), MetricsError> {
        self.update(value, labels, |current, value| *current = value)
    }

    pub fn inc(&self, labels: &[&str]) -> Result<(), MetricsError> {
        self.add(1.0, labels)
    }

    pub fn dec(&self, labels: &[&str]) -> Result<(), MetricsError> {
        self.add(-1.0, labels)
    }

    pub fn add(&self, value: f64, labels: &[&str]) -> Result<(), MetricsError> {
        self.update(value, labels, |current, value| *current += value)
    }

    pub fn sub(&self, value: f64, labels: &[&str]) -> Result<(), MetricsError> {
        self.add(-value, labels)
    }

    fn update(
        &self,
        value: f64,
        labels: &[&str],
        update: impl FnOnce(&mut f64, f64),
    ) -> Result<(), MetricsError> {
        if !value.is_finite() {
            return Err(MetricsError::InvalidGaugeValue(value));
        }
        let labels = self.family.validate_labels(labels)?;
        let mut values = self
            .family
            .values
            .lock()
            .expect("metric values mutex poisoned");
        let MetricValues::Gauge(values) = &mut *values else {
            unreachable!("gauge vector must contain gauge values");
        };
        update(values.entry(labels).or_default(), value);
        Ok(())
    }
}

/// A registered histogram vector.
#[derive(Clone)]
pub struct HistogramVec {
    family: Arc<MetricFamily>,
}

impl HistogramVec {
    pub fn observe(&self, value: f64, labels: &[&str]) -> Result<(), MetricsError> {
        if !value.is_finite() {
            return Err(MetricsError::InvalidObservation(value));
        }
        let labels = self.family.validate_labels(labels)?;
        let mut values = self
            .family
            .values
            .lock()
            .expect("metric values mutex poisoned");
        let MetricValues::Histogram { observations, .. } = &mut *values else {
            unreachable!("histogram vector must contain histogram values");
        };
        observations.entry(labels).or_default().values.push(value);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetricsError {
    InvalidMetricName(String),
    InvalidLabelName(String),
    DuplicateLabel(String),
    DuplicateMetric(String),
    LabelCount { expected: usize, actual: usize },
    InvalidCounterValue(f64),
    InvalidGaugeValue(f64),
    InvalidObservation(f64),
    InvalidHistogramBuckets(String),
}

impl fmt::Display for MetricsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMetricName(name) => {
                write!(formatter, "invalid Prometheus metric name: {name}")
            }
            Self::InvalidLabelName(name) => {
                write!(formatter, "invalid Prometheus label name: {name}")
            }
            Self::DuplicateLabel(name) => {
                write!(formatter, "duplicate Prometheus label name: {name}")
            }
            Self::DuplicateMetric(name) => {
                write!(formatter, "metric is already registered: {name}")
            }
            Self::LabelCount { expected, actual } => {
                write!(
                    formatter,
                    "metric expects {expected} label values but received {actual}"
                )
            }
            Self::InvalidCounterValue(value) => {
                write!(
                    formatter,
                    "counter values must be finite and non-negative: {value}"
                )
            }
            Self::InvalidGaugeValue(value) => {
                write!(formatter, "gauge values must be finite: {value}")
            }
            Self::InvalidObservation(value) => {
                write!(formatter, "histogram observations must be finite: {value}")
            }
            Self::InvalidHistogramBuckets(name) => {
                write!(
                    formatter,
                    "histogram buckets must be finite, positive, and increasing: {name}"
                )
            }
        }
    }
}

impl std::error::Error for MetricsError {}

const DEFAULT_HISTOGRAM_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

struct MetricFamily {
    name: String,
    help: String,
    labels: Vec<String>,
    kind: MetricKind,
    values: Mutex<MetricValues>,
}

impl MetricFamily {
    fn validate_labels(&self, labels: &[&str]) -> Result<Vec<String>, MetricsError> {
        if labels.len() != self.labels.len() {
            return Err(MetricsError::LabelCount {
                expected: self.labels.len(),
                actual: labels.len(),
            });
        }
        Ok(labels.iter().map(|label| (*label).to_owned()).collect())
    }
}

#[derive(Clone, Copy)]
enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    fn prometheus_name(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

enum MetricValues {
    Counter(BTreeMap<Vec<String>, f64>),
    Gauge(BTreeMap<Vec<String>, f64>),
    Histogram {
        buckets: Vec<f64>,
        observations: BTreeMap<Vec<String>, HistogramObservation>,
    },
}

#[derive(Default)]
struct HistogramObservation {
    values: Vec<f64>,
}

fn validate_options(options: &VectorOptions, name: &str) -> Result<(), MetricsError> {
    if !is_valid_identifier(name) {
        return Err(MetricsError::InvalidMetricName(name.to_owned()));
    }

    let mut labels = BTreeSet::new();
    for label in &options.labels {
        if !is_valid_identifier(label) {
            return Err(MetricsError::InvalidLabelName(label.clone()));
        }
        if !labels.insert(label) {
            return Err(MetricsError::DuplicateLabel(label.clone()));
        }
    }

    Ok(())
}

fn is_valid_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(characters.next(), Some(character) if character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn write_sample(
    output: &mut String,
    name: &str,
    label_names: &[String],
    labels: &[String],
    value: f64,
) {
    output.push_str(name);
    write_labels(output, label_names, labels, None);
    output.push(' ');
    output.push_str(&format_float(value));
    output.push('\n');
}

fn write_histogram_bucket(
    output: &mut String,
    name: &str,
    label_names: &[String],
    labels: &[String],
    bucket: f64,
    count: u64,
) {
    output.push_str(name);
    output.push_str("_bucket");
    write_labels(
        output,
        label_names,
        labels,
        Some(("le", format_float(bucket))),
    );
    output.push(' ');
    output.push_str(&count.to_string());
    output.push('\n');
}

fn write_labels(
    output: &mut String,
    label_names: &[String],
    labels: &[String],
    extra: Option<(&str, String)>,
) {
    if label_names.is_empty() && extra.is_none() {
        return;
    }

    output.push('{');
    for (index, (name, value)) in label_names.iter().zip(labels).enumerate() {
        if index > 0 {
            output.push(',');
        }
        write_label(output, name, value);
    }
    if let Some((name, value)) = extra {
        if !label_names.is_empty() {
            output.push(',');
        }
        write_label(output, name, &value);
    }
    output.push('}');
}

fn write_label(output: &mut String, name: &str, value: &str) {
    output.push_str(name);
    output.push_str("=\"");
    output.push_str(&escape_label(value));
    output.push('"');
}

fn format_float(value: f64) -> String {
    if value == f64::INFINITY {
        "+Inf".to_owned()
    } else {
        value.to_string()
    }
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_labeled_counters_and_gauges() {
        let metrics = Metrics::new();
        let requests = metrics
            .counter_vec(
                VectorOptions::new("requests_total", "Completed requests")
                    .with_namespace("users")
                    .with_labels(["method", "status"]),
            )
            .unwrap();
        let connections = metrics
            .gauge_vec(VectorOptions::new("connections", "Open connections"))
            .unwrap();

        requests.inc(&["GET", "200"]).unwrap();
        requests.add(2.0, &["GET", "200"]).unwrap();
        connections.set(4.0, &[]).unwrap();
        connections.dec(&[]).unwrap();

        assert_eq!(
            metrics.render(),
            concat!(
                "# HELP connections Open connections\n",
                "# TYPE connections gauge\n",
                "connections 3\n",
                "# HELP users_requests_total Completed requests\n",
                "# TYPE users_requests_total counter\n",
                "users_requests_total{method=\"GET\",status=\"200\"} 3\n",
            )
        );
    }

    #[test]
    fn renders_cumulative_histogram_buckets() {
        let metrics = Metrics::new();
        let duration = metrics
            .histogram_vec(
                HistogramOptions::new("request_duration_seconds", "Request duration")
                    .with_buckets(vec![0.1, 1.0]),
            )
            .unwrap();

        duration.observe(0.05, &[]).unwrap();
        duration.observe(0.5, &[]).unwrap();

        assert_eq!(
            metrics.render(),
            concat!(
                "# HELP request_duration_seconds Request duration\n",
                "# TYPE request_duration_seconds histogram\n",
                "request_duration_seconds_bucket{le=\"0.1\"} 1\n",
                "request_duration_seconds_bucket{le=\"1\"} 2\n",
                "request_duration_seconds_bucket{le=\"+Inf\"} 2\n",
                "request_duration_seconds_sum 0.55\n",
                "request_duration_seconds_count 2\n",
            )
        );
    }

    #[test]
    fn rejects_invalid_metric_definitions_and_label_counts() {
        let metrics = Metrics::new();

        assert_eq!(
            metrics
                .counter_vec(VectorOptions::new("invalid-name", "Invalid"))
                .err()
                .expect("invalid metric name must be rejected"),
            MetricsError::InvalidMetricName("invalid-name".to_owned())
        );

        let counter = metrics
            .counter_vec(VectorOptions::new("events_total", "Events").with_labels(["kind"]))
            .unwrap();
        assert_eq!(
            counter.inc(&[]).unwrap_err(),
            MetricsError::LabelCount {
                expected: 1,
                actual: 0
            }
        );
    }
}
