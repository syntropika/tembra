//! Diagnostic records of native Orbs, for the host's log observer.

use super::*;

/// A diagnostic record, attributed by the host to the Orb that wrote it, never by a name the Orb
/// gives.
#[derive(Clone, Debug, Serialize)]
pub struct LogRecord {
    pub orb: String,
    pub level: String,
    pub message: String,
}

impl Host {
    /// Observe the records native Orbs write with [`Context::log`]; a later observer replaces this
    /// one. Without an observer, records are dropped. The host releases its observer when it
    /// closes.
    pub fn on_log(&self, observer: impl Fn(&LogRecord) + 'static) {
        *self.0.log_observer.borrow_mut() = Some(Rc::new(observer));
    }
}

impl Context {
    /// Hand a record to the host's log observer, attributed to the Orb running this invocation;
    /// without an observer it is dropped. Writing needs no admission: a cancelled call still logs.
    /// An observer that panics does not interrupt the call.
    pub fn log(&self, level: impl Into<String>, message: impl Into<String>) -> Result<()> {
        let call = self.invocation()?;
        let observer = self.host.0.log_observer.borrow().clone();
        let Some(observer) = observer else {
            return Ok(());
        };
        let record = LogRecord {
            orb: self.host.kernel().orb(call.orb)?.declaration.name,
            level: level.into(),
            message: message.into(),
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(&record)));
        Ok(())
    }
}
