//! Typed private WIT progress keeps waits outside the interpreter's native stack.
use super::runtime::{State, request};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use wasmtime::{
    Store,
    component::{Instance, TypedFunc},
};

mod wire {
    wasmtime::component::bindgen!({path: "wit", world: "interpreter"});
}
use wire::tembra::runtime::execution::{Completion, Outcome, Progress};

pub(super) struct Driver {
    load: TypedFunc<(String,), (Outcome,)>,
    declare: TypedFunc<(String,), (Outcome,)>,
    start: TypedFunc<(String,), (Progress,)>,
    resume: TypedFunc<(Vec<Completion>,), (Progress,)>,
}
impl Driver {
    /// Resolve checked typed exports once for this isolated installation.
    pub fn new(instance: &Instance, store: &mut Store<State>) -> wasmtime::Result<Self> {
        Ok(Self {
            load: instance.get_typed_func(&mut *store, "load")?,
            declare: instance.get_typed_func(&mut *store, "declare")?,
            start: instance.get_typed_func(&mut *store, "start")?,
            resume: instance.get_typed_func(&mut *store, "resume")?,
        })
    }
    /// Synchronous Wasm steps may suspend in standard WASI I/O, never while waiting on an Orb.
    pub async fn call(
        &self,
        export: &str,
        packet: String,
        store: &mut Store<State>,
    ) -> wasmtime::Result<Outcome> {
        match export {
            "load" => return Ok(self.load.call_async(store, (packet,)).await?.0),
            "declare" => return Ok(self.declare.call_async(store, (packet,)).await?.0),
            "dispatch" => {}
            _ => return Err(wasmtime::Error::msg("unknown interpreter entrypoint")),
        }
        let (mut progress,) = self.start.call_async(&mut *store, (packet,)).await?;
        let mut pending = FuturesUnordered::new();
        loop {
            if let Some(outcome) = progress.outcome {
                return Ok(outcome);
            }
            if pending.len() + progress.effects.len() > 128 {
                return Err(wasmtime::Error::msg(
                    "interpreter host operation queue is full",
                ));
            }
            for effect in progress.effects {
                let receive = request(store.data(), effect.packet, true);
                pending.push(
                    async move {
                        let value = match receive {
                            Ok(receive) => receive
                                .await
                                .unwrap_or_else(|_| Err("host callback scope ended".into())),
                            Err(error) => Err(error),
                        };
                        Completion {
                            id: effect.id,
                            value,
                        }
                    }
                    .boxed(),
                );
            }
            let completed = if pending.is_empty() {
                futures::future::pending().await
            } else {
                pending.next().await.expect("nonempty effect queue")
            };
            let mut completed = vec![completed];
            while let Some(Some(ready)) = pending.next().now_or_never() {
                completed.push(ready);
            }
            progress = self.resume.call_async(&mut *store, (completed,)).await?.0;
        }
    }
}
