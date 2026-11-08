//! Console observation is independent of callback authority and attributed by the host.
use crate::{
    LogRecord,
    engine::LogSink,
    protocol::{LogLevel, Request},
};
use serde_json::Value;

#[derive(Clone)]
pub(super) struct Diagnostics {
    pub orb: String,
    pub sink: Option<LogSink>,
}
impl Diagnostics {
    /// Consume only well-formed diagnostic packets; every other packet, including a malformed
    /// console record, falls through to scope routing, which rejects it explicitly.
    pub fn accept(&self, packet: &str) -> Option<tembra::Result<Value>> {
        let Ok(Request::Console { level, message }) = Request::decode(packet) else {
            return None;
        };
        Some(self.emit(level, message))
    }

    /// Deliver one record without allowing observer panics to interrupt execution.
    fn emit(&self, level: LogLevel, message: String) -> tembra::Result<Value> {
        if let Some(sink) = &self.sink {
            let record = LogRecord {
                orb: self.orb.clone(),
                level: level.as_str().to_owned(),
                message,
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(&record)));
        }
        Ok(Value::Null)
    }
}
