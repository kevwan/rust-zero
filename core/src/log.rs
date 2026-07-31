use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::TraceContext;

/// Severity threshold and event level for structured logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Slow,
    Warn,
    Error,
    Severe,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Slow => "slow",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Severe => "severe",
        }
    }
}

/// Encoding used for each emitted log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogEncoding {
    Json,
    Plain,
}

/// File rollover strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationPolicy {
    /// Writes to a UTC date-stamped file and opens a new file when the date changes.
    Daily,
    /// Rolls the active file before it would exceed `max_bytes`.
    ///
    /// `max_backups == 0` retains all backups. Otherwise, only that many numbered backups
    /// are retained.
    Size { max_bytes: u64, max_backups: usize },
}

/// Destination for log records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    Console,
    File {
        directory: PathBuf,
        rotation: RotationPolicy,
    },
}

/// Standalone structured logger configuration.
#[derive(Debug, Clone)]
pub struct LogConfig {
    pub service_name: String,
    pub level: LogLevel,
    pub encoding: LogEncoding,
    pub target: LogTarget,
    pub max_content_length: Option<usize>,
}

impl LogConfig {
    pub fn console(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            level: LogLevel::Info,
            encoding: LogEncoding::Json,
            target: LogTarget::Console,
            max_content_length: None,
        }
    }

    pub fn file(
        service_name: impl Into<String>,
        directory: impl Into<PathBuf>,
        rotation: RotationPolicy,
    ) -> Self {
        Self {
            service_name: service_name.into(),
            level: LogLevel::Info,
            encoding: LogEncoding::Json,
            target: LogTarget::File {
                directory: directory.into(),
                rotation,
            },
            max_content_length: None,
        }
    }

    pub fn with_level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }

    pub fn with_encoding(mut self, encoding: LogEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    pub fn with_max_content_length(mut self, length: usize) -> Self {
        assert!(
            length > 0,
            "maximum content length must be greater than zero"
        );
        self.max_content_length = Some(length);
        self
    }
}

/// Opt-in desensitization for values containing secrets or personal information.
pub trait Sensitive {
    fn mask_sensitive(&self) -> Value;
}

/// One structured key/value pair.
#[derive(Debug, Clone, PartialEq)]
pub struct LogField {
    key: String,
    value: Value,
}

impl LogField {
    pub fn new(key: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    pub fn from_serializable(
        key: impl Into<String>,
        value: impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            key: key.into(),
            value: serde_json::to_value(value)?,
        })
    }

    pub fn sensitive(key: impl Into<String>, value: &impl Sensitive) -> Self {
        Self {
            key: key.into(),
            value: value.mask_sensitive(),
        }
    }
}

/// Fields shared by logs emitted while handling one request or operation.
#[derive(Debug, Clone, Default)]
pub struct LogContext {
    fields: BTreeMap<String, Value>,
    trace: Option<TraceContext>,
}

impl LogContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_field(mut self, field: LogField) -> Self {
        self.fields.insert(field.key, field.value);
        self
    }

    pub fn with_trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }
}

/// A cheap deterministic sampler that keeps an initial burst and then every Nth event.
#[derive(Debug)]
pub struct LogSampler {
    first: u64,
    thereafter: u64,
    seen: AtomicU64,
}

impl LogSampler {
    pub fn new(first: u64, thereafter: u64) -> Self {
        assert!(
            thereafter > 0,
            "sampling interval must be greater than zero"
        );
        Self {
            first,
            thereafter,
            seen: AtomicU64::new(0),
        }
    }

    pub fn allow(&self) -> bool {
        let seen = self.seen.fetch_add(1, Ordering::Relaxed);
        seen < self.first || (seen - self.first).is_multiple_of(self.thereafter)
    }
}

/// Cloneable, process-wide structured logger.
#[derive(Clone)]
pub struct Logger {
    config: Arc<LogConfig>,
    sink: Arc<Mutex<Sink>>,
}

