use std::{
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time,
};

/// Buffers work and executes it in bounded batches by size or elapsed time.
pub struct BatchExecutor<T> {
    sender: mpsc::Sender<Command<T>>,
    worker: JoinHandle<()>,
}

/// Buffers weighted items and flushes when their combined size reaches a byte limit.
pub struct ChunkExecutor<T> {
    sender: mpsc::Sender<ChunkCommand<T>>,
    worker: JoinHandle<()>,
}

/// Coalesces repeated triggers into at most one delayed execution.
pub struct DelayExecutor {
    trigger: mpsc::Sender<()>,
    triggered: Arc<AtomicBool>,
    shutdown: oneshot::Sender<()>,
    worker: JoinHandle<Result<(), String>>,
}

/// Executes at most once during each threshold window.
pub struct LessExecutor {
    threshold: Duration,
    last_execution: Mutex<Option<Instant>>,
}

impl LessExecutor {
    pub fn new(threshold: Duration) -> Self {
        assert!(!threshold.is_zero(), "threshold must be greater than zero");
        Self {
            threshold,
            last_execution: Mutex::new(None),
        }
    }

    /// Runs `execute` when the threshold has elapsed, returning whether it ran.
    pub fn do_or_discard<F>(&self, execute: F) -> bool
    where
        F: FnOnce(),
    {
        let now = Instant::now();
        {
            let mut last = self
                .last_execution
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.is_some_and(|last| now.duration_since(last) <= self.threshold) {
                return false;
            }
            *last = Some(now);
        }
        execute();
        true
    }
}

impl DelayExecutor {
    pub fn new<F, Fut, E>(delay: Duration, mut execute: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: fmt::Display,
    {
        assert!(!delay.is_zero(), "delay must be greater than zero");
        let (trigger, mut triggers) = mpsc::channel(1);
        let triggered = Arc::new(AtomicBool::new(false));
        let worker_triggered = Arc::clone(&triggered);
        let (shutdown, mut stopping) = oneshot::channel();
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopping => return Ok(()),
                    trigger = triggers.recv() => {
                        if trigger.is_none() {
                            return Ok(());
                        }
                        tokio::select! {
                            _ = &mut stopping => return Ok(()),
                            _ = time::sleep(delay) => {}
                        }
                        // Match go-zero: allow a trigger made by the job to schedule another run.
                        worker_triggered.store(false, Ordering::Release);
                        execute().await.map_err(|error| error.to_string())?;
                    }
                }
            }
        });
        Self {
            trigger,
            triggered,
            shutdown,
            worker,
        }
    }

    /// Schedules the job unless a delayed execution is already pending.
    pub fn trigger(&self) -> bool {
        if self
            .triggered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if self.trigger.try_send(()).is_err() {
            self.triggered.store(false, Ordering::Release);
            return false;
        }
        true
    }

    pub async fn shutdown(self, timeout: Duration) -> Result<(), DelayExecutorError> {
        let Self {
            trigger,
            triggered: _,
            shutdown,
            mut worker,
        } = self;
        drop(trigger);
        let _ = shutdown.send(());
        match time::timeout(timeout, &mut worker).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(DelayExecutorError::Job(error)),
            Ok(Err(error)) => Err(DelayExecutorError::Worker(error.to_string())),
            Err(_) => {
                worker.abort();
                let _ = worker.await;
                Err(DelayExecutorError::TimedOut(timeout))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelayExecutorError {
    Job(String),
    TimedOut(Duration),
    Worker(String),
}

impl fmt::Display for DelayExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Job(error) => write!(formatter, "delayed job failed: {error}"),
            Self::TimedOut(timeout) => {
                write!(formatter, "delay executor did not stop within {timeout:?}")
            }
            Self::Worker(error) => write!(formatter, "delay executor worker failed: {error}"),
        }
    }
}

impl std::error::Error for DelayExecutorError {}

/// Runs one asynchronous job at a fixed interval until shutdown or the first job failure.
///
/// Jobs never overlap: the next interval is observed only after the current invocation
/// completes. Shutdown is bounded and aborts a job that does not finish within the caller's
/// deadline.
pub struct PeriodicExecutor {
    shutdown: oneshot::Sender<()>,
    worker: JoinHandle<Result<(), String>>,
}

