use std::collections::HashMap;
use std::error::Error;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::task::JoinSet;

/// A bounded-concurrency MapReduce executor.
pub struct MapReduce<K, V, R>
where
    K: Clone + Eq + Hash + Send + 'static,
    V: Send + 'static,
    R: Send + 'static,
{
    max_concurrent_tasks: usize,
    _marker: PhantomData<(K, V, R)>,
}

impl<K, V, R> MapReduce<K, V, R>
where
    K: Clone + Eq + Hash + Send + 'static,
    V: Send + 'static,
    R: Send + 'static,
{
    /// Creates an executor that runs at most `max_concurrent_tasks` map or reduce jobs at once.
    pub fn new(max_concurrent_tasks: usize) -> Self {
        assert!(
            max_concurrent_tasks > 0,
            "maximum concurrent tasks must be greater than zero"
        );

        Self {
            max_concurrent_tasks,
            _marker: PhantomData,
        }
    }

    /// Maps the input concurrently, groups mapped values by key, and reduces each group concurrently.
    pub async fn execute<M, Red, I>(
        &self,
        input_data: I,
        map_fn: M,
        reduce_fn: Red,
    ) -> Result<HashMap<K, R>, Box<dyn Error + Send + Sync>>
    where
        I: IntoIterator<Item = V> + Send + 'static,
        M: Fn(V) -> Vec<(K, R)> + Send + Sync + 'static,
        Red: Fn(K, Vec<R>) -> R + Send + Sync + 'static,
    {
        let map_results = self.map_phase(input_data, map_fn).await?;
        let grouped_results = self.shuffle_phase(map_results);
        self.reduce_phase(grouped_results, reduce_fn).await
    }

    async fn map_phase<M, I>(
        &self,
        input_data: I,
        map_fn: M,
    ) -> Result<Vec<(K, R)>, Box<dyn Error + Send + Sync>>
    where
        I: IntoIterator<Item = V> + Send + 'static,
        M: Fn(V) -> Vec<(K, R)> + Send + Sync + 'static,
    {
        let mut jobs = JoinSet::new();
        let mut mapped = Vec::new();
        let map_fn = Arc::new(map_fn);

        for item in input_data {
            if jobs.len() == self.max_concurrent_tasks {
                mapped.extend(
                    jobs.join_next()
                        .await
                        .expect("a non-empty task set must yield a task")?,
                );
            }

            let map_fn = Arc::clone(&map_fn);
            jobs.spawn(async move { map_fn(item) });
        }

        while let Some(result) = jobs.join_next().await {
            mapped.extend(result?);
        }

        Ok(mapped)
    }

    fn shuffle_phase(&self, map_results: Vec<(K, R)>) -> HashMap<K, Vec<R>> {
        let mut grouped: HashMap<K, Vec<R>> = HashMap::new();

        for (key, value) in map_results {
            grouped.entry(key).or_default().push(value);
        }

        grouped
    }

    async fn reduce_phase<Red>(
        &self,
        grouped_results: HashMap<K, Vec<R>>,
        reduce_fn: Red,
    ) -> Result<HashMap<K, R>, Box<dyn Error + Send + Sync>>
    where
        Red: Fn(K, Vec<R>) -> R + Send + Sync + 'static,
    {
        let mut jobs = JoinSet::new();
        let mut reduced = HashMap::new();
        let reduce_fn = Arc::new(reduce_fn);

        for (key, values) in grouped_results {
            if jobs.len() == self.max_concurrent_tasks {
                let (key, value) = jobs
                    .join_next()
                    .await
                    .expect("a non-empty task set must yield a task")?;
                reduced.insert(key, value);
            }

            let reduce_fn = Arc::clone(&reduce_fn);
            jobs.spawn(async move {
                let value = reduce_fn(key.clone(), values);
                (key, value)
            });
        }

        while let Some(result) = jobs.join_next().await {
            let (key, value) = result?;
            reduced.insert(key, value);
        }

        Ok(reduced)
    }
}

#[cfg(test)]
mod tests {
    use super::MapReduce;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn counts_words_across_documents() {
        let map_reduce = MapReduce::<String, String, i32>::new(2);
        let documents = vec!["the quick brown fox".to_owned(), "the quick dog".to_owned()];

        let results = map_reduce
            .execute(
                documents,
                |document| {
                    document
                        .split_whitespace()
                        .map(|word| (word.to_owned(), 1))
                        .collect()
                },
                |_, counts| counts.into_iter().sum(),
            )
            .await
            .unwrap();

        assert_eq!(results.get("the"), Some(&2));
        assert_eq!(results.get("quick"), Some(&2));
        assert_eq!(results.get("brown"), Some(&1));
        assert_eq!(results.get("dog"), Some(&1));
    }

    #[tokio::test]
    async fn processes_more_than_channel_capacity_without_deadlocking() {
        let map_reduce = MapReduce::<usize, usize, usize>::new(1);
        let results = tokio::time::timeout(
            Duration::from_secs(1),
            map_reduce.execute(
                0..1_000,
                |value| vec![(value, value)],
                |_, values| values[0],
            ),
        )
        .await
        .expect("map-reduce must not deadlock");

        assert_eq!(results.unwrap().len(), 1_000);
    }

    #[tokio::test]
    async fn respects_the_configured_concurrency_limit() {
        let map_reduce = MapReduce::<usize, usize, usize>::new(3);
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));

        let results = map_reduce
            .execute(
                0..24,
                {
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    move |value| {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                        active.fetch_sub(1, Ordering::SeqCst);
                        vec![(value, value)]
                    }
                },
                |_, values| values[0],
            )
            .await
            .unwrap();

        assert_eq!(results.len(), 24);
        assert!(maximum.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    #[should_panic(expected = "maximum concurrent tasks must be greater than zero")]
    fn rejects_a_zero_concurrency_limit() {
        MapReduce::<usize, usize, usize>::new(0);
    }
}
