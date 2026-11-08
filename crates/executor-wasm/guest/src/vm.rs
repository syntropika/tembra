//! Synchronous VM access keeps Rust futures out of the serializable JavaScript heap.
use futures::future::AbortHandle;
use rquickjs::{
    Context, Function, Promise, Runtime,
    context::EvalOptions,
    promise::PromiseState,
    snapshot::{SnapshotHost, SnapshotKind},
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Load {
    source: String,
    heap_bytes: usize,
    contract: Value,
    #[serde(default = "default_ticks")]
    instruction_ticks: u32,
}
fn default_ticks() -> u32 {
    10000
}
#[derive(serde::Deserialize)]
pub(super) struct Effect {
    pub id: u64,
    pub packet: String,
}

pub(super) struct Vm {
    pub context: Context,
    pub runtime: Runtime,
    pub accepting: Cell<bool>,
    pub interrupted: Rc<Cell<bool>>,
    pub interrupt: RefCell<Option<AbortHandle>>,
    ticks: Rc<Cell<u32>>,
    tick_limit: u32,
    host: SnapshotHost,
}
impl Vm {
    /// Configure runtime-only bounds again rather than serializing interrupt handlers or host cells.
    fn runtime(configuration: &Load) -> Result<(Runtime, Rc<Cell<u32>>, Rc<Cell<bool>>), String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        runtime.set_memory_limit(configuration.heap_bytes);
        runtime.set_max_stack_size(64 << 10);
        let ticks = Rc::new(Cell::new(configuration.instruction_ticks));
        let interrupted = Rc::new(Cell::new(false));
        let budget = ticks.clone();
        let stopped = interrupted.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let remaining = budget.get();
            budget.set(remaining.saturating_sub(1));
            if remaining == 0 {
                stopped.set(true);
            }
            remaining == 0
        })));
        Ok((runtime, ticks, interrupted))
    }
    /// Load TypeScript and npm's closed bundle without evaluating it in the outer host.
    pub fn load(packet: &str) -> Result<Self, String> {
        let configuration: Load = serde_json::from_str(packet).map_err(|e| e.to_string())?;
        let (runtime, ticks, interrupted) = Self::runtime(&configuration)?;
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        let host = super::io::host();
        context.with(|ctx| -> Result<(), String> {
            let result = (|| -> rquickjs::Result<()> {
                super::io::install(&ctx, &host)?;
                ctx.globals().set(
                    "__tembraContract",
                    ctx.json_parse(configuration.contract.to_string())?,
                )?;
                ctx.eval::<(), _>(include_str!("bridge.js"))?;
                ctx.eval::<(), _>(include_str!("state.js"))?;
                ctx.eval::<(), _>(include_str!("values.js"))?;
                ctx.eval::<(), _>(include_str!("runtime.js"))?;
                let mut options = EvalOptions::default();
                options.typescript = true;
                ctx.eval_with_options::<(), _>(configuration.source.clone(), options)
            })();
            result.map_err(|error| format!("{error:?}: {:?}", ctx.catch()))
        })?;
        let vm = Self {
            context,
            runtime,
            accepting: Cell::new(false),
            interrupted,
            interrupt: RefCell::new(None),
            ticks,
            tick_limit: configuration.instruction_ticks,
            host,
        };
        vm.pump()?;
        Ok(vm)
    }
    /// Read and consume jobs only between context borrows, never across a Rust await.
    pub fn pump(&self) -> Result<(), String> {
        loop {
            match self.runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(error) => return Err(format!("pending JavaScript job failed: {error:?}")),
            }
        }
    }
    /// Call one synchronous engine helper and preserve its JavaScript exception text.
    pub fn helper(&self, name: &str, packet: Option<String>) -> Result<String, String> {
        self.context.with(|ctx| {
            let result = (|| -> rquickjs::Result<String> {
                let function: Function = ctx.globals().get(name)?;
                if let Some(packet) = packet {
                    function.call((packet,))
                } else {
                    function.call(())
                }
            })();
            result.map_err(|error| format!("{error:?}: {:?}", ctx.catch()))
        })
    }
    /// Reset a callback budget only at its start, not whenever an async effect yields.
    pub fn begin(&self) {
        self.interrupted.set(false);
        self.ticks.set(self.tick_limit);
        self.accepting.set(true);
    }
    /// Source factories remain synchronous even if platform bindings use an asynchronous wrapper.
    pub fn declare(&self, packet: String) -> Result<String, String> {
        self.begin();
        let result = self.helper("__tembraDeclare", Some(packet));
        self.finish(result)
    }
    /// Retain the main Promise in the heap rather than attaching Rust future callbacks to it.
    pub fn start(&self, packet: String) -> Result<(), String> {
        self.begin();
        self.context.with(|ctx| {
            let result = (|| -> rquickjs::Result<()> {
                let dispatch: Function = ctx.globals().get("__tembraDispatch")?;
                let promise: Promise = dispatch.call((packet,))?;
                ctx.globals().set("__tembraPendingDispatch", promise)
            })();
            result.map_err(|error| format!("{error:?}: {:?}", ctx.catch()))
        })
    }
    /// Observe a settled Promise without creating any Rust-class object in the heap.
    pub fn outcome(&self) -> Result<Option<Result<String, String>>, String> {
        self.context.with(|ctx| {
            let promise: Promise = ctx
                .globals()
                .get("__tembraPendingDispatch")
                .map_err(|e| e.to_string())?;
            if promise.state() == PromiseState::Pending {
                return Ok(None);
            }
            Ok(Some(
                promise
                    .result::<String>()
                    .expect("settled Promise")
                    .map_err(|error| format!("{error:?}: {:?}", ctx.catch())),
            ))
        })
    }
    /// Drain data-only effect requests for the outer host to admit under the current scope.
    pub fn effects(&self) -> Result<Vec<Effect>, String> {
        serde_json::from_str(&self.helper("__tembraTakeEffects", None)?).map_err(|e| e.to_string())
    }
    /// Deliver one outcome to its original Promise; unknown and duplicate effects are refused.
    pub fn complete(&self, id: u64, result: Result<String, String>) -> Result<(), String> {
        let packet = match result {
            Ok(value) => json!({"id":id,"ok":true,"value":value}),
            Err(error) => json!({"id":id,"ok":false,"value":error}),
        };
        self.context.with(|ctx| {
            let result = (|| -> rquickjs::Result<()> {
                let function: Function = ctx.globals().get("__tembraCompleteEffect")?;
                function.call((packet.to_string(),))
            })();
            result.map_err(|error| format!("{error:?}: {:?}", ctx.catch()))
        })
    }
    /// Revoke native authority before settling unobserved Promise continuations.
    pub fn stop(&self) -> Result<(), String> {
        self.accepting.set(false);
        // Revoked callbacks may run rejection handlers, but never receive an unbounded drain.
        self.ticks.set(default_ticks());
        let result = self
            .context
            .with(|ctx| {
                let function: Function = ctx
                    .globals()
                    .get("__tembraStopEffects")
                    .map_err(|e| e.to_string())?;
                function.call::<_, ()>(()).map_err(|e| e.to_string())
            })
            .and_then(|()| self.pump());
        if result.is_err() {
            self.interrupted.set(true);
        }
        result
    }
    /// Preserve the original callback failure if bounded cleanup also fails.
    pub fn finish(&self, outcome: Result<String, String>) -> Result<String, String> {
        match (outcome, self.stop()) {
            (outcome, Ok(())) => outcome,
            (Ok(_), Err(cleanup)) => Err(format!("interpreter cleanup failed: {cleanup}")),
            (Err(primary), Err(cleanup)) => {
                Err(format!("{primary}; interpreter cleanup failed: {cleanup}"))
            }
        }
    }
    /// Capture only idle engine state; managed continuation reattachment is a separate host protocol.
    pub fn capture(&self) -> Result<Vec<u8>, String> {
        if self.accepting.get() {
            return Err("capture requires closed admission".into());
        }
        self.context.with(|ctx| {
            ctx.write_snapshot(&self.host, SnapshotKind::Full, &[])
                .map_err(|error| format!("{error:?}: {:?}", ctx.catch()))
        })
    }
}