impl PeriodicExecutor {
    pub fn new<F, Fut, E>(interval: Duration, mut execute: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: fmt::Display,
    {
        assert!(!interval.is_zero(), "interval must be greater than zero");
        let (shutdown, mut stopping) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let start = time::Instant::now() + interval;
            let mut ticker = time::interval_at(start, interval);
            ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = &mut stopping => return Ok(()),
                    _ = ticker.tick() => {
                        execute().await.map_err(|error| error.to_string())?;
                    }
                }
            }
        });
        Self { shutdown, worker }
    }

    /// Requests shutdown and waits no longer than `timeout` for an active job to finish.
    pub async fn shutdown(self, timeout: Duration) -> Result<(), PeriodicExecutorError> {
        let Self {
            shutdown,
            mut worker,
        } = self;
        let _ = shutdown.send(());

        match time::timeout(timeout, &mut worker).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(PeriodicExecutorError::Job(error)),
            Ok(Err(error)) => Err(PeriodicExecutorError::Worker(error.to_string())),
            Err(_) => {
                worker.abort();
                let _ = worker.await;
                Err(PeriodicExecutorError::TimedOut(timeout))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeriodicExecutorError {
    Job(String),
    TimedOut(Duration),
    Worker(String),
}

impl fmt::Display for PeriodicExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Job(error) => write!(formatter, "periodic job failed: {error}"),
            Self::TimedOut(timeout) => {
                write!(
                    formatter,
                    "periodic executor did not stop within {timeout:?}"
                )
            }
            Self::Worker(error) => write!(formatter, "periodic executor worker failed: {error}"),
        }
    }
}

impl std::error::Error for PeriodicExecutorError {}

impl<T> BatchExecutor<T>
where
    T: Send + 'static,
{
    pub fn new<F, Fut>(max_batch_size: usize, flush_interval: Duration, execute: F) -> Self
    where
        F: FnMut(Vec<T>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        assert!(
            max_batch_size > 0,
            "maximum batch size must be greater than zero"
        );
        assert!(
            !flush_interval.is_zero(),
            "flush interval must be greater than zero"
        );

        let (sender, receiver) = mpsc::channel(max_batch_size.saturating_mul(2).max(1));
        let worker = tokio::spawn(run_worker(
            receiver,
            max_batch_size,
            flush_interval,
            execute,
        ));
        Self { sender, worker }
    }

    /// Queues one item, applying backpressure when the input buffer is full.
    pub async fn push(&self, value: T) -> Result<(), BatchExecutorError> {
        self.sender
            .send(Command::Item(value))
            .await
            .map_err(|_| BatchExecutorError::Closed)
    }

    /// Executes all items accepted before this call and waits for completion.
    pub async fn flush(&self) -> Result<(), BatchExecutorError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Command::Flush(sender))
            .await
            .map_err(|_| BatchExecutorError::Closed)?;
        receiver.await.map_err(|_| BatchExecutorError::Closed)
    }

    /// Flushes pending work and stops the worker.
    pub async fn shutdown(self) -> Result<(), BatchExecutorError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Command::Shutdown(sender))
            .await
            .map_err(|_| BatchExecutorError::Closed)?;
        receiver.await.map_err(|_| BatchExecutorError::Closed)?;
        self.worker
            .await
            .map_err(|error| BatchExecutorError::Worker(error.to_string()))
    }
}

impl<T> ChunkExecutor<T>
where
    T: Send + 'static,
{
    pub fn new<F, Fut>(max_chunk_bytes: usize, flush_interval: Duration, execute: F) -> Self
    where
        F: FnMut(Vec<T>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        assert!(max_chunk_bytes > 0, "chunk size must be greater than zero");
        assert!(
            !flush_interval.is_zero(),
            "flush interval must be greater than zero"
        );

        let (sender, receiver) = mpsc::channel(128);
        let worker = tokio::spawn(run_chunk_worker(
            receiver,
            max_chunk_bytes,
            flush_interval,
            execute,
        ));
        Self { sender, worker }
    }

    /// Queues an item with its accounting size, applying backpressure when full.
    pub async fn push(&self, value: T, size: usize) -> Result<(), BatchExecutorError> {
        self.sender
            .send(ChunkCommand::Item { value, size })
            .await
            .map_err(|_| BatchExecutorError::Closed)
    }

    /// Executes all items accepted before this call and waits for completion.
    pub async fn flush(&self) -> Result<(), BatchExecutorError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(ChunkCommand::Flush(sender))
            .await
            .map_err(|_| BatchExecutorError::Closed)?;
        receiver.await.map_err(|_| BatchExecutorError::Closed)
    }

    /// Flushes pending work and stops the worker.
    pub async fn shutdown(self) -> Result<(), BatchExecutorError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(ChunkCommand::Shutdown(sender))
            .await
            .map_err(|_| BatchExecutorError::Closed)?;
        receiver.await.map_err(|_| BatchExecutorError::Closed)?;
        self.worker
            .await
            .map_err(|error| BatchExecutorError::Worker(error.to_string()))
    }
}

