use actix_web::{
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    web::Bytes,
    HttpResponse,
};
use futures::{Stream, StreamExt};
use rust_zero_core::{CounterVec, Metrics, MetricsError, VectorOptions};
use std::{
    convert::Infallible,
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

/// One event in a `text/event-stream` response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    retry: Option<Duration>,
    comment: Option<String>,
}

/// Limits for detecting and disconnecting EventSource consumers that stop accepting frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SseBackpressurePolicy {
    slow_client_threshold: Duration,
    disconnect_threshold: Duration,
}

impl SseBackpressurePolicy {
    pub fn new(
        slow_client_threshold: Duration,
        disconnect_threshold: Duration,
    ) -> Result<Self, SseBackpressurePolicyError> {
        if slow_client_threshold.is_zero() {
            return Err(SseBackpressurePolicyError::ZeroSlowClientThreshold);
        }
        if disconnect_threshold < slow_client_threshold {
            return Err(SseBackpressurePolicyError::DisconnectBeforeSlowClient);
        }
        Ok(Self {
            slow_client_threshold,
            disconnect_threshold,
        })
    }

    pub fn slow_client_threshold(&self) -> Duration {
        self.slow_client_threshold
    }

    pub fn disconnect_threshold(&self) -> Duration {
        self.disconnect_threshold
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseBackpressurePolicyError {
    ZeroSlowClientThreshold,
    DisconnectBeforeSlowClient,
}

impl fmt::Display for SseBackpressurePolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSlowClientThreshold => {
                formatter.write_str("SSE slow-client threshold must be greater than zero")
            }
            Self::DisconnectBeforeSlowClient => formatter.write_str(
                "SSE disconnect threshold must be greater than or equal to the slow-client threshold",
            ),
        }
    }
}

impl std::error::Error for SseBackpressurePolicyError {}

/// Prometheus counters for SSE backpressure decisions.
#[derive(Clone)]
pub struct SseMetrics {
    decisions: CounterVec,
}

impl SseMetrics {
    pub fn new(metrics: &Metrics, namespace: impl Into<String>) -> Result<Self, MetricsError> {
        let options = VectorOptions::new(
            "sse_backpressure_decisions_total",
            "SSE backpressure decisions by outcome",
        )
        .with_namespace(namespace)
        .with_labels(["decision"]);
        Ok(Self {
            decisions: metrics.counter_vec(options)?,
        })
    }

    fn record(&self, decision: &'static str) {
        self.decisions
            .inc(&[decision])
            .expect("SSE metric labels must match the registered metric");
    }
}

impl SseEvent {
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            data: Some(data.into()),
            ..Self::default()
        }
    }

    /// Creates a comment frame, which is useful as a connection heartbeat.
    pub fn comment(comment: impl Into<String>) -> Self {
        Self {
            comment: Some(comment.into()),
            ..Self::default()
        }
    }

    pub fn with_event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(sanitize_single_line(event.into()));
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(sanitize_single_line(id.into()).replace('\0', ""));
        self
    }

    pub fn with_retry(mut self, retry: Duration) -> Self {
        self.retry = Some(retry);
        self
    }

    /// Encodes the event according to the EventSource wire format.
    pub fn encode(&self) -> Bytes {
        let mut encoded = String::new();
        if let Some(comment) = &self.comment {
            for line in lines(comment) {
                encoded.push_str(": ");
                encoded.push_str(line);
                encoded.push('\n');
            }
        }
        if let Some(event) = &self.event {
            encoded.push_str("event: ");
            encoded.push_str(event);
            encoded.push('\n');
        }
        if let Some(id) = &self.id {
            encoded.push_str("id: ");
            encoded.push_str(id);
            encoded.push('\n');
        }
        if let Some(retry) = self.retry {
            encoded.push_str("retry: ");
            encoded.push_str(&retry.as_millis().to_string());
            encoded.push('\n');
        }
        if let Some(data) = &self.data {
            for line in lines(data) {
                encoded.push_str("data: ");
                encoded.push_str(line);
                encoded.push('\n');
            }
        }
        encoded.push('\n');
        Bytes::from(encoded)
    }
}