impl fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Logger")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Logger {
    pub fn new(config: LogConfig) -> Result<Self, LogError> {
        validate_config(&config)?;
        let sink = match &config.target {
            LogTarget::Console => Sink::Writer(Box::new(io::stdout())),
            LogTarget::File {
                directory,
                rotation,
            } => Sink::Rotating(RotatingFile::new(
                directory,
                &config.service_name,
                *rotation,
            )?),
        };
        Ok(Self {
            config: Arc::new(config),
            sink: Arc::new(Mutex::new(sink)),
        })
    }

    /// Creates a logger backed by an application-provided writer.
    pub fn to_writer(
        config: LogConfig,
        writer: impl Write + Send + 'static,
    ) -> Result<Self, LogError> {
        validate_config(&config)?;
        Ok(Self {
            config: Arc::new(config),
            sink: Arc::new(Mutex::new(Sink::Writer(Box::new(writer)))),
        })
    }

    pub fn enabled(&self, level: LogLevel) -> bool {
        level >= self.config.level
    }

    pub fn log(
        &self,
        level: LogLevel,
        message: impl AsRef<str>,
        fields: impl IntoIterator<Item = LogField>,
    ) -> Result<bool, LogError> {
        self.log_with_context(level, message, None, fields)
    }

    pub fn log_with_context(
        &self,
        level: LogLevel,
        message: impl AsRef<str>,
        context: Option<&LogContext>,
        fields: impl IntoIterator<Item = LogField>,
    ) -> Result<bool, LogError> {
        if !self.enabled(level) {
            return Ok(false);
        }

        let record = self.record(level, message.as_ref(), context, fields);
        let mut encoded = match self.config.encoding {
            LogEncoding::Json => serde_json::to_vec(&record)?,
            LogEncoding::Plain => encode_plain(&record),
        };
        encoded.push(b'\n');
        self.sink
            .lock()
            .map_err(|_| LogError::Poisoned)?
            .write_all(&encoded)?;
        Ok(true)
    }

    pub fn log_sampled(
        &self,
        sampler: &LogSampler,
        level: LogLevel,
        message: impl AsRef<str>,
        context: Option<&LogContext>,
        fields: impl IntoIterator<Item = LogField>,
    ) -> Result<bool, LogError> {
        if !self.enabled(level) || !sampler.allow() {
            return Ok(false);
        }
        self.log_with_context(level, message, context, fields)
    }

    fn record(
        &self,
        level: LogLevel,
        message: &str,
        context: Option<&LogContext>,
        fields: impl IntoIterator<Item = LogField>,
    ) -> Map<String, Value> {
        let mut record = Map::new();
        if let Some(context) = context {
            for (key, value) in &context.fields {
                record.insert(key.clone(), value.clone());
            }
        }
        for field in fields {
            record.insert(field.key, field.value);
        }

        // Framework-owned fields are inserted last so callers cannot spoof severity, service,
        // timestamps, messages, or trace identity with a custom field.
        record.insert("timestamp".to_owned(), Value::String(timestamp()));
        record.insert("level".to_owned(), Value::String(level.as_str().to_owned()));
        record.insert(
            "service".to_owned(),
            Value::String(self.config.service_name.clone()),
        );
        record.insert(
            "message".to_owned(),
            Value::String(truncate(message, self.config.max_content_length)),
        );
        if let Some(trace) = context.and_then(|context| context.trace.as_ref()) {
            record.insert("trace_id".to_owned(), Value::String(trace.trace_id()));
            record.insert("span_id".to_owned(), Value::String(trace.span_id()));
        }
        record
    }
}

enum Sink {
    Writer(Box<dyn Write + Send>),
    Rotating(RotatingFile),
}

impl Sink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Writer(writer) => {
                writer.write_all(bytes)?;
                writer.flush()
            }
            Self::Rotating(file) => file.write_all(bytes),
        }
    }
}

struct RotatingFile {
    directory: PathBuf,
    service_name: String,
    rotation: RotationPolicy,
    file: Option<File>,
    active_path: PathBuf,
    active_day: i64,
    bytes_written: u64,
}

