//! CPU-triggered continuous profiling exported to Grafana Pyroscope.

use crate::load::{CpuSource, ProcessCpuSource};
use pyroscope::{
    backend::{pprof_backend, BackendConfig, PprofConfig},
    pyroscope::{
        PyroscopeAgent, PyroscopeAgentBuilder, PyroscopeAgentReady, PyroscopeAgentRunning,
    },
    PyroscopeError,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn default_sample_rate() -> u32 {
    100
}

fn default_check_interval_ms() -> u64 {
    10_000
}

fn default_profiling_duration_ms() -> u64 {
    120_000
}

fn default_cpu_threshold() -> f64 {
    0.7
}

/// Configuration for overload-triggered continuous profiling.
///
/// Profiling remains idle until process CPU reaches `cpu_threshold`, then samples for one
/// `profiling_duration_ms` window and uploads profiles to the configured Pyroscope server.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ContinuousProfileConfig {
    pub server_address: String,
    pub application_name: String,
    #[serde(default)]
    pub auth_user: Option<String>,
    #[serde(default)]
    pub auth_password: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_check_interval_ms")]
    pub check_interval_ms: u64,
    #[serde(default = "default_profiling_duration_ms")]
    pub profiling_duration_ms: u64,
    #[serde(default = "default_cpu_threshold")]
    pub cpu_threshold: f64,
}

impl fmt::Debug for ContinuousProfileConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContinuousProfileConfig")
            .field("server_address", &self.server_address)
            .field("application_name", &self.application_name)
            .field("auth_user", &self.auth_user)
            .field(
                "auth_password",
                &self.auth_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("tags", &self.tags)
            .field("sample_rate", &self.sample_rate)
            .field("check_interval_ms", &self.check_interval_ms)
            .field("profiling_duration_ms", &self.profiling_duration_ms)
            .field("cpu_threshold", &self.cpu_threshold)
            .finish()
    }
}

impl ContinuousProfileConfig {
    pub fn new(server_address: impl Into<String>, application_name: impl Into<String>) -> Self {
        Self {
            server_address: server_address.into(),
            application_name: application_name.into(),
            auth_user: None,
            auth_password: None,
            tags: BTreeMap::new(),
            sample_rate: default_sample_rate(),
            check_interval_ms: default_check_interval_ms(),
            profiling_duration_ms: default_profiling_duration_ms(),
            cpu_threshold: default_cpu_threshold(),
        }
    }

    pub fn validate(&self) -> Result<(), ContinuousProfileError> {
        if self.server_address.trim().is_empty() {
            return Err(ContinuousProfileError::InvalidConfig(
                "server_address cannot be empty",
            ));
        }
        if !(self.server_address.starts_with("http://")
            || self.server_address.starts_with("https://"))
        {
            return Err(ContinuousProfileError::InvalidConfig(
                "server_address must use http:// or https://",
            ));
        }
        if self.application_name.trim().is_empty() {
            return Err(ContinuousProfileError::InvalidConfig(
                "application_name cannot be empty",
            ));
        }
        if self.sample_rate == 0 {
            return Err(ContinuousProfileError::InvalidConfig(
                "sample_rate must be greater than zero",
            ));
        }
        if self.check_interval_ms == 0 {
            return Err(ContinuousProfileError::InvalidConfig(
                "check_interval_ms must be greater than zero",
            ));
        }
        if self.profiling_duration_ms == 0 {
            return Err(ContinuousProfileError::InvalidConfig(
                "profiling_duration_ms must be greater than zero",
            ));
        }
        if !(0.0..=1.0).contains(&self.cpu_threshold) {
            return Err(ContinuousProfileError::InvalidConfig(
                "cpu_threshold must be between zero and one",
            ));
        }
        if self.auth_user.is_some() != self.auth_password.is_some() {
            return Err(ContinuousProfileError::InvalidConfig(
                "auth_user and auth_password must be configured together",
            ));
        }
        if self.tags.iter().any(|(key, _)| key.trim().is_empty()) {
            return Err(ContinuousProfileError::InvalidConfig(
                "profile tag keys cannot be empty",
            ));
        }
        Ok(())
    }
}

struct RunningAgent {
    agent: PyroscopeAgent<PyroscopeAgentRunning>,
    started_at: Instant,
}

/// Handle for a background CPU monitor and Pyroscope profiling agent.
pub struct ContinuousProfiler {
    stop: Sender<()>,
    worker: Option<JoinHandle<Result<(), ContinuousProfileError>>>,
}

