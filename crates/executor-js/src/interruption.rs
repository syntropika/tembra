use deno_core::v8;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Host-owned control for interrupting the entire V8 realm from another thread.
/// Interruption does not close Tembra resources or undo completed native effects.
#[derive(Clone)]
pub struct InterruptHandle {
    pub(crate) isolate: v8::IsolateHandle,
    pub(crate) requested: Arc<AtomicBool>,
}

impl InterruptHandle {
    /// Request termination of this V8 realm, including synchronous loops.
    /// An idle request closes admission for subsequent execution. Synchronize watchdog requests
    /// with the guarded work. Only terminal cleanup can enter an interrupted realm.
    /// Returns false after the isolate is destroyed. True confirms a request only.
    /// The owner must observe the execution result and reconcile uncertain effects.
    pub fn terminate(&self) -> bool {
        self.requested.store(true, Ordering::Release);
        self.isolate.terminate_execution()
    }
}
