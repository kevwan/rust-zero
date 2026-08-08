use std::{error::Error, fmt, future::Future, io, pin::Pin, time::Duration};

use tokio::{
    sync::watch,
    task::{JoinError, JoinSet},
};

type BoxError = Box<dyn Error + Send + Sync>;
type ServiceFuture = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>;
type ServiceStarter = Box<dyn FnOnce(Shutdown) -> ServiceFuture + Send>;

/// A cancellation signal passed to every service in a [`ServiceGroup`].
#[derive(Clone)]
pub struct Shutdown {
    receiver: watch::Receiver<bool>,
}

impl Shutdown {
    pub fn is_requested(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn requested(&mut self) {
        if self.is_requested() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if self.is_requested() {
                return;
            }
        }
    }
}

/// A clonable handle that requests graceful shutdown of a running service group.
#[derive(Clone)]
pub struct ShutdownHandle {
    sender: watch::Sender<bool>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        let _ = self.sender.send(true);
    }

    pub fn is_shutdown(&self) -> bool {
        *self.sender.borrow()
    }
}

/// Starts and supervises a set of long-running asynchronous services.
pub struct ServiceGroup {
    services: Vec<(String, ServiceStarter)>,
    shutdown_timeout: Duration,
}

impl Default for ServiceGroup {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

impl ServiceGroup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        assert!(
            !timeout.is_zero(),
            "shutdown timeout must be greater than zero"
        );
        self.shutdown_timeout = timeout;
        self
    }

    pub fn add<F, Fut, E>(&mut self, name: impl Into<String>, service: F)
    where
        F: FnOnce(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + Sync + 'static,
    {
        let name = name.into();
        assert!(!name.trim().is_empty(), "service name cannot be empty");
        self.services.push((
            name,
            Box::new(move |shutdown| {
                Box::pin(async move {
                    service(shutdown)
                        .await
                        .map_err(|error| Box::new(error) as BoxError)
                })
            }),
        ));
    }

    pub fn start(self) -> Result<RunningServices, ServiceGroupError> {
        if self.services.is_empty() {
            return Err(ServiceGroupError::Empty);
        }

        let (sender, receiver) = watch::channel(false);
        let mut tasks = JoinSet::new();
        let service_count = self.services.len();
        for (name, start) in self.services {
            let shutdown = Shutdown {
                receiver: receiver.clone(),
            };
            tasks.spawn(async move {
                let result = start(shutdown).await;
                (name, result)
            });
        }

        Ok(RunningServices {
            handle: ShutdownHandle { sender },
            receiver,
            tasks,
            service_count,
            shutdown_timeout: self.shutdown_timeout,
        })
    }
}

pub struct RunningServices {
    handle: ShutdownHandle,
    receiver: watch::Receiver<bool>,
    tasks: JoinSet<(String, Result<(), BoxError>)>,
    service_count: usize,
    shutdown_timeout: Duration,
}

impl RunningServices {
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.handle.clone()
    }

    /// Waits until shutdown is requested or one service exits unexpectedly.
    ///
    /// If one service exits first, all siblings are asked to stop before the error is returned.
    pub async fn wait(mut self) -> Result<(), ServiceGroupError> {
        if self.handle.is_shutdown() {
            return self.drain(None).await;
        }

        tokio::select! {
            changed = self.receiver.changed() => {
                if changed.is_err() {
                    return Err(ServiceGroupError::SignalClosed);
                }
                self.drain(None).await
            }
            result = self.tasks.join_next() => {
                let failure = match result {
                    Some(Ok((name, Ok(())))) => ServiceGroupError::UnexpectedExit(name),
                    Some(Ok((name, Err(error)))) => ServiceGroupError::ServiceFailed {
                        name,
                        message: error.to_string(),
                    },
                    Some(Err(error)) => join_error(error),
                    None => return Ok(()),
                };
                self.handle.shutdown();
                self.service_count = self.service_count.saturating_sub(1);
                self.drain(Some(failure)).await
            }
        }
    }

    /// Waits until SIGINT/SIGTERM requests shutdown or one service exits unexpectedly.
    ///
    /// Receiving a process signal asks every service to stop and then applies the group's
    /// configured shutdown timeout while draining their tasks. On platforms without Unix
    /// signals, Ctrl-C is used as the shutdown signal.
    pub async fn wait_for_signal(self) -> Result<(), ServiceGroupError> {
        self.wait_for_signal_from(wait_for_shutdown_signal()).await
    }

    async fn wait_for_signal_from<F>(self, signal: F) -> Result<(), ServiceGroupError>
    where
        F: Future<Output = io::Result<ShutdownSignal>>,
    {
        let handle = self.shutdown_handle();
        let wait = self.wait();
        tokio::pin!(signal);
        tokio::pin!(wait);

        tokio::select! {
            result = &mut wait => result,
            received = &mut signal => {
                handle.shutdown();
                match received {
                    Ok(_) => wait.await,
                    Err(error) => {
                        let _ = wait.await;
                        Err(ServiceGroupError::Signal(error.to_string()))
                    }
                }
            }
        }
    }

    async fn drain(
        &mut self,
        initial_error: Option<ServiceGroupError>,
    ) -> Result<(), ServiceGroupError> {
        let mut first_error = initial_error;
        let drain = async {
            while let Some(result) = self.tasks.join_next().await {
                self.service_count = self.service_count.saturating_sub(1);
                match result {
                    Ok((_name, Ok(()))) => {}
                    Ok((name, Err(error))) if first_error.is_none() => {
                        first_error = Some(ServiceGroupError::ServiceFailed {
                            name,
                            message: error.to_string(),
                        });
                    }
                    Err(error) if first_error.is_none() => {
                        first_error = Some(join_error(error));
                    }
                    _ => {}
                }
            }
        };

        if tokio::time::timeout(self.shutdown_timeout, drain)
            .await
            .is_err()
        {
            self.tasks.abort_all();
            return Err(ServiceGroupError::ShutdownTimeout {
                remaining: self.service_count,
            });
        }

        first_error.map_or(Ok(()), Err)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceGroupError {
    Empty,
    UnexpectedExit(String),
    ServiceFailed { name: String, message: String },
    ServicePanicked(String),
    ShutdownTimeout { remaining: usize },
    SignalClosed,
    Signal(String),
}

impl fmt::Display for ServiceGroupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("service group cannot be empty"),
            Self::UnexpectedExit(name) => {
                write!(formatter, "service {name} exited before shutdown")
            }
            Self::ServiceFailed { name, message } => {
                write!(formatter, "service {name} failed: {message}")
            }
            Self::ServicePanicked(message) => write!(formatter, "service task panicked: {message}"),
            Self::ShutdownTimeout { remaining } => write!(
                formatter,
                "service shutdown timed out with {remaining} task(s) still running"
            ),
            Self::SignalClosed => formatter.write_str("service shutdown signal closed"),
            Self::Signal(message) => {
                write!(
                    formatter,
                    "failed to listen for a process shutdown signal: {message}"
                )
            }
        }
    }
}

