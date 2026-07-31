use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const DECAY_TIME: Duration = Duration::from_secs(10);
const FORCE_PICK_AFTER: Duration = Duration::from_secs(1);

struct NodeState {
    latency_micros: f64,
    success: f64,
    last_update: Instant,
    last_pick: Instant,
}

struct Node<T> {
    value: T,
    inflight: AtomicUsize,
    requests: AtomicU64,
    state: Mutex<NodeState>,
}

impl<T> Node<T> {
    fn new(value: T) -> Self {
        let now = Instant::now();
        Self {
            value,
            inflight: AtomicUsize::new(0),
            requests: AtomicU64::new(0),
            state: Mutex::new(NodeState {
                latency_micros: 0.0,
                success: 1.0,
                last_update: now,
                last_pick: now,
            }),
        }
    }

    fn load(&self) -> f64 {
        let state = self.state.lock().expect("P2C node state lock poisoned");
        let latency = state.latency_micros.max(1.0).sqrt();
        latency * (self.inflight.load(Ordering::Relaxed) as f64 + 1.0)
    }

    fn healthy(&self) -> bool {
        self.state
            .lock()
            .expect("P2C node state lock poisoned")
            .success
            > 0.5
    }

    fn mark_picked(&self) {
        self.state
            .lock()
            .expect("P2C node state lock poisoned")
            .last_pick = Instant::now();
        self.inflight.fetch_add(1, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    fn complete(&self, started: Instant, success: bool) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut state = self.state.lock().expect("P2C node state lock poisoned");
        let elapsed = now.saturating_duration_since(state.last_update);
        let weight = (-elapsed.as_secs_f64() / DECAY_TIME.as_secs_f64()).exp();
        let latency = now.saturating_duration_since(started).as_micros() as f64;

        state.latency_micros = if state.latency_micros == 0.0 {
            latency
        } else {
            state.latency_micros * weight + latency * (1.0 - weight)
        };
        let outcome = if success { 1.0 } else { 0.0 };
        state.success = state.success * weight + outcome * (1.0 - weight);
        state.last_update = now;
    }
}

/// Power-of-two-choices load balancer with latency EWMA and inflight weighting.
///
/// Each pick returns a tracked request. Completing it feeds latency and health
/// back into later choices; dropping it without completion records a failure.
pub struct P2cBalancer<T> {
    nodes: Arc<[Arc<Node<T>>]>,
    random: AtomicU64,
}

impl<T> P2cBalancer<T> {
    pub fn new(nodes: impl IntoIterator<Item = T>) -> Result<Self, BalancerError> {
        let nodes: Vec<_> = nodes
            .into_iter()
            .map(|node| Arc::new(Node::new(node)))
            .collect();
        if nodes.is_empty() {
            return Err(BalancerError::Empty);
        }

        Ok(Self {
            nodes: nodes.into(),
            random: AtomicU64::new(0x4d59_5df4_d0f3_3173),
        })
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn pick(&self) -> P2cRequest<T> {
        let selected = match self.nodes.len() {
            1 => Arc::clone(&self.nodes[0]),
            len => {
                let first = self.next_index(len);
                let mut second = self.next_index(len - 1);
                if second >= first {
                    second += 1;
                }
                choose(&self.nodes[first], &self.nodes[second])
            }
        };

        selected.mark_picked();
        P2cRequest {
            node: Some(selected),
            started: Instant::now(),
        }
    }

    fn next_index(&self, len: usize) -> usize {
        let mut current = self.random.load(Ordering::Relaxed);
        loop {
            let mut next = current;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            match self.random.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next as usize % len,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn snapshots(&self) -> Vec<NodeSnapshot<'_, T>> {
        self.nodes
            .iter()
            .map(|node| {
                let state = node.state.lock().expect("P2C node state lock poisoned");
                NodeSnapshot {
                    value: &node.value,
                    inflight: node.inflight.load(Ordering::Relaxed),
                    requests: node.requests.load(Ordering::Relaxed),
                    latency: Duration::from_secs_f64(state.latency_micros / 1_000_000.0),
                    success_rate: state.success,
                }
            })
            .collect()
    }
}

fn choose<T>(first: &Arc<Node<T>>, second: &Arc<Node<T>>) -> Arc<Node<T>> {
    let first_healthy = first.healthy();
    let second_healthy = second.healthy();
    if first_healthy != second_healthy {
        return Arc::clone(if first_healthy { first } else { second });
    }

    {
        let state = second.state.lock().expect("P2C node state lock poisoned");
        if state.last_pick.elapsed() > FORCE_PICK_AFTER {
            return Arc::clone(second);
        }
    }

    Arc::clone(if first.load() <= second.load() {
        first
    } else {
        second
    })
}

/// A selected node whose completion updates the balancer.
pub struct P2cRequest<T> {
    node: Option<Arc<Node<T>>>,
    started: Instant,
}

impl<T> P2cRequest<T> {
    pub fn value(&self) -> &T {
        &self
            .node
            .as_ref()
            .expect("P2C request already completed")
            .value
    }

    pub fn complete(mut self, success: bool) {
        if let Some(node) = self.node.take() {
            node.complete(self.started, success);
        }
    }
}

impl<T> Drop for P2cRequest<T> {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            node.complete(self.started, false);
        }
    }
}

#[derive(Debug)]
pub struct NodeSnapshot<'a, T> {
    pub value: &'a T,
    pub inflight: usize,
    pub requests: u64,
    pub latency: Duration,
    pub success_rate: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalancerError {
    Empty,
}

impl fmt::Display for BalancerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a P2C balancer requires at least one node")
    }
}

impl std::error::Error for BalancerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_empty_pool() {
        assert!(matches!(
            P2cBalancer::<&str>::new([]),
            Err(BalancerError::Empty)
        ));
    }

    #[test]
    fn tracks_inflight_requests_and_completions() {
        let balancer = P2cBalancer::new(["one"]).unwrap();
        let request = balancer.pick();
        assert_eq!(request.value(), &"one");
        assert_eq!(balancer.snapshots()[0].inflight, 1);

        request.complete(true);
        let snapshot = balancer.snapshots();
        assert_eq!(snapshot[0].inflight, 0);
        assert_eq!(snapshot[0].requests, 1);
        assert!(snapshot[0].success_rate > 0.5);
    }

    #[test]
    fn dropping_a_request_records_a_failure() {
        let balancer = P2cBalancer::new(["one"]).unwrap();
        drop(balancer.pick());

        let snapshot = balancer.snapshots();
        assert_eq!(snapshot[0].inflight, 0);
        assert!(snapshot[0].success_rate < 1.0);
    }
}
