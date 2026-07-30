use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    hash::Hash,
    sync::{Arc, Mutex},
};

use tokio::sync::oneshot;

type Waiter<V, E> = oneshot::Sender<Result<V, SingleFlightError<E>>>;

/// Coalesces concurrent operations for the same key so only one operation executes.
pub struct SingleFlight<K, V, E> {
    flights: Mutex<HashMap<K, Vec<Waiter<V, E>>>>,
}

impl<K, V, E> Default for SingleFlight<K, V, E>
where
    K: Eq + Hash,
{
    fn default() -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V, E> SingleFlight<K, V, E>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `operation` unless another call for `key` is already in progress.
    ///
    /// Concurrent callers receive the leader's result. If the leader is cancelled before it
    /// finishes, waiting callers receive [`SingleFlightError::LeaderCancelled`].
    pub async fn execute<F, Fut>(&self, key: K, operation: F) -> Result<V, SingleFlightError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let receiver = {
            let mut flights = self.flights.lock().expect("single-flight mutex poisoned");

            if let Some(waiters) = flights.get_mut(&key) {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                Some(receiver)
            } else {
                flights.insert(key.clone(), Vec::new());
                None
            }
        };

        if let Some(receiver) = receiver {
            return receiver
                .await
                .expect("single-flight leader must notify all waiting callers");
        }

        let mut guard = FlightGuard {
            flights: &self.flights,
            key: key.clone(),
            armed: true,
        };
        let result = operation()
            .await
            .map_err(|error| SingleFlightError::Operation(Arc::new(error)));

        let waiters = self
            .flights
            .lock()
            .expect("single-flight mutex poisoned")
            .remove(&key)
            .expect("single-flight leader must have an active flight");
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
        guard.armed = false;

        result
    }
}

struct FlightGuard<'a, K: Eq + Hash, V, E> {
    flights: &'a Mutex<HashMap<K, Vec<Waiter<V, E>>>>,
    key: K,
    armed: bool,
}

impl<K, V, E> Drop for FlightGuard<'_, K, V, E>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        if let Ok(mut flights) = self.flights.lock() {
            if let Some(waiters) = flights.remove(&self.key) {
                for waiter in waiters {
                    let _ = waiter.send(Err(SingleFlightError::LeaderCancelled));
                }
            }
        }
    }
}

/// Errors returned by [`SingleFlight::execute`].
#[derive(Debug, PartialEq, Eq)]
pub enum SingleFlightError<E> {
    Operation(Arc<E>),
    LeaderCancelled,
}

impl<E> Clone for SingleFlightError<E> {
    fn clone(&self) -> Self {
        match self {
            Self::Operation(error) => Self::Operation(Arc::clone(error)),
            Self::LeaderCancelled => Self::LeaderCancelled,
        }
    }
}

impl<E> fmt::Display for SingleFlightError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(error) => write!(formatter, "single-flight operation failed: {error}"),
            Self::LeaderCancelled => formatter.write_str("single-flight leader was cancelled"),
        }
    }
}

impl<E> Error for SingleFlightError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Operation(error) => Some(error.as_ref()),
            Self::LeaderCancelled => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SingleFlight, SingleFlightError};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::Notify;

    #[tokio::test]
    async fn coalesces_concurrent_calls_for_the_same_key() {
        let flights = Arc::new(SingleFlight::<String, usize, String>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let leader_started = Arc::new(Notify::new());
        let release_leader = Arc::new(Notify::new());

        let first = {
            let flights = Arc::clone(&flights);
            let calls = Arc::clone(&calls);
            let leader_started = Arc::clone(&leader_started);
            let release_leader = Arc::clone(&release_leader);
            tokio::spawn(async move {
                flights
                    .execute("profile:42".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        leader_started.notify_one();
                        release_leader.notified().await;
                        Ok(42)
                    })
                    .await
            })
        };
        leader_started.notified().await;

        let second = {
            let flights = Arc::clone(&flights);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                flights
                    .execute("profile:42".to_owned(), || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(42)
                    })
                    .await
            })
        };

        wait_for_waiter(&flights).await;
        release_leader.notify_one();

        assert_eq!(first.await.unwrap().unwrap(), 42);
        assert_eq!(second.await.unwrap().unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn propagates_the_leader_error_to_waiting_callers() {
        let flights = Arc::new(SingleFlight::<String, usize, String>::new());
        let leader_started = Arc::new(Notify::new());
        let release_leader = Arc::new(Notify::new());

        let leader = {
            let flights = Arc::clone(&flights);
            let leader_started = Arc::clone(&leader_started);
            let release_leader = Arc::clone(&release_leader);
            tokio::spawn(async move {
                flights
                    .execute("profile:42".to_owned(), || async move {
                        leader_started.notify_one();
                        release_leader.notified().await;
                        Err("database unavailable".to_owned())
                    })
                    .await
            })
        };
        leader_started.notified().await;

        let waiter = {
            let flights = Arc::clone(&flights);
            tokio::spawn(async move {
                flights
                    .execute("profile:42".to_owned(), || async { Ok(42) })
                    .await
            })
        };

        wait_for_waiter(&flights).await;
        release_leader.notify_one();

        for result in [leader.await.unwrap(), waiter.await.unwrap()] {
            assert_eq!(
                result,
                Err(SingleFlightError::Operation(Arc::new(
                    "database unavailable".to_owned()
                )))
            );
        }
    }

    #[tokio::test]
    async fn allows_a_new_call_after_a_completed_flight() {
        let flights = SingleFlight::<String, usize, String>::new();
        let calls = AtomicUsize::new(0);

        for expected in [1, 2] {
            let result = flights
                .execute("profile:42".to_owned(), || async {
                    Ok(calls.fetch_add(1, Ordering::SeqCst) + 1)
                })
                .await;
            assert_eq!(result, Ok(expected));
        }
    }

    async fn wait_for_waiter(flights: &SingleFlight<String, usize, String>) {
        loop {
            if flights
                .flights
                .lock()
                .expect("single-flight mutex poisoned")
                .get("profile:42")
                .is_some_and(|waiters| waiters.len() == 1)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}
