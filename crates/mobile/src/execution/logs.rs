use std::sync::{Arc, Mutex};
use tembra_executor_wasm::{Engine, LogRecord};

/** Retain a bounded diagnostic batch until the platform next services this revision. */
#[derive(Clone, Default)]
pub(super) struct Logs(Arc<Mutex<Vec<LogRecord>>>);
impl Logs {
    /// Attribute records in the native executor and keep logging outside execution authority.
    pub(super) fn attach(&self, engine: Engine) -> Engine {
        let sink = self.clone();
        engine.on_log(move |record| {
            let mut logs = sink.0.lock().unwrap_or_else(|e| e.into_inner());
            if logs.len() < 128 {
                let mut record = record.clone();
                record.message = record.message.chars().take(4096).collect();
                logs.push(record);
            }
        })
    }
    /// Transfer each record once and release its retained strings.
    pub(super) fn take(&self) -> String {
        serde_json::to_string(&std::mem::take(
            &mut *self.0.lock().unwrap_or_else(|e| e.into_inner()),
        ))
        .expect("serializable diagnostic records")
    }
}
