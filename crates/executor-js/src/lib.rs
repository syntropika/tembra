//! Optional V8 adapter. Pass the SDK's prebuilt `embedded.js`; the kernel contains no JS engine.
use deno_core::{JsBuffer, JsRuntime, OpState, RuntimeOptions, extension, op2, v8};
use futures::{
    StreamExt,
    channel::mpsc,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
    task::{LocalFutureObj, LocalSpawn, SpawnError},
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra::core::Reason;
use tembra::{Error, Host, Result};
use tembra_bridge::protocol::{EffectType, NativeOp};

mod bindings;
mod bridge;
mod runtime;

pub use runtime::Runtime;

#[derive(Clone)]
pub struct TokioExecutor(mpsc::UnboundedSender<LocalFutureObj<'static, ()>>);

impl TokioExecutor {
    /// Capture a running Tokio LocalSet. Requests remain local even when V8 polls an op elsewhere.
    pub fn new() -> Self {
        let (send, mut receive) = mpsc::unbounded::<LocalFutureObj<'static, ()>>();
        tokio::task::spawn_local(async move {
            let mut running = FuturesUnordered::new();
            loop {
                futures::select! {
                    future=receive.next()=>match future {
                        Some(future)=>running.push(future),
                        None=>{while running.next().await.is_some(){}break;},
                    },
                    ()=running.select_next_some()=>{},
                }
            }
        });
        Self(send)
    }
}
impl Default for TokioExecutor {
    fn default() -> Self {
        Self::new()
    }
}
impl LocalSpawn for TokioExecutor {
    /// Spawn physical callback work on the current Tokio local executor.
    fn spawn_local_obj(
        &self,
        future: LocalFutureObj<'static, ()>,
    ) -> std::result::Result<(), SpawnError> {
        self.0
            .unbounded_send(future)
            .map_err(|_| SpawnError::shutdown())
    }
}
