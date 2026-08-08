use crate::{
    CounterVec, GaugeVec, HistogramOptions, HistogramVec, Metrics, MetricsError, VectorOptions,
};
use std::{
    error::Error,
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc, watch, Mutex},
    task::{JoinError, JoinSet},
};

/// Creates a bounded, backpressure-aware queue for asynchronous service handoff.
pub fn bounded<T>(capacity: usize) -> (QueueSender<T>, QueueReceiver<T>) {
    assert!(capacity > 0, "queue capacity must be greater than zero");
    let (sender, receiver) = mpsc::channel(capacity);
    (QueueSender { sender }, QueueReceiver { receiver })
}

/// Sending half of a bounded service queue.
#[derive(Clone)]
pub struct QueueSender<T> {
    sender: mpsc::Sender<T>,
}

impl<T> QueueSender<T> {
    pub async fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        self.sender.send(value).await
    }

    pub fn try_send(&self, value: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.sender.try_send(value)
    }
}

/// Receiving half of a bounded service queue.
pub struct QueueReceiver<T> {
    receiver: mpsc::Receiver<T>,
}

impl<T> QueueReceiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.receiver.recv().await
    }
}

/// Configuration for a supervised in-process consumer queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRuntimeConfig {
    pub name: String,
    pub capacity: usize,
    pub workers: usize,
    pub event_capacity: usize,
    pub shutdown_timeout: Duration,
}

impl QueueRuntimeConfig {
    pub fn new(name: impl Into<String>, capacity: usize, workers: usize) -> Self {
        Self {
            name: name.into(),
            capacity,
            workers,
            event_capacity: 256,
            shutdown_timeout: Duration::from_secs(30),
        }
    }

    pub fn validate(&self) -> Result<(), QueueConfigError> {
        if self.name.trim().is_empty() {
            return Err(QueueConfigError::EmptyName);
        }
        if self.capacity == 0 {
            return Err(QueueConfigError::ZeroCapacity);
        }
        if self.workers == 0 {
            return Err(QueueConfigError::ZeroWorkers);
        }
        if self.event_capacity == 0 {
            return Err(QueueConfigError::ZeroEventCapacity);
        }
        if self.shutdown_timeout.is_zero() {
            return Err(QueueConfigError::ZeroShutdownTimeout);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueConfigError {
    EmptyName,
    ZeroCapacity,
    ZeroWorkers,
    ZeroEventCapacity,
    ZeroShutdownTimeout,
}

impl fmt::Display for QueueConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyName => "queue name must not be empty",
            Self::ZeroCapacity => "queue capacity must be greater than zero",
            Self::ZeroWorkers => "queue worker count must be greater than zero",
            Self::ZeroEventCapacity => "queue event capacity must be greater than zero",
            Self::ZeroShutdownTimeout => "queue shutdown timeout must be greater than zero",
        })
    }
}

impl Error for QueueConfigError {}

/// Lifecycle and processing notifications emitted by a running queue.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueEvent {
    Queued,
    Started {
        worker: usize,
    },
    Succeeded {
        worker: usize,
        elapsed: Duration,
    },
    Failed {
        worker: usize,
        elapsed: Duration,
        message: String,
    },
    Paused,
    Resumed,
    Shutdown,
}

/// Prometheus instruments shared by queue producers and consumers.
#[derive(Clone)]
pub struct QueueMetrics {
    messages: CounterVec,
    inflight: GaugeVec,
    duration: HistogramVec,
}

impl QueueMetrics {
    pub fn new(registry: &Metrics, namespace: impl Into<String>) -> Result<Self, MetricsError> {
        let namespace = namespace.into();
        let messages = registry.counter_vec(
            VectorOptions::new("messages_total", "Queue messages by queue and outcome")
                .with_namespace(namespace.clone())
                .with_subsystem("queue")
                .with_labels(["queue", "outcome"]),
        )?;
        let inflight = registry.gauge_vec(
            VectorOptions::new("inflight", "Queue messages currently being processed")
                .with_namespace(namespace.clone())
                .with_subsystem("queue")
                .with_labels(["queue"]),
        )?;
        let duration = registry.histogram_vec(
            HistogramOptions::new("processing_duration_seconds", "Queue processing latency")
                .with_vector_options(
                    VectorOptions::new("processing_duration_seconds", "Queue processing latency")
                        .with_namespace(namespace)
                        .with_subsystem("queue")
                        .with_labels(["queue", "outcome"]),
                ),
        )?;
        Ok(Self {
            messages,
            inflight,
            duration,
        })
    }

    fn message(&self, queue: &str, outcome: &str) {
        let _ = self.messages.inc(&[queue, outcome]);
    }

