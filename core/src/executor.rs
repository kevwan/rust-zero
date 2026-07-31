use std::{fmt, future::Future, time::Duration};

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

enum Command<T> {
    Item(T),
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
}