enum Command<T> {
    Item(T),
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

enum ChunkCommand<T> {
    Item { value: T, size: usize },
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

async fn run_worker<T, F, Fut>(
    mut receiver: mpsc::Receiver<Command<T>>,
    max_batch_size: usize,
    flush_interval: Duration,
    mut execute: F,
) where
    T: Send + 'static,
    F: FnMut(Vec<T>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut batch = Vec::with_capacity(max_batch_size);
    let mut ticker = time::interval(flush_interval);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            command = receiver.recv() => {
                match command {
                    Some(Command::Item(value)) => {
                        batch.push(value);
                        if batch.len() >= max_batch_size {
                            execute(std::mem::take(&mut batch)).await;
                            batch.reserve(max_batch_size);
                        }
                    }
                    Some(Command::Flush(done)) => {
                        flush_batch(&mut batch, &mut execute).await;
                        let _ = done.send(());
                    }
                    Some(Command::Shutdown(done)) => {
                        flush_batch(&mut batch, &mut execute).await;
                        let _ = done.send(());
                        return;
                    }
                    None => {
                        flush_batch(&mut batch, &mut execute).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                flush_batch(&mut batch, &mut execute).await;
            }
        }
    }
}

async fn run_chunk_worker<T, F, Fut>(
    mut receiver: mpsc::Receiver<ChunkCommand<T>>,
    max_chunk_bytes: usize,
    flush_interval: Duration,
    mut execute: F,
) where
    T: Send + 'static,
    F: FnMut(Vec<T>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut chunk = Vec::new();
    let mut chunk_bytes = 0usize;
    let mut ticker = time::interval(flush_interval);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            command = receiver.recv() => {
                match command {
                    Some(ChunkCommand::Item { value, size }) => {
                        chunk.push(value);
                        chunk_bytes = chunk_bytes.saturating_add(size);
                        if chunk_bytes >= max_chunk_bytes {
                            execute(std::mem::take(&mut chunk)).await;
                            chunk_bytes = 0;
                        }
                    }
                    Some(ChunkCommand::Flush(done)) => {
                        flush_batch(&mut chunk, &mut execute).await;
                        chunk_bytes = 0;
                        let _ = done.send(());
                    }
                    Some(ChunkCommand::Shutdown(done)) => {
                        flush_batch(&mut chunk, &mut execute).await;
                        let _ = done.send(());
                        return;
                    }
                    None => {
                        flush_batch(&mut chunk, &mut execute).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                flush_batch(&mut chunk, &mut execute).await;
                chunk_bytes = 0;
            }
        }
    }
}

async fn flush_batch<T, F, Fut>(batch: &mut Vec<T>, execute: &mut F)
where
    F: FnMut(Vec<T>) -> Fut,
    Fut: Future<Output = ()>,
{
    if !batch.is_empty() {
        execute(std::mem::take(batch)).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchExecutorError {
    Closed,
    Worker(String),
}

impl fmt::Display for BatchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("batch executor is closed"),
            Self::Worker(error) => write!(formatter, "batch executor worker failed: {error}"),
        }
    }
}

impl std::error::Error for BatchExecutorError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn flushes_when_the_batch_reaches_its_limit() {
        let (batches, mut received) = mpsc::unbounded_channel();
        let executor = BatchExecutor::new(3, Duration::from_secs(60), move |batch| {
            let batches = batches.clone();
            async move {
                batches.send(batch).unwrap();
            }
        });

        executor.push(1).await.unwrap();
        executor.push(2).await.unwrap();
        executor.push(3).await.unwrap();

