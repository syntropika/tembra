//! Console observation is independent of callback authority and attributed by the host.
use crate::{LogRecord, engine::LogSink, failure};
use serde_json::Value;

#[derive(Clone)]
pub(super) struct Diagnostics {
    pub orb: String,
    pub sink: Option<LogSink>,
}
impl Diagnostics {
    /// Consume only diagnostic packets; ordinary operations still require an issued scope.
    pub fn accept(&self, packet: &str) -> Option<tembra::Result<Value>> {
        let packet: Value = serde_json::from_str(packet).ok()?;
        if packet["operation"] != "console" {
            return None;
        }
        Some(self.emit(&packet))
    }

    fn emit(&self, packet: &Value) -> tembra::Result<Value> {
        let level = packet["level"]
            .as_str()
            .filter(|level| ["log", "debug", "info", "warn", "error"].contains(level))
            .ok_or_else(|| failure("invalid console level"))?;
        let message = packet["message"]
            .as_str()
            .ok_or_else(|| failure("invalid console message"))?;
        if let Some(sink) = &self.sink {
            let record = LogRecord {
                orb: self.orb.clone(),
                level: level.to_owned(),
                message: message.to_owned(),
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(&record)));
        }
        Ok(Value::Null)
    }
}
