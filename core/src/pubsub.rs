use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

use tokio::sync::broadcast;

/// A typed, topic-based in-process message broker.
///
/// Slow subscribers receive [`broadcast::error::RecvError::Lagged`] rather than blocking
/// publishers, matching the fail-fast behavior expected from service event buses.
pub struct Broker<Topic, Message> {
    capacity: usize,
    topics: Arc<Mutex<HashMap<Topic, broadcast::Sender<Message>>>>,
}

impl<Topic, Message> Clone for Broker<Topic, Message> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            topics: Arc::clone(&self.topics),
        }
    }
}

impl<Topic, Message> Broker<Topic, Message>
where
    Topic: Clone + Eq + Hash,
    Message: Clone,
{
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "broker capacity must be greater than zero");
        Self {
            capacity,
            topics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn subscribe(&self, topic: Topic) -> Subscription<Message> {
        let mut topics = self.topics.lock().expect("broker lock poisoned");
        let sender = topics.entry(topic).or_insert_with(|| {
            let (sender, _) = broadcast::channel(self.capacity);
            sender
        });
        Subscription {
            receiver: sender.subscribe(),
        }
    }

    /// Publishes a message and returns the number of active subscribers.
    pub fn publish(&self, topic: Topic, message: Message) -> usize {
        let mut topics = self.topics.lock().expect("broker lock poisoned");
        let sender = topics.entry(topic).or_insert_with(|| {
            let (sender, _) = broadcast::channel(self.capacity);
            sender
        });
        sender.send(message).unwrap_or(0)
    }

    pub fn subscriber_count(&self, topic: &Topic) -> usize {
        self.topics
            .lock()
            .expect("broker lock poisoned")
            .get(topic)
            .map_or(0, broadcast::Sender::receiver_count)
    }
}

/// A subscription to one broker topic.
pub struct Subscription<Message> {
    receiver: broadcast::Receiver<Message>,
}

impl<Message> Subscription<Message>
where
    Message: Clone,
{
    pub async fn recv(&mut self) -> Result<Message, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fans_messages_out_to_topic_subscribers() {
        let broker = Broker::new(8);
        let mut first = broker.subscribe("orders");
        let mut second = broker.subscribe("orders");
        let mut unrelated = broker.subscribe("users");

        assert_eq!(broker.publish("orders", 42), 2);
        assert_eq!(first.recv().await.unwrap(), 42);
        assert_eq!(second.recv().await.unwrap(), 42);
        assert!(unrelated.receiver.try_recv().is_err());
    }
}
