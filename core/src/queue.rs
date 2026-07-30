use tokio::sync::mpsc;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_fifo_order_and_backpressure() {
        let (sender, mut receiver) = bounded(1);
        sender.try_send(1).unwrap();
        assert!(sender.try_send(2).is_err());
        assert_eq!(receiver.recv().await, Some(1));
        sender.send(2).await.unwrap();
        assert_eq!(receiver.recv().await, Some(2));
    }
}
