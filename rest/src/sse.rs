use actix_web::{
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    web::Bytes,
    HttpResponse,
};
use futures::{Stream, StreamExt};
use std::{convert::Infallible, time::Duration};

/// One event in a `text/event-stream` response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    retry: Option<Duration>,
    comment: Option<String>,
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
}
