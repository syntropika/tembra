//! Console observation is independent of callback authority and attributed by the host.
use crate::{LogRecord, engine::LogSink, protocol::LogLevel};

#[derive(Clone)]
pub(crate) struct Diagnostics {
    pub orb: String,
    pub sink: Option<LogSink>,
}

impl Diagnostics {
    /// Deliver one record without allowing observer panics to interrupt execution.
    pub fn emit(&self, level: LogLevel, message: String) {
        if let Some(sink) = &self.sink {
            let record = LogRecord {
                orb: self.orb.clone(),
                level: level.as_str().to_owned(),
                message,
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(&record)));
        }
    }
}