impl RotatingFile {
    fn new(directory: &Path, service_name: &str, rotation: RotationPolicy) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let service_name = safe_file_name(service_name);
        let active_day = unix_day();
        let active_path = log_path(directory, &service_name, rotation, active_day);
        let file = append_file(&active_path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            directory: directory.to_owned(),
            service_name,
            rotation,
            file: Some(file),
            active_path,
            active_day,
            bytes_written,
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.rotation {
            RotationPolicy::Daily if unix_day() != self.active_day => {
                if let Err(error) = self.rotate_daily() {
                    self.reopen_active();
                    return Err(error);
                }
            }
            RotationPolicy::Size { max_bytes, .. }
                if self.bytes_written > 0
                    && self.bytes_written.saturating_add(bytes.len() as u64) > max_bytes =>
            {
                if let Err(error) = self.rotate_size() {
                    self.reopen_active();
                    return Err(error);
                }
            }
            _ => {}
        }
        let file = self.file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "rotating log file is closed")
        })?;
        file.write_all(bytes)?;
        file.flush()?;
        self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn rotate_daily(&mut self) -> io::Result<()> {
        self.file.take();
        self.active_day = unix_day();
        self.active_path = log_path(
            &self.directory,
            &self.service_name,
            self.rotation,
            self.active_day,
        );
        let file = append_file(&self.active_path)?;
        self.bytes_written = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate_size(&mut self) -> io::Result<()> {
        let RotationPolicy::Size { max_backups, .. } = self.rotation else {
            return Ok(());
        };
        self.file.take();

        if max_backups == 0 {
            let backup = self
                .directory
                .join(format!("{}.{}.log", self.service_name, unix_nanos()));
            fs::rename(&self.active_path, backup)?;
        } else {
            let oldest = backup_path(&self.active_path, max_backups);
            if oldest.exists() {
                fs::remove_file(oldest)?;
            }
            for index in (1..max_backups).rev() {
                let from = backup_path(&self.active_path, index);
                if from.exists() {
                    fs::rename(from, backup_path(&self.active_path, index + 1))?;
                }
            }
            if self.active_path.exists() {
                fs::rename(&self.active_path, backup_path(&self.active_path, 1))?;
            }
        }

        self.file = Some(append_file(&self.active_path)?);
        self.bytes_written = 0;
        Ok(())
    }

    fn reopen_active(&mut self) {
        if self.file.is_none() {
            self.file = append_file(&self.active_path).ok();
        }
    }
}

fn validate_config(config: &LogConfig) -> Result<(), LogError> {
    if config.service_name.trim().is_empty() {
        return Err(LogError::EmptyServiceName);
    }
    if let LogTarget::File {
        rotation: RotationPolicy::Size { max_bytes, .. },
        ..
    } = &config.target
    {
        if *max_bytes == 0 {
            return Err(LogError::InvalidMaxSize);
        }
    }
    Ok(())
}

fn append_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn log_path(directory: &Path, service_name: &str, rotation: RotationPolicy, day: i64) -> PathBuf {
    match rotation {
        RotationPolicy::Daily => {
            directory.join(format!("{service_name}.{}.log", date_from_unix_day(day)))
        }
        RotationPolicy::Size { .. } => directory.join(format!("{service_name}.log")),
    }
}

fn backup_path(active: &Path, index: usize) -> PathBuf {
    let mut path = active.as_os_str().to_owned();
    path.push(format!(".{index}"));
    PathBuf::from(path)
}

fn safe_file_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn truncate(value: &str, limit: Option<usize>) -> String {
    let Some(limit) = limit else {
        return value.to_owned();
    };
    value.chars().take(limit).collect()
}

fn encode_plain(record: &Map<String, Value>) -> Vec<u8> {
    let timestamp = record["timestamp"].as_str().unwrap_or_default();
    let level = record["level"].as_str().unwrap_or_default();
    let service = record["service"].as_str().unwrap_or_default();
    let message = record["message"].as_str().unwrap_or_default();
    let mut output = format!("{timestamp} {level} {service}: {message}");
    for (key, value) in record {
        if matches!(key.as_str(), "timestamp" | "level" | "service" | "message") {
            continue;
        }
        output.push(' ');
        output.push_str(key);
        output.push('=');
        output.push_str(&value.to_string());
    }
    output.into_bytes()
}

