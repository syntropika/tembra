use super::bounded;
use serde::Serialize;

/// Why an explicit typed payload capture was refused. Runtime execution is unaffected.
#[derive(Debug, PartialEq, Eq)]
pub enum PayloadCaptureError {
    /// The requested limit is zero or exceeds 65,536 bytes.
    InvalidLimit,
    /// Encoded JSON exceeded the requested byte limit; partial output is discarded.
    LimitExceeded,
    /// The explicitly selected serializer refused the value.
    Serialization(String),
}
impl std::fmt::Display for PayloadCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PayloadCaptureError {}

/// Explicitly encodes a known serializable type with a hard output byte limit.
///
/// Call this in an application adapter or after draining, then associate its result with an
/// observed ID. It is never called by the core. Native `Any` values have no implicit encoding.
/// The selected `Serialize` implementation runs synchronously and can itself allocate, block,
/// panic, or have effects; only bytes written by this function are bounded. Failures discard
/// partial output and do not change any runtime operation or observation queue.
///
/// # Errors
/// Returns invalid limits, output overflow, or a bounded serialization diagnostic.
///
/// ```
/// use tembra_core::observability::{capture_payload, PayloadCaptureError};
/// assert_eq!(capture_payload(&vec![1, 2], 16).unwrap(), "[1,2]");
/// assert_eq!(capture_payload(&vec![1, 2], 3), Err(PayloadCaptureError::LimitExceeded));
/// ```
pub fn capture_payload<T: Serialize>(
    value: &T,
    max_bytes: usize,
) -> std::result::Result<String, PayloadCaptureError> {
    if max_bytes == 0 || max_bytes > 65_536 {
        return Err(PayloadCaptureError::InvalidLimit);
    }
    struct Output {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("payload limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        bytes: Vec::with_capacity(max_bytes),
        limit: max_bytes,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut output, value) {
        return Err(if output.exceeded {
            PayloadCaptureError::LimitExceeded
        } else {
            PayloadCaptureError::Serialization(bounded(&error.to_string()))
        });
    }
    String::from_utf8(output.bytes)
        .map_err(|error| PayloadCaptureError::Serialization(bounded(&error.to_string())))
}