impl ContinuousProfiler {
    /// Validates the configuration and starts the CPU monitor.
    pub fn start(config: ContinuousProfileConfig) -> Result<Self, ContinuousProfileError> {
        config.validate()?;
        // reqwest is built without an implicit rustls provider so enabling this feature cannot
        // conflict with a transport crate's provider. Respect an application-installed provider;
        // otherwise select the same ring provider used by rust-zero's REST stack.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("rust-zero-profiler".to_owned())
            .spawn(move || {
                let cpu = ProcessCpuSource::new();
                let check_interval = Duration::from_millis(config.check_interval_ms);
                let profiling_duration = Duration::from_millis(config.profiling_duration_ms);
                let mut agent: Option<RunningAgent> = None;

                loop {
                    match receiver.recv_timeout(check_interval) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }

                    if agent.is_none() && cpu.usage() >= config.cpu_threshold {
                        agent = Some(RunningAgent {
                            agent: build_agent(&config)?.start()?,
                            started_at: Instant::now(),
                        });
                    } else if agent
                        .as_ref()
                        .is_some_and(|running| running.started_at.elapsed() >= profiling_duration)
                    {
                        let ready = agent
                            .take()
                            .expect("running profiling agent must exist")
                            .agent
                            .stop()?;
                        ready.shutdown();
                    }
                }

                if let Some(running) = agent {
                    running.agent.stop()?.shutdown();
                }
                Ok(())
            })
            .map_err(ContinuousProfileError::Spawn)?;

        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    /// Stops monitoring, flushes the final profile if necessary, and joins the agent thread.
    pub fn shutdown(mut self) -> Result<(), ContinuousProfileError> {
        let _ = self.stop.send(());
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<(), ContinuousProfileError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| ContinuousProfileError::WorkerPanicked)?
    }
}

fn build_agent(
    config: &ContinuousProfileConfig,
) -> Result<PyroscopeAgent<PyroscopeAgentReady>, ContinuousProfileError> {
    let backend = pprof_backend(PprofConfig::default(), BackendConfig::default());
    let mut builder = PyroscopeAgentBuilder::new(
        &config.server_address,
        &config.application_name,
        config.sample_rate,
        "rust-zero",
        env!("CARGO_PKG_VERSION"),
        backend,
    );
    if let (Some(user), Some(password)) = (&config.auth_user, &config.auth_password) {
        builder = builder.basic_auth(user, password);
    }
    if !config.tags.is_empty() {
        builder = builder.tags(
            config
                .tags
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect(),
        );
    }
    Ok(builder.build()?)
}

impl Drop for ContinuousProfiler {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let _ = self.join_worker();
    }
}

#[derive(Debug)]
pub enum ContinuousProfileError {
    InvalidConfig(&'static str),
    Agent(PyroscopeError),
    Spawn(std::io::Error),
    WorkerPanicked,
}

impl fmt::Display for ContinuousProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => {
                write!(formatter, "invalid profiling config: {message}")
            }
            Self::Agent(error) => write!(formatter, "Pyroscope agent failed: {error}"),
            Self::Spawn(error) => write!(formatter, "failed to spawn profiling worker: {error}"),
            Self::WorkerPanicked => formatter.write_str("profiling worker panicked"),
        }
    }
}

impl Error for ContinuousProfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::Spawn(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PyroscopeError> for ContinuousProfileError {
    fn from(error: PyroscopeError) -> Self {
        Self::Agent(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_required_and_bounded_settings() {
        let mut config = ContinuousProfileConfig::new("http://localhost:4040", "users-api");
        assert!(config.validate().is_ok());

        config.cpu_threshold = 1.1;
        assert!(matches!(
            config.validate(),
            Err(ContinuousProfileError::InvalidConfig(_))
        ));
        config.cpu_threshold = 0.7;
        config.auth_user = Some("tenant".to_owned());
        assert!(matches!(
            config.validate(),
            Err(ContinuousProfileError::InvalidConfig(_))
        ));
    }

    #[test]
    fn debug_output_redacts_basic_auth_password() {
        let mut config = ContinuousProfileConfig::new("https://profiles.example", "payments");
        config.auth_user = Some("tenant".to_owned());
        config.auth_password = Some("super-secret".to_owned());
        let output = format!("{config:?}");
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("super-secret"));
    }

    #[test]
    fn serde_defaults_match_production_sampling_policy() {
        let config: ContinuousProfileConfig = serde_json::from_str(
            r#"{"server_address":"http://localhost:4040","application_name":"orders"}"#,
        )
        .unwrap();
        assert_eq!(config.sample_rate, 100);
        assert_eq!(config.check_interval_ms, 10_000);
        assert_eq!(config.profiling_duration_ms, 120_000);
        assert_eq!(config.cpu_threshold, 0.7);
    }

    #[test]
    fn idle_monitor_shuts_down_without_contacting_the_server() {
        let mut config = ContinuousProfileConfig::new("http://127.0.0.1:1", "idle-test");
        config.cpu_threshold = 1.0;
        config.check_interval_ms = 60_000;
        ContinuousProfiler::start(config)
            .unwrap()
            .shutdown()
            .unwrap();
    }
}