    fn begin(&self, queue: &str) {
        let _ = self.inflight.inc(&[queue]);
    }

    fn finish(&self, queue: &str, outcome: &str, elapsed: Duration) {
        let _ = self.inflight.add(-1.0, &[queue]);
        let _ = self.message_and_duration(queue, outcome, elapsed);
    }

    fn message_and_duration(
        &self,
        queue: &str,
        outcome: &str,
        elapsed: Duration,
    ) -> Result<(), MetricsError> {
        self.messages.inc(&[queue, outcome])?;
        self.duration
            .observe(elapsed.as_secs_f64(), &[queue, outcome])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueState {
    Running,
    Paused,
    Shutdown,
}

/// A clonable producer for a supervised queue.
pub struct QueueProducer<T> {
    name: Arc<str>,
    sender: mpsc::Sender<T>,
    events: broadcast::Sender<QueueEvent>,
    metrics: Option<QueueMetrics>,
}

impl<T> Clone for QueueProducer<T> {
    fn clone(&self) -> Self {
        Self {
            name: Arc::clone(&self.name),
            sender: self.sender.clone(),
            events: self.events.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

impl<T> QueueProducer<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.events.subscribe()
    }

    pub async fn push(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        self.sender.send(value).await?;
        self.record_queued();
        Ok(())
    }

    pub fn try_push(&self, value: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.sender.try_send(value)?;
        self.record_queued();
        Ok(())
    }

    fn record_queued(&self) {
        let _ = self.events.send(QueueEvent::Queued);
        if let Some(metrics) = &self.metrics {
            metrics.message(&self.name, "queued");
        }
    }
}

/// Starts a queue and supervises a configurable pool of consumers.
pub struct QueueRuntime;

impl QueueRuntime {
    pub fn start<T, H, Fut, E>(
        config: QueueRuntimeConfig,
        handler: H,
    ) -> Result<(QueueProducer<T>, RunningQueue), QueueConfigError>
    where
        T: Send + 'static,
        H: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: fmt::Display + Send + 'static,
    {
        Self::start_inner(config, None, handler)
    }

    pub fn start_with_metrics<T, H, Fut, E>(
        config: QueueRuntimeConfig,
        metrics: QueueMetrics,
        handler: H,
    ) -> Result<(QueueProducer<T>, RunningQueue), QueueConfigError>
    where
        T: Send + 'static,
        H: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: fmt::Display + Send + 'static,
    {
        Self::start_inner(config, Some(metrics), handler)
    }

    fn start_inner<T, H, Fut, E>(
        config: QueueRuntimeConfig,
        metrics: Option<QueueMetrics>,
        handler: H,
    ) -> Result<(QueueProducer<T>, RunningQueue), QueueConfigError>
    where
        T: Send + 'static,
        H: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: fmt::Display + Send + 'static,
    {
        config.validate()?;
        let (sender, receiver) = mpsc::channel(config.capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let handler = Arc::new(handler);
        let (state_sender, state_receiver) = watch::channel(QueueState::Running);
        let (events, _) = broadcast::channel(config.event_capacity);
        let name: Arc<str> = Arc::from(config.name.as_str());
        let mut tasks = JoinSet::new();

        for worker in 0..config.workers {
            let receiver = Arc::clone(&receiver);
            let handler = Arc::clone(&handler);
            let state = state_receiver.clone();
            let events = events.clone();
            let metrics = metrics.clone();
            let name = Arc::clone(&name);
            tasks.spawn(async move {
                worker_loop(worker, name, receiver, state, events, metrics, handler).await
            });
        }

        Ok((
            QueueProducer {
                name,
                sender,
                events: events.clone(),
                metrics,
            },
            RunningQueue {
                state: state_sender,
                events,
                tasks,
                workers: config.workers,
                shutdown_timeout: config.shutdown_timeout,
            },
        ))
    }
}

async fn worker_loop<T, H, Fut, E>(
    worker: usize,
    name: Arc<str>,
    receiver: Arc<Mutex<mpsc::Receiver<T>>>,
    mut state: watch::Receiver<QueueState>,
    events: broadcast::Sender<QueueEvent>,
    metrics: Option<QueueMetrics>,
    handler: Arc<H>,
) where
    T: Send + 'static,
    H: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: fmt::Display + Send + 'static,
{
    loop {
        let current_state = *state.borrow();
        match current_state {
            QueueState::Shutdown => return,
            QueueState::Paused => {
                if state.changed().await.is_err() {
                    return;
                }
                continue;
            }
            QueueState::Running => {}
        }

        let next = async {
            let mut receiver = receiver.lock().await;
            receiver.recv().await
        };
        let value = tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() { return; }
                continue;
            }
            value = next => value,
        };
        let Some(value) = value else {
            return;
        };

        let _ = events.send(QueueEvent::Started { worker });
        if let Some(metrics) = &metrics {
            metrics.begin(&name);
        }
        let started = Instant::now();
        match handler(value).await {
            Ok(()) => {
                let elapsed = started.elapsed();
                if let Some(metrics) = &metrics {
                    metrics.finish(&name, "succeeded", elapsed);
                }
                let _ = events.send(QueueEvent::Succeeded { worker, elapsed });
            }
            Err(error) => {
                let elapsed = started.elapsed();
                if let Some(metrics) = &metrics {
                    metrics.finish(&name, "failed", elapsed);
                }
                let _ = events.send(QueueEvent::Failed {
                    worker,
                    elapsed,
                    message: error.to_string(),
                });
            }
        }
    }
}

/// Control and supervision handle for a running queue.
pub struct RunningQueue {
    state: watch::Sender<QueueState>,
    events: broadcast::Sender<QueueEvent>,
    tasks: JoinSet<()>,
    workers: usize,
    shutdown_timeout: Duration,
}

impl RunningQueue {
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.events.subscribe()
    }

    pub fn is_paused(&self) -> bool {
        *self.state.borrow() == QueueState::Paused
    }

    pub fn pause(&self) {
        if *self.state.borrow() == QueueState::Running {
            let _ = self.state.send(QueueState::Paused);
            let _ = self.events.send(QueueEvent::Paused);
        }
    }

    pub fn resume(&self) {
        if *self.state.borrow() == QueueState::Paused {
            let _ = self.state.send(QueueState::Running);
            let _ = self.events.send(QueueEvent::Resumed);
        }
    }

    /// Requests shutdown and waits for all workers to stop within the configured deadline.
    pub async fn shutdown(mut self) -> Result<(), QueueRuntimeError> {
        let _ = self.state.send(QueueState::Shutdown);
        let _ = self.events.send(QueueEvent::Shutdown);
        self.drain().await
    }

    /// Waits for workers to exit because all producers were dropped.
    ///
    /// A panic in any worker is surfaced and causes the remaining workers to be stopped.
    pub async fn wait(mut self) -> Result<(), QueueRuntimeError> {
        while let Some(result) = self.tasks.join_next().await {
            self.workers = self.workers.saturating_sub(1);
            if let Err(error) = result {
                let _ = self.state.send(QueueState::Shutdown);
                self.tasks.abort_all();
                return Err(join_error(error));
            }
        }
        Ok(())
    }

    async fn drain(&mut self) -> Result<(), QueueRuntimeError> {
        let drain = async {
            while let Some(result) = self.tasks.join_next().await {
                self.workers = self.workers.saturating_sub(1);
                result.map_err(join_error)?;
            }
            Ok(())
        };
        match tokio::time::timeout(self.shutdown_timeout, drain).await {
            Ok(result) => result,
            Err(_) => {
                self.tasks.abort_all();
                Err(QueueRuntimeError::ShutdownTimeout {
                    remaining: self.workers,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueRuntimeError {
    WorkerPanicked(String),
    ShutdownTimeout { remaining: usize },
}

impl fmt::Display for QueueRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerPanicked(message) => write!(formatter, "queue worker panicked: {message}"),
            Self::ShutdownTimeout { remaining } => write!(
                formatter,
                "queue shutdown timed out with {remaining} worker(s) remaining"
            ),
        }
    }
}

impl Error for QueueRuntimeError {}

fn join_error(error: JoinError) -> QueueRuntimeError {
    QueueRuntimeError::WorkerPanicked(error.to_string())
}

/// Round-robin, non-blocking pusher that fails over when a queue is full or closed.
pub struct BalancedPusher<T> {
    producers: Arc<[QueueProducer<T>]>,
    next: AtomicUsize,
}

impl<T> BalancedPusher<T> {
    pub fn new(
        producers: impl IntoIterator<Item = QueueProducer<T>>,
    ) -> Result<Self, PusherConfigError> {
        let producers: Arc<[QueueProducer<T>]> = producers.into_iter().collect::<Vec<_>>().into();
        if producers.is_empty() {
            return Err(PusherConfigError::Empty);
        }
        Ok(Self {
            producers,
            next: AtomicUsize::new(0),
        })
    }

    pub fn try_push(&self, mut value: T) -> Result<usize, PushError<T>> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.producers.len();
        for offset in 0..self.producers.len() {
            let index = (start + offset) % self.producers.len();
            match self.producers[index].try_push(value) {
                Ok(()) => return Ok(index),
                Err(mpsc::error::TrySendError::Full(returned))
                | Err(mpsc::error::TrySendError::Closed(returned)) => value = returned,
            }
        }
        Err(PushError { value })
    }
}

/// Pusher that offers a clone of each message to every configured queue.
pub struct FanoutPusher<T> {
    producers: Arc<[QueueProducer<T>]>,
}

impl<T> FanoutPusher<T> {
    pub fn new(
        producers: impl IntoIterator<Item = QueueProducer<T>>,
    ) -> Result<Self, PusherConfigError> {
        let producers: Arc<[QueueProducer<T>]> = producers.into_iter().collect::<Vec<_>>().into();
        if producers.is_empty() {
            return Err(PusherConfigError::Empty);
        }
        Ok(Self { producers })
    }
}

impl<T: Clone> FanoutPusher<T> {
    /// Returns the indexes of queues that were full or closed.
    pub fn try_push(&self, value: T) -> Vec<usize> {
        self.producers
            .iter()
            .enumerate()
            .filter_map(|(index, producer)| producer.try_push(value.clone()).err().map(|_| index))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PusherConfigError {
    Empty,
}

impl fmt::Display for PusherConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("at least one queue producer is required")
    }
}

impl Error for PusherConfigError {}

#[derive(Debug)]
pub struct PushError<T> {
    pub value: T,
}

impl<T> fmt::Display for PushError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("all queue producers are full or closed")
    }
}

impl<T: fmt::Debug> Error for PushError<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn preserves_fifo_order_and_backpressure() {
        let (sender, mut receiver) = bounded(1);
        sender.try_send(1).unwrap();
        assert!(sender.try_send(2).is_err());
        assert_eq!(receiver.recv().await, Some(1));
        sender.send(2).await.unwrap();
        assert_eq!(receiver.recv().await, Some(2));
    }

