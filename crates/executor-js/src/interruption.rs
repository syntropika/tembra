use deno_core::v8;

/// Host-owned control for interrupting the entire V8 realm from another thread.
/// Interruption does not close Tembra resources or undo completed native effects.
#[derive(Clone)]
pub struct InterruptHandle(pub(crate) v8::IsolateHandle);

impl InterruptHandle {
    /// Request termination of current JavaScript execution, including synchronous loops.
    /// Returns false after the isolate is destroyed. True confirms a request only.
    /// The owner must observe the execution result and reconcile uncertain effects.
    pub fn terminate(&self) -> bool {
        self.0.terminate_execution()
    }
}
