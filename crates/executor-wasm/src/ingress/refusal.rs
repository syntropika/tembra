//! A handler's `wasi:http` `error-code` in the standard binary codec that platform carriers
//! decode (`IngressEvent::Refused`).
//!
//! Layout: a variant is its little-endian `u32` case index then its payload; an option is a `u8`
//! tag then its value; a string is a little-endian `u32` length then UTF-8 bytes; integers are
//! little-endian. Case indices follow the bundled `wasi:http@0.3.0` WIT, which a unit test pins.
use wasmtime_wasi_http::p3::bindings::http::types::{ErrorCode, FieldSizePayload};

/// Append-only little-endian writer.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn tag(&mut self, index: u32) {
        self.0.extend_from_slice(&index.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.tag(u32::try_from(value.len()).unwrap_or(u32::MAX));
        self.0.extend_from_slice(value.as_bytes());
    }

    fn option<T>(&mut self, value: Option<T>, write: impl FnOnce(&mut Self, T)) {
        match value {
            None => self.0.push(0),
            Some(value) => {
                self.0.push(1);
                write(self, value);
            }
        }
    }

    fn optional_string(&mut self, value: Option<String>) {
        self.option(value, |writer, value| writer.string(&value));
    }

    fn optional_u32(&mut self, value: Option<u32>) {
        self.option(value, |writer, value| {
            writer.0.extend_from_slice(&value.to_le_bytes())
        });
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        self.option(value, |writer, value| {
            writer.0.extend_from_slice(&value.to_le_bytes())
        });
    }

    fn field_size(&mut self, payload: FieldSizePayload) {
        self.optional_string(payload.field_name);
        self.optional_u32(payload.field_size);
    }
}

/// Encode one `error-code` value.
pub(crate) fn encode(code: ErrorCode) -> Vec<u8> {
    let mut w = Writer::default();
    match code {
        ErrorCode::DnsTimeout => w.tag(0),
        ErrorCode::DnsError(payload) => {
            w.tag(1);
            w.optional_string(payload.rcode);
            w.option(payload.info_code, |writer, value| {
                writer.0.extend_from_slice(&value.to_le_bytes())
            });
        }
        ErrorCode::DestinationNotFound => w.tag(2),
        ErrorCode::DestinationUnavailable => w.tag(3),
        ErrorCode::DestinationIpProhibited => w.tag(4),
        ErrorCode::DestinationIpUnroutable => w.tag(5),
        ErrorCode::ConnectionRefused => w.tag(6),
        ErrorCode::ConnectionTerminated => w.tag(7),
        ErrorCode::ConnectionTimeout => w.tag(8),
        ErrorCode::ConnectionReadTimeout => w.tag(9),
        ErrorCode::ConnectionWriteTimeout => w.tag(10),
        ErrorCode::ConnectionLimitReached => w.tag(11),
        ErrorCode::TlsProtocolError => w.tag(12),
        ErrorCode::TlsCertificateError => w.tag(13),
        ErrorCode::TlsAlertReceived(payload) => {
            w.tag(14);
            w.option(payload.alert_id, |writer, value| writer.0.push(value));
            w.optional_string(payload.alert_message);
        }
        ErrorCode::HttpRequestDenied => w.tag(15),
        ErrorCode::HttpRequestLengthRequired => w.tag(16),
        ErrorCode::HttpRequestBodySize(size) => {
            w.tag(17);
            w.optional_u64(size);
        }
        ErrorCode::HttpRequestMethodInvalid => w.tag(18),
        ErrorCode::HttpRequestUriInvalid => w.tag(19),
        ErrorCode::HttpRequestUriTooLong => w.tag(20),
        ErrorCode::HttpRequestHeaderSectionSize(size) => {
            w.tag(21);
            w.optional_u32(size);
        }
        ErrorCode::HttpRequestHeaderSize(payload) => {
            w.tag(22);
            w.option(payload, Writer::field_size);
        }
        ErrorCode::HttpRequestTrailerSectionSize(size) => {
            w.tag(23);
            w.optional_u32(size);
        }
        ErrorCode::HttpRequestTrailerSize(payload) => {
            w.tag(24);
            w.field_size(payload);
        }
        ErrorCode::HttpResponseIncomplete => w.tag(25),
        ErrorCode::HttpResponseHeaderSectionSize(size) => {
            w.tag(26);
            w.optional_u32(size);
        }
        ErrorCode::HttpResponseHeaderSize(payload) => {
            w.tag(27);
            w.field_size(payload);
        }
        ErrorCode::HttpResponseBodySize(size) => {
            w.tag(28);
            w.optional_u64(size);
        }
        ErrorCode::HttpResponseTrailerSectionSize(size) => {
            w.tag(29);
            w.optional_u32(size);
        }
        ErrorCode::HttpResponseTrailerSize(payload) => {
            w.tag(30);
            w.field_size(payload);
        }
        ErrorCode::HttpResponseTransferCoding(coding) => {
            w.tag(31);
            w.optional_string(coding);
        }
        ErrorCode::HttpResponseContentCoding(coding) => {
            w.tag(32);
            w.optional_string(coding);
        }
        ErrorCode::HttpResponseTimeout => w.tag(33),
        ErrorCode::HttpUpgradeFailed => w.tag(34),
        ErrorCode::HttpProtocolError => w.tag(35),
        ErrorCode::LoopDetected => w.tag(36),
        ErrorCode::ConfigurationError => w.tag(37),
        ErrorCode::InternalError(message) => {
            w.tag(38);
            w.optional_string(message);
        }
    }
    w.0
}

impl crate::ingress::IngressEvent {
    /// A handler's `error-code` as the typed `Refused` event of platform carriers.
    pub(crate) fn refused(code: ErrorCode) -> Self {
        Self::Refused(encode(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder's case indices are the bundled WIT's case order.
    #[test]
    fn case_indices_follow_the_bundled_wit() {
        let source = include_str!("../../../contracts/wit/wasi03/http.wit");
        let body = source
            .split("variant error-code {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap();
        let cases: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(|line| line.split(['(', ',']).next().unwrap())
            .collect();
        assert_eq!(cases.len(), 39);
        let index = |name: &str| cases.iter().position(|case| *case == name).unwrap() as u32;
        let tag = |code: ErrorCode| u32::from_le_bytes(encode(code)[..4].try_into().unwrap());
        assert_eq!(tag(ErrorCode::DnsTimeout), index("DNS-timeout"));
        assert_eq!(
            tag(ErrorCode::HttpRequestDenied),
            index("HTTP-request-denied")
        );
        assert_eq!(
            tag(ErrorCode::HttpResponseIncomplete),
            index("HTTP-response-incomplete")
        );
        assert_eq!(tag(ErrorCode::LoopDetected), index("loop-detected"));
        assert_eq!(tag(ErrorCode::InternalError(None)), index("internal-error"));
        assert_eq!(
            encode(ErrorCode::InternalError(Some("x".into()))),
            [38, 0, 0, 0, 1, 1, 0, 0, 0, b'x']
        );
        assert_eq!(
            encode(ErrorCode::HttpRequestTrailerSize(FieldSizePayload {
                field_name: None,
                field_size: Some(7),
            })),
            [24, 0, 0, 0, 0, 1, 7, 0, 0, 0]
        );
    }
}