    #[tokio::test]
    async fn pauses_resumes_reports_failures_and_records_metrics() {
        let registry = Metrics::new();
        let metrics = QueueMetrics::new(&registry, "test").unwrap();
        let processed = Arc::new(AtomicUsize::new(0));
        let (producer, running) =
            QueueRuntime::start_with_metrics(QueueRuntimeConfig::new("emails", 4, 2), metrics, {
                let processed = Arc::clone(&processed);
                move |value: usize| {
                    let processed = Arc::clone(&processed);
                    async move {
                        processed.fetch_add(1, Ordering::SeqCst);
                        if value == 2 {
                            Err("rejected")
                        } else {
                            Ok(())
                        }
                    }
                }
            })
            .unwrap();
        let mut events = running.subscribe();
        running.pause();
        producer.push(1).await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(processed.load(Ordering::SeqCst), 0);
        running.resume();
        producer.push(2).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while processed.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut saw_failure = false;
        while let Ok(event) = events.try_recv() {
            saw_failure |= matches!(event, QueueEvent::Failed { .. });
        }
        assert!(saw_failure);
        running.shutdown().await.unwrap();

        let rendered = registry.render();
        assert!(rendered.contains("test_queue_messages_total"));
        assert!(rendered.contains("outcome=\"succeeded\""));
        assert!(rendered.contains("outcome=\"failed\""));
    }