impl Error for ServiceGroupError {}

fn join_error(error: JoinError) -> ServiceGroupError {
    ServiceGroupError::ServicePanicked(error.to_string())
}

/// A process signal that conventionally requests graceful shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

/// Waits for SIGINT or SIGTERM without terminating the process immediately.
///
/// Callers that do not use [`ServiceGroup`] can use this function to connect process lifecycle
/// events to their own cancellation mechanism.
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() -> io::Result<ShutdownSignal> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => Ok(ShutdownSignal::Interrupt),
        _ = terminate.recv() => Ok(ShutdownSignal::Terminate),
    }
}

#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> io::Result<ShutdownSignal> {
    tokio::signal::ctrl_c().await?;
    Ok(ShutdownSignal::Interrupt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io, sync::Arc};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn gracefully_stops_all_services() {
        let stopped = Arc::new(Notify::new());
        let mut group = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(1));
        group.add("http", {
            let stopped = Arc::clone(&stopped);
            move |mut shutdown| async move {
                shutdown.requested().await;
                stopped.notify_one();
                Ok::<_, io::Error>(())
            }
        });

        let running = group.start().unwrap();
        let handle = running.shutdown_handle();
        handle.shutdown();
        running.wait().await.unwrap();
        stopped.notified().await;
    }

    #[tokio::test]
    async fn one_failure_stops_sibling_services() {
        let sibling_stopped = Arc::new(Notify::new());
        let mut group = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(1));
        group.add("failing", |_| async {
            Err::<(), _>(io::Error::other("database unavailable"))
        });
        group.add("sibling", {
            let sibling_stopped = Arc::clone(&sibling_stopped);
            move |mut shutdown| async move {
                shutdown.requested().await;
                sibling_stopped.notify_one();
                Ok::<_, io::Error>(())
            }
        });

        let error = group.start().unwrap().wait().await.unwrap_err();
        assert_eq!(
            error,
            ServiceGroupError::ServiceFailed {
                name: "failing".to_owned(),
                message: "database unavailable".to_owned(),
            }
        );
        sibling_stopped.notified().await;
    }

    #[tokio::test]
    async fn process_signal_gracefully_stops_all_services() {
        let stopped = Arc::new(Notify::new());
        let mut group = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(1));
        group.add("worker", {
            let stopped = Arc::clone(&stopped);
            move |mut shutdown| async move {
                shutdown.requested().await;
                stopped.notify_one();
                Ok::<_, io::Error>(())
            }
        });

        group
            .start()
            .unwrap()
            .wait_for_signal_from(async { Ok(ShutdownSignal::Terminate) })
            .await
            .unwrap();
        stopped.notified().await;
    }

    #[tokio::test]
    async fn signal_registration_failure_still_stops_services() {
        let stopped = Arc::new(Notify::new());
        let mut group = ServiceGroup::new().with_shutdown_timeout(Duration::from_secs(1));
        group.add("worker", {
            let stopped = Arc::clone(&stopped);
            move |mut shutdown| async move {
                shutdown.requested().await;
                stopped.notify_one();
                Ok::<_, io::Error>(())
            }
        });

        let error = group
            .start()
            .unwrap()
            .wait_for_signal_from(async { Err(io::Error::other("signals unavailable")) })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ServiceGroupError::Signal("signals unavailable".to_owned())
        );
        stopped.notified().await;
    }

    #[test]
    fn rejects_empty_groups() {
        assert!(matches!(
            ServiceGroup::new().start(),
            Err(ServiceGroupError::Empty)
        ));
    }
}
