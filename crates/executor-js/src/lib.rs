//! Optional V8 adapter. Pass the SDK's prebuilt `embedded.js`; the kernel contains no JS engine.
use deno_core::{JsBuffer, JsRuntime, OpState, RuntimeOptions, extension, op2, v8};
use futures::{
    future::{AbortHandle, Abortable},
    task::{LocalFutureObj, LocalSpawn, SpawnError},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::core::Reason;
use tembra::{Error, Host, Result};
use tembra_bridge::protocol::{EffectType, NativeOp};

mod bindings;
mod runtime;

pub use runtime::Runtime;

#[derive(Clone)]
pub struct TokioExecutor;
impl LocalSpawn for TokioExecutor {
    /// Spawn physical callback work on the current Tokio local executor.
    fn spawn_local_obj(
        &self,
        future: LocalFutureObj<'static, ()>,
    ) -> std::result::Result<(), SpawnError> {
        tokio::task::spawn_local(future);
        Ok(())
    }
}
