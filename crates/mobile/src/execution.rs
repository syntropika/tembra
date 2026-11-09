//! Mobile transports reuse the native executor without moving its thread-affine owners.
use crate::MobileError;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, mpsc},
    thread,
};
use tembra_executor_wasm::{Binding, Engine, Grants, Limits};

mod logs;
mod port;
pub use port::NativePort;

/// Preserve execution diagnostics across Swift and Kotlin without exporting host capabilities.
fn error(value: impl ToString) -> MobileError {
    MobileError::Execution {
        diagnostic: value.to_string(),
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Configuration {
    limits: Limits,
    filesystem: BTreeMap<String, String>,
    network: Vec<String>,
}

#[derive(uniffi::Object)]
pub struct NativeArtifact {
    artifact: tembra_contracts::Artifact,
}

#[uniffi::export]
impl NativeArtifact {
    /// Decode closed Orb bytes without evaluating guest code.
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>) -> Result<Arc<Self>, MobileError> {
        Ok(Arc::new(Self {
            artifact: tembra_contracts::Artifact::decode(&bytes).map_err(error)?,
        }))
    }

    /// Return the identity carried by the prepared artifact.
    pub fn description(&self) -> String {
        self.artifact.describe().to_string()
    }

    /// Bind a checked method signature before guest invocation.
    pub fn bind(&self, binding: String, method: String, exports: bool) -> Result<u32, MobileError> {
        self.artifact
            .contract
            .bind(&binding, &method, exports)
            .map_err(error)
    }

    /// Validate the original captured bytes against their prepared slot.
    pub fn validate(&self, slot: u32, arguments: bool, value: String) -> String {
        self.artifact
            .contract
            .validate_packet(slot, arguments, &value)
    }

    /// Answer installation queries through the shared prepared contract.
    pub fn request(&self, packet: String) -> String {
        self.artifact.contract.request(&packet)
    }
}

#[derive(uniffi::Object)]
pub struct NativeInterpreter {
    engine: Engine,
}

#[uniffi::export]
impl NativeInterpreter {
    /// Configure portable execution and host providers without granting any Orb access.
    #[uniffi::constructor]
    pub fn new(configuration: String, interpreter: Vec<u8>) -> Result<Arc<Self>, MobileError> {
        let config: Configuration = serde_json::from_str(&configuration).map_err(error)?;
        let mut engine = Engine::interpreted(config.limits).map_err(error)?;
        for (name, path) in config.filesystem {
            engine = engine.filesystem(name, path).map_err(error)?;
        }
        for name in config.network {
            engine = engine.network(name).map_err(error)?;
        }
        if !interpreter.is_empty() {
            engine = engine.interpreter(&interpreter).map_err(error)?;
        }
        Ok(Arc::new(Self { engine }))
    }

    /// Create an isolated source or compiled revision after validating its imports and grants.
    ///
    /// Compiled dependency calls arrive as ordinary scoped host events on the same port.
    pub fn open(
        &self,
        artifact: Arc<NativeArtifact>,
        bindings: String,
        grants: String,
    ) -> Result<Arc<NativePort>, MobileError> {
        let bindings: BTreeMap<String, Binding> = serde_json::from_str(&bindings).map_err(error)?;
        let grants: Grants = serde_json::from_str(&grants).map_err(error)?;
        NativePort::new(self.engine.clone(), artifact, bindings, grants)
    }

    /// Count physical guest Stores, including work still draining after cancellation.
    pub fn instances(&self) -> u64 {
        self.engine.instances() as u64
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Reply<T> {
    Ok(T),
    Error(Failure),
}
#[derive(Serialize)]
struct Failure {
    code: &'static str,
    reason: String,
    message: String,
}
impl<T> From<tembra::Result<T>> for Reply<T> {
    /// Preserve structured executor failures at each foreign transport boundary.
    fn from(value: tembra::Result<T>) -> Self {
        match value {
            Ok(value) => Self::Ok(value),
            Err(value) => Self::Error(Failure {
                code: value.code.as_str(),
                reason: value.reason,
                message: value.message,
            }),
        }
    }
}

/// Wake foreign continuations on an OS stack, never on Wasmtime's temporary fiber stack.
fn delivery() -> Result<&'static tokio::runtime::Runtime, MobileError> {
    static DELIVERY: std::sync::LazyLock<std::io::Result<tokio::runtime::Runtime>> =
        std::sync::LazyLock::new(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("tembra-mobile-events")
                .build()
        });
    DELIVERY.as_ref().map_err(error)
}

/// Run thread-affine state behind a typed job queue; no foreign callback executes on this thread.
type Job<T> = Box<dyn FnOnce(&mut T) + Send>;
struct Owner<T> {
    sender: Option<mpsc::Sender<Job<T>>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl<T: 'static> Owner<T> {
    /// Construct local state on its final owner and report initialization failures synchronously.
    fn new(
        initialize: impl FnOnce() -> Result<T, MobileError> + Send + 'static,
    ) -> Result<Self, MobileError> {
        let (sender, jobs) = mpsc::channel::<Job<T>>();
        let (ready, result) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("tembra-mobile-port".into())
            .spawn(move || match initialize() {
                Ok(mut state) => {
                    let _ = ready.send(Ok(()));
                    for job in jobs {
                        job(&mut state);
                    }
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            })
            .map_err(error)?;
        if let Err(e) = result.recv().map_err(error)? {
            let _ = thread.join();
            return Err(e);
        }
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    /// Exchange data with the owner without moving its local interpreter or ticket.
    fn call<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut T) -> Result<R, MobileError> + Send + 'static,
    ) -> Result<R, MobileError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or(MobileError::Closed)?
            .send(Box::new(move |state| {
                let _ = send.send(job(state));
            }))
            .map_err(error)?;
        receive.recv().map_err(error)?
    }
}
impl<T> Drop for Owner<T> {
    /// Destroy thread-affine state on its owner after all earlier commands.
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
