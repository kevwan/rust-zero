use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// W3C trace flags carried by a [`TraceContext`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceFlags(u8);

impl TraceFlags {
    pub const NONE: Self = Self(0);
    pub const SAMPLED: Self = Self(1);

    pub fn is_sampled(self) -> bool {
        self.0 & 1 == 1
    }
}

/// A parsed W3C `traceparent` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: Option<[u8; 8]>,
    flags: TraceFlags,
}

impl TraceContext {
    pub fn root(flags: TraceFlags) -> Self {
        Self {
            trace_id: next_trace_id(),
            span_id: next_span_id(),
            parent_span_id: None,
            flags,
        }
    }

    pub fn parse(value: &str) -> Result<Self, TraceContextError> {
        let mut parts = value.split('-');
        let version = parts.next().ok_or(TraceContextError)?;
        let trace = parts.next().ok_or(TraceContextError)?;
        let span = parts.next().ok_or(TraceContextError)?;
        let flags = parts.next().ok_or(TraceContextError)?;
        if parts.next().is_some()
            || version != "00"
            || trace.len() != 32
            || span.len() != 16
            || flags.len() != 2
        {
            return Err(TraceContextError);
        }

        let trace_id = decode_hex::<16>(trace)?;
        let span_id = decode_hex::<8>(span)?;
        let flags = TraceFlags(u8::from_str_radix(flags, 16).map_err(|_| TraceContextError)?);
        if trace_id == [0; 16] || span_id == [0; 8] {
            return Err(TraceContextError);
        }

        Ok(Self {
            trace_id,
            span_id,
            parent_span_id: None,
            flags,
        })
    }

    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: next_span_id(),
            parent_span_id: Some(self.span_id),
            flags: self.flags,
        }
    }

    pub fn trace_id(&self) -> String {
        encode_hex(&self.trace_id)
    }

    pub fn span_id(&self) -> String {
        encode_hex(&self.span_id)
    }

    pub fn parent_span_id(&self) -> Option<String> {
        self.parent_span_id.as_ref().map(|id| encode_hex(id))
    }

    pub fn flags(&self) -> TraceFlags {
        self.flags
    }

    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            encode_hex(&self.trace_id),
            encode_hex(&self.span_id),
            self.flags.0
        )
    }
}

/// Returned when a W3C trace context is malformed or uses an unsupported version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceContextError;

impl fmt::Display for TraceContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid W3C traceparent value")
    }
}

impl std::error::Error for TraceContextError {}

fn next_trace_id() -> [u8; 16] {
    let mut id = [0; 16];
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    id.copy_from_slice(&time.to_be_bytes());
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed).to_be_bytes();
    for (target, source) in id[8..].iter_mut().zip(sequence) {
        *target ^= source;
    }
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}

fn next_span_id() -> [u8; 8] {
    let mut id = NEXT_ID.fetch_add(1, Ordering::Relaxed).to_be_bytes();
    if id == [0; 8] {
        id[7] = 1;
    }
    id
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N], TraceContextError> {
    let mut output = [0; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| TraceContextError)?;
    }
    Ok(output)
}

fn encode_hex(bytes: &[u8]) -> String {
    use fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_creates_child_trace_contexts() {
        let parent =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let child = parent.child();

        assert_eq!(child.trace_id(), parent.trace_id());
        assert_eq!(child.parent_span_id(), Some(parent.span_id()));
        assert!(child.flags().is_sampled());
        assert!(TraceContext::parse(&child.traceparent()).is_ok());
    }

    #[test]
    fn rejects_zero_and_malformed_identifiers() {
        assert!(
            TraceContext::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_err()
        );
        assert!(TraceContext::parse("not-a-trace").is_err());
    }
}