/// Builds a streaming EventSource response without proxy or browser caching.
pub fn sse_response<S>(events: S) -> HttpResponse
where
    S: Stream<Item = SseEvent> + 'static,
{
    HttpResponse::Ok()
        .insert_header((CONTENT_TYPE, "text/event-stream"))
        .insert_header((CACHE_CONTROL, "no-cache, no-transform"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(events.map(|event| Ok::<_, Infallible>(event.encode())))
}

/// Builds an EventSource response that observes downstream demand and disconnects stalled clients.
///
/// A client is considered slow when Actix does not request another frame within the configured
/// threshold after receiving the previous one. Source-side idle time is not counted. Pass metrics
/// to expose `sse_backpressure_decisions_total{decision="slow|disconnected"}`.
pub fn sse_response_with_backpressure<S>(
    events: S,
    policy: SseBackpressurePolicy,
    metrics: Option<SseMetrics>,
) -> HttpResponse
where
    S: Stream<Item = SseEvent> + 'static,
{
    HttpResponse::Ok()
        .insert_header((CONTENT_TYPE, "text/event-stream"))
        .insert_header((CACHE_CONTROL, "no-cache, no-transform"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(SseBackpressureStream {
            events: Box::pin(events),
            policy,
            metrics,
            yielded_at: None,
            done: false,
        })
}

struct SseBackpressureStream<S> {
    events: Pin<Box<S>>,
    policy: SseBackpressurePolicy,
    metrics: Option<SseMetrics>,
    yielded_at: Option<Instant>,
    done: bool,
}

impl<S> Stream for SseBackpressureStream<S>
where
    S: Stream<Item = SseEvent>,
{
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }

        if let Some(yielded_at) = self.yielded_at.take() {
            let elapsed = yielded_at.elapsed();
            if elapsed >= self.policy.disconnect_threshold {
                if let Some(metrics) = &self.metrics {
                    metrics.record("disconnected");
                }
                self.done = true;
                return Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSE client stopped accepting frames",
                ))));
            }
            if elapsed >= self.policy.slow_client_threshold {
                if let Some(metrics) = &self.metrics {
                    metrics.record("slow");
                }
            }
        }

        match self.events.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                self.yielded_at = Some(Instant::now());
                Poll::Ready(Some(Ok(event.encode())))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn sanitize_single_line(value: String) -> String {
    value.replace(['\r', '\n'], "")
}

fn lines(value: &str) -> impl Iterator<Item = &str> {
    value
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{body::to_bytes, http::StatusCode};
    use futures::stream;
    use std::thread;

    #[test]
    fn encodes_named_multiline_events_and_heartbeats() {
        let event = SseEvent::data("first\nsecond")
            .with_event("update\nignored")
            .with_id("42\r\n")
            .with_retry(Duration::from_millis(1500));
        assert_eq!(
            event.encode(),
            Bytes::from_static(
                b"event: updateignored\nid: 42\nretry: 1500\ndata: first\ndata: second\n\n"
            )
        );
        assert_eq!(
            SseEvent::comment("keepalive").encode(),
            Bytes::from_static(b": keepalive\n\n")
        );
    }

    #[actix_rt::test]
    async fn response_sets_event_stream_headers_and_streams_frames() {
        let response = sse_response(stream::iter([SseEvent::data("one"), SseEvent::data("two")]));

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "no-cache, no-transform"
        );
        assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
        assert_eq!(
            to_bytes(response.into_body()).await.unwrap(),
            Bytes::from_static(b"data: one\n\ndata: two\n\n")
        );
    }

    #[test]
    fn validates_backpressure_thresholds() {
        assert_eq!(
            SseBackpressurePolicy::new(Duration::ZERO, Duration::from_secs(1)),
            Err(SseBackpressurePolicyError::ZeroSlowClientThreshold)
        );
        assert_eq!(
            SseBackpressurePolicy::new(Duration::from_secs(2), Duration::from_secs(1)),
            Err(SseBackpressurePolicyError::DisconnectBeforeSlowClient)
        );
    }

    #[actix_rt::test]
    async fn records_slow_consumers_without_counting_source_idle_time() {
        let registry = Metrics::new();
        let metrics = SseMetrics::new(&registry, "service").unwrap();
        let policy =
            SseBackpressurePolicy::new(Duration::from_millis(5), Duration::from_millis(100))
                .unwrap();
        let mut events = SseBackpressureStream {
            events: Box::pin(stream::iter([SseEvent::data("one"), SseEvent::data("two")])),
            policy,
            metrics: Some(metrics),
            yielded_at: None,
            done: false,
        };

        assert!(events.next().await.unwrap().is_ok());
        thread::sleep(Duration::from_millis(10));
        assert!(events.next().await.unwrap().is_ok());
        assert!(registry
            .render()
            .contains("service_sse_backpressure_decisions_total{decision=\"slow\"} 1"));
    }

    #[actix_rt::test]
    async fn disconnects_consumers_that_exceed_the_hard_limit() {
        let registry = Metrics::new();
        let metrics = SseMetrics::new(&registry, "service").unwrap();
        let policy =
            SseBackpressurePolicy::new(Duration::from_millis(2), Duration::from_millis(5)).unwrap();
        let mut events = SseBackpressureStream {
            events: Box::pin(stream::iter([SseEvent::data("one"), SseEvent::data("two")])),
            policy,
            metrics: Some(metrics),
            yielded_at: None,
            done: false,
        };

        assert!(events.next().await.unwrap().is_ok());
        thread::sleep(Duration::from_millis(10));
        let error = events.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(events.next().await.is_none());
        assert!(registry
            .render()
            .contains("service_sse_backpressure_decisions_total{decision=\"disconnected\"} 1"));
    }
}