    #[tokio::test]
    async fn shutdown_is_bounded_when_a_handler_does_not_finish() {
        let blocked = Arc::new(Notify::new());
        let mut config = QueueRuntimeConfig::new("blocked", 1, 1);
        config.shutdown_timeout = Duration::from_millis(10);
        let (producer, running) = QueueRuntime::start(config, {
            let blocked = Arc::clone(&blocked);
            move |_: ()| {
                let blocked = Arc::clone(&blocked);
                async move {
                    blocked.notified().await;
                    Ok::<_, &'static str>(())
                }
            }
        })
        .unwrap();
        producer.push(()).await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            running.shutdown().await,
            Err(QueueRuntimeError::ShutdownTimeout { remaining: 1 })
        );
    }

    #[tokio::test]
    async fn balanced_failover_and_fanout_route_messages() {
        let (first, first_running) =
            QueueRuntime::start(QueueRuntimeConfig::new("first", 1, 1), |_: usize| async {
                Ok::<_, &'static str>(())
            })
            .unwrap();
        let (second, second_running) =
            QueueRuntime::start(QueueRuntimeConfig::new("second", 1, 1), |_: usize| async {
                Ok::<_, &'static str>(())
            })
            .unwrap();

        let balanced = BalancedPusher::new([first.clone(), second.clone()]).unwrap();
        assert_eq!(balanced.try_push(1).unwrap(), 0);
        assert_eq!(balanced.try_push(2).unwrap(), 1);

        let fanout = FanoutPusher::new([first, second]).unwrap();
        tokio::task::yield_now().await;
        assert!(fanout.try_push(3).is_empty());

        first_running.shutdown().await.unwrap();
        second_running.shutdown().await.unwrap();
    }
}