        assert_eq!(received.recv().await.unwrap(), vec![1, 2, 3]);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_flush_and_shutdown_do_not_lose_work() {
        let (batches, mut received) = mpsc::unbounded_channel();
        let executor = BatchExecutor::new(10, Duration::from_secs(60), move |batch| {
            let batches = batches.clone();
            async move {
                batches.send(batch).unwrap();
            }
        });

        executor.push("first").await.unwrap();
        executor.flush().await.unwrap();
        assert_eq!(received.recv().await.unwrap(), vec!["first"]);

        executor.push("second").await.unwrap();
        executor.shutdown().await.unwrap();
        assert_eq!(received.recv().await.unwrap(), vec!["second"]);
    }

    #[tokio::test]
    async fn chunk_executor_flushes_on_combined_size() {
        let (chunks, mut received) = mpsc::unbounded_channel();
        let executor = ChunkExecutor::new(10, Duration::from_secs(60), move |chunk| {
            let chunks = chunks.clone();
            async move {
                chunks.send(chunk).unwrap();
            }
        });

        executor.push("small", 4).await.unwrap();
        executor.push("large", 6).await.unwrap();
        assert_eq!(received.recv().await.unwrap(), vec!["small", "large"]);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn chunk_executor_flushes_on_interval_and_shutdown() {
        let (chunks, mut received) = mpsc::unbounded_channel();
        let executor = ChunkExecutor::new(100, Duration::from_millis(5), move |chunk| {
            let chunks = chunks.clone();
            async move {
                chunks.send(chunk).unwrap();
            }
        });

        executor.push(1, 1).await.unwrap();
        assert_eq!(received.recv().await.unwrap(), vec![1]);
        executor.push(2, 1).await.unwrap();
        executor.shutdown().await.unwrap();
        assert_eq!(received.recv().await.unwrap(), vec![2]);
    }

    #[tokio::test]
    async fn delay_executor_coalesces_pending_triggers_and_can_run_again() {
        let (runs, mut received) = mpsc::unbounded_channel();
        let executor = DelayExecutor::new(Duration::from_millis(5), move || {
            let runs = runs.clone();
            async move {
                runs.send(()).unwrap();
                Ok::<_, &'static str>(())
            }
        });

        assert!(executor.trigger());
        assert!(!executor.trigger());
        time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(executor.trigger());
        time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        executor.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn delay_executor_reports_job_failure() {
        let executor = DelayExecutor::new(Duration::from_millis(1), || async {
            Err::<(), _>("write failed")
        });
        assert!(executor.trigger());
        time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            executor.shutdown(Duration::from_secs(1)).await,
            Err(DelayExecutorError::Job("write failed".to_owned()))
        );
    }

    #[test]
    fn less_executor_discards_calls_inside_the_threshold() {
        let executor = LessExecutor::new(Duration::from_millis(10));
        let mut runs = 0;
        assert!(executor.do_or_discard(|| runs += 1));
        assert!(!executor.do_or_discard(|| runs += 1));
        std::thread::sleep(Duration::from_millis(15));
        assert!(executor.do_or_discard(|| runs += 1));
        assert_eq!(runs, 2);
    }

    #[tokio::test]
    async fn periodic_executor_runs_repeatedly_and_stops() {
        let (runs, mut received) = mpsc::unbounded_channel();
        let executor = PeriodicExecutor::new(Duration::from_millis(5), move || {
            let runs = runs.clone();
            async move {
                runs.send(()).unwrap();
                Ok::<_, &'static str>(())
            }
        });

        time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        executor.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn periodic_executor_reports_job_failure() {
        let executor = PeriodicExecutor::new(Duration::from_millis(1), || async {
            Err::<(), _>("backend unavailable")
        });
        time::sleep(Duration::from_millis(10)).await;

        assert_eq!(
            executor.shutdown(Duration::from_secs(1)).await,
            Err(PeriodicExecutorError::Job("backend unavailable".to_owned()))
        );
    }

    #[tokio::test]
    async fn periodic_executor_bounds_slow_shutdown() {
        let (started, start_received) = oneshot::channel();
        let mut started = Some(started);
        let executor = PeriodicExecutor::new(Duration::from_millis(1), move || {
            if let Some(started) = started.take() {
                let _ = started.send(());
            }
            async {
                std::future::pending::<()>().await;
                Ok::<_, &'static str>(())
            }
        });
        time::timeout(Duration::from_secs(1), start_received)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            executor.shutdown(Duration::from_millis(5)).await,
            Err(PeriodicExecutorError::TimedOut(Duration::from_millis(5)))
        );
    }
}
