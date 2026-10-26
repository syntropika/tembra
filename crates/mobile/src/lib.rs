//! Foreign bindings keep the thread-affine Rust kernel on one private worker.
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

uniffi::setup_scaffolding!();

#[derive(Debug, uniffi::Error)]
pub enum MobileError {
    Closed,
    WorkerUnavailable,
}

impl std::fmt::Display for MobileError {
    /// Describe transport failures separately from kernel protocol errors.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Closed => "The native kernel has been released",
            Self::WorkerUnavailable => "The native kernel worker is unavailable",
        })
    }
}

impl std::error::Error for MobileError {}

struct Request {
    metadata: String,
    reply: mpsc::SyncSender<String>,
}

struct Worker {
    sender: mpsc::SyncSender<Request>,
    thread: JoinHandle<()>,
}

impl Worker {
    /// Disconnect and join so the kernel is also destroyed on its owning thread.
    fn stop(self) {
        drop(self.sender);
        let _ = self.thread.join();
    }
}

#[derive(uniffi::Object)]
pub struct NativeKernel {
    worker: Mutex<Option<Worker>>,
}

#[uniffi::export]
impl NativeKernel {
    /// Create an isolated kernel without moving its thread-affine state across FFI threads.
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, MobileError> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(1);
        let thread = thread::Builder::new()
            .name("tembra-kernel".into())
            .spawn(move || {
                let kernel = tembra_bridge::Kernel::new();
                while let Ok(request) = receiver.recv() {
                    let _ = request.reply.send(kernel.request(&request.metadata));
                }
            })
            .map_err(|_| MobileError::WorkerUnavailable)?;
        Ok(Arc::new(Self {
            worker: Mutex::new(Some(Worker { sender, thread })),
        }))
    }

    /// Exchange metadata synchronously while serializing requests with release.
    pub fn request(&self, metadata: String) -> Result<String, MobileError> {
        let guard = self
            .worker
            .lock()
            .map_err(|_| MobileError::WorkerUnavailable)?;
        let worker = guard.as_ref().ok_or(MobileError::Closed)?;
        let (reply, receiver) = mpsc::sync_channel(1);
        worker
            .sender
            .send(Request { metadata, reply })
            .map_err(|_| MobileError::WorkerUnavailable)?;
        receiver.recv().map_err(|_| MobileError::WorkerUnavailable)
    }

    /// Release native storage after the language adapter has completed host shutdown.
    pub fn shutdown(&self) {
        let mut guard = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(worker) = guard.take() {
            worker.stop();
        }
    }
}

impl Drop for NativeKernel {
    /// Release the worker even when foreign code omits explicit cleanup.
    fn drop(&mut self) {
        self.shutdown();
    }
}