fn timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs() as i64;
    let day = seconds.div_euclid(86_400);
    let seconds_in_day = seconds.rem_euclid(86_400);
    let hour = seconds_in_day / 3_600;
    let minute = seconds_in_day % 3_600 / 60;
    let second = seconds_in_day % 60;
    format!(
        "{}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        date_from_unix_day(day),
        duration.subsec_millis()
    )
}

fn unix_day() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .div_euclid(86_400) as i64
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn date_from_unix_day(day: i64) -> String {
    let day = day + 719_468;
    let era = if day >= 0 { day } else { day - 146_096 } / 146_097;
    let day_of_era = day - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Debug)]
pub enum LogError {
    EmptyServiceName,
    InvalidMaxSize,
    Io(io::Error),
    Serialize(serde_json::Error),
    Poisoned,
}

impl fmt::Display for LogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyServiceName => formatter.write_str("log service name cannot be empty"),
            Self::InvalidMaxSize => {
                formatter.write_str("log rotation maximum size must be greater than zero")
            }
            Self::Io(error) => write!(formatter, "log I/O error: {error}"),
            Self::Serialize(error) => write!(formatter, "log serialization error: {error}"),
            Self::Poisoned => formatter.write_str("log writer mutex poisoned"),
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialize(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LogError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for LogError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn emits_json_with_context_and_masks_sensitive_fields() {
        struct Credentials;
        impl Sensitive for Credentials {
            fn mask_sensitive(&self) -> Value {
                serde_json::json!({"password": "******"})
            }
        }

        let output = SharedWriter::default();
        let logger =
            Logger::to_writer(LogConfig::console("users"), output.clone()).expect("valid logger");
        let trace =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let context = LogContext::new()
            .with_field(LogField::new("request_id", "request-1"))
            .with_trace(trace);

        logger
            .log_with_context(
                LogLevel::Info,
                "authenticated",
                Some(&context),
                [LogField::sensitive("credentials", &Credentials)],
            )
            .unwrap();

        let bytes = output.0.lock().unwrap().clone();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["service"], "users");
        assert_eq!(record["request_id"], "request-1");
        assert_eq!(record["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(record["credentials"]["password"], "******");
    }

    #[test]
    fn filters_levels_truncates_content_and_samples() {
        let output = SharedWriter::default();
        let logger = Logger::to_writer(
            LogConfig::console("orders")
                .with_level(LogLevel::Warn)
                .with_max_content_length(4),
            output.clone(),
        )
        .unwrap();
        let sampler = LogSampler::new(1, 3);

        assert!(!logger.log(LogLevel::Info, "hidden", []).unwrap());
        for _ in 0..5 {
            logger
                .log_sampled(&sampler, LogLevel::Error, "abcdef", None, [])
                .unwrap();
        }

        let bytes = output.0.lock().unwrap().clone();
        let lines: Vec<_> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), 3);
        assert!(lines
            .iter()
            .all(|line| serde_json::from_slice::<Value>(line).unwrap()["message"] == "abcd"));
    }

    #[test]
    fn rotates_size_limited_files() {
        let directory = std::env::temp_dir().join(format!("rust-zero-log-{}", unix_nanos()));
        let config = LogConfig::file(
            "gateway",
            &directory,
            RotationPolicy::Size {
                max_bytes: 80,
                max_backups: 2,
            },
        );
        let logger = Logger::new(config).unwrap();

        for index in 0..4 {
            logger
                .log(
                    LogLevel::Info,
                    "a record long enough to rotate",
                    [LogField::new("index", index)],
                )
                .unwrap();
        }

        assert!(directory.join("gateway.log").exists());
        assert!(directory.join("gateway.log.1").exists());
        drop(logger);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn converts_epoch_to_expected_date() {
        assert_eq!(date_from_unix_day(0), "1970-01-01");
        assert_eq!(date_from_unix_day(20_665), "2026-07-31");
    }
}
