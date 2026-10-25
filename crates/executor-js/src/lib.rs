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

#[derive(Clone)]
pub struct TokioExecutor;
impl LocalSpawn for TokioExecutor {
    fn spawn_local_obj(
        &self,
        future: LocalFutureObj<'static, ()>,
    ) -> std::result::Result<(), SpawnError> {
        tokio::task::spawn_local(future);
        Ok(())
    }
}
struct Bindings {
    kernel: tembra_bridge::Kernel,
    native: Host,
    codec: Rc<dyn tembra::codec::Codec>,
    started: Instant,
    timers: RefCell<BTreeMap<u32, AbortHandle>>,
}

#[op2]
#[string]
fn op_tembra_kernel(state: &mut OpState, #[string] request: String) -> String {
    state.borrow::<Rc<Bindings>>().kernel.request(&request)
}
#[op2(fast)]
fn op_tembra_has_facet(
    state: &mut OpState,
    id: f64,
) -> std::result::Result<bool, deno_error::JsErrorBox> {
    Ok(state
        .borrow::<Rc<Bindings>>()
        .native
        .has_facet(number_id(id)?))
}
#[op2]
#[buffer]
fn op_tembra_value(
    state: &mut OpState,
    id: f64,
) -> std::result::Result<Vec<u8>, deno_error::JsErrorBox> {
    let bindings = state.borrow::<Rc<Bindings>>();
    encode_answer(bindings, bindings.native.portable_value(number_id(id)?))
}
#[op2]
#[string]
fn op_tembra_codec(state: &mut OpState) -> String {
    state.borrow::<Rc<Bindings>>().codec.id().into()
}
#[op2(fast)]
fn op_tembra_drop_value(
    state: &mut OpState,
    id: f64,
) -> std::result::Result<(), deno_error::JsErrorBox> {
    state
        .borrow::<Rc<Bindings>>()
        .native
        .drop_value(number_id(id)?);
    Ok(())
}

fn number_id(id: f64) -> std::result::Result<tembra::core::Id, deno_error::JsErrorBox> {
    if !(0.0..=tembra::core::MAX_ID as f64).contains(&id) || id.fract() != 0.0 {
        return Err(deno_error::JsErrorBox::generic(
            "identity must be a safe unsigned integer",
        ));
    }
    Ok(id as tembra::core::Id)
}
#[op2(fast)]
fn op_tembra_now(state: &mut OpState) -> f64 {
    state
        .borrow::<Rc<Bindings>>()
        .started
        .elapsed()
        .as_secs_f64()
        * 1000.0
}
#[op2(fast)]
fn op_tembra_clear_timer(state: &mut OpState, id: u32) {
    if let Some(timer) = state
        .borrow::<Rc<Bindings>>()
        .timers
        .borrow_mut()
        .remove(&id)
    {
        timer.abort();
    }
}
#[op2]
async fn op_tembra_sleep(state: Rc<RefCell<OpState>>, id: u32, milliseconds: u32) {
    let bindings = state.borrow().borrow::<Rc<Bindings>>().clone();
    let (abort, registration) = AbortHandle::new_pair();
    bindings.timers.borrow_mut().insert(id, abort);
    let _ = Abortable::new(
        tokio::time::sleep(Duration::from_millis(milliseconds.into())),
        registration,
    )
    .await;
    bindings.timers.borrow_mut().remove(&id);
}
#[op2]
#[buffer]
async fn op_tembra_native(
    state: Rc<RefCell<OpState>>,
    #[string] operation: String,
    #[buffer] payload: JsBuffer,
) -> std::result::Result<Vec<u8>, deno_error::JsErrorBox> {
    let bindings = state.borrow().borrow::<Rc<Bindings>>().clone();
    let native = bindings.native.clone();
    let result = async {
        let value = bindings.codec.deserialize(&payload)?;
        let id = |name: &str| -> Result<tembra::core::Id> {
            value[name]
                .as_u64()
                .filter(|number| *number <= tembra::core::MAX_ID)
                .ok_or_else(|| Error::invalid(Reason::Arguments, name))
        };
        match NativeOp::parse(&operation) {
            Some(NativeOp::Activate) => {
                native.activate(
                    id("object")?,
                    id("facet")?,
                    value["service"]
                        .as_str()
                        .ok_or_else(|| Error::invalid(Reason::Service, "missing service"))?,
                )?;
                Ok(Value::Null)
            }
            Some(NativeOp::Dispose) => {
                native.dispose(id("object")?).await?;
                Ok(Value::Null)
            }
            Some(NativeOp::CancelEffect) => {
                native.cancel_effect(&tembra::core::Effect::Open {
                    resource: id("resource")?,
                });
                Ok(Value::Null)
            }
            Some(NativeOp::Effect) => {
                use tembra::core::Effect;
                let effect = match value["type"].as_str().and_then(EffectType::parse) {
                    Some(EffectType::Open) => Effect::Open {
                        resource: id("resource")?,
                    },
                    Some(EffectType::Close) => Effect::Close {
                        resource: id("resource")?,
                    },
                    Some(EffectType::Drop) => Effect::Drop {
                        scope: id("scope")?,
                    },
                    Some(EffectType::Stop) => Effect::Stop {
                        scope: id("scope")?,
                        keep_listeners: false,
                    },
                    Some(EffectType::Prepare) => Effect::Prepare {
                        scope: id("scope")?,
                        previous: None,
                    },
                    Some(EffectType::Start) => Effect::Start {
                        scope: id("scope")?,
                    },
                    _ => return Err(Error::invalid(Reason::NativeEffect, "unsupported effect")),
                };
                Ok(native
                    .execute_effect(&effect)
                    .await?
                    .map_or(Value::Null, |id| json!(id)))
            }
            Some(NativeOp::Invoke) => {
                let invocation = &value["invocation"];
                let call = native.kernel().invocation(
                    invocation["id"]
                        .as_u64()
                        .filter(|id| *id <= tembra::core::MAX_ID)
                        .ok_or_else(|| Error::invalid(Reason::Invocation, "missing invocation"))?,
                )?;
                native
                    .invoke_portable(call, value["arguments"].clone())
                    .await
            }
            _ => Err(Error::invalid(Reason::Operation, operation)),
        }
    }
    .await;
    encode_answer(&bindings, result)
}
fn encode_answer(
    bindings: &Bindings,
    result: Result<Value>,
) -> std::result::Result<Vec<u8>, deno_error::JsErrorBox> {
    let value = match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => {
            json!({"error": {"code": error.code.as_str(), "reason": error.reason, "message": error.message}})
        }
    };
    bindings
        .codec
        .serialize(&value)
        .map_err(|error| deno_error::JsErrorBox::generic(error.to_string()))
}
extension!(
    tembra_ops,
    ops = [
        op_tembra_kernel,
        op_tembra_has_facet,
        op_tembra_codec,
        op_tembra_value,
        op_tembra_drop_value,
        op_tembra_now,
        op_tembra_clear_timer,
        op_tembra_sleep,
        op_tembra_native
    ]
);

/// One V8 environment per host. Only prebundled JavaScript is accepted; dependencies are resolved
/// by SDK tooling before execution. This adapter provides neither Node nor DOM APIs.
pub struct Runtime {
    js: JsRuntime,
    native: Host,
    codec: Rc<dyn tembra::codec::Codec>,
}
impl Runtime {
    pub fn new(native: Host, sdk_source: impl Into<String>) -> Result<Self> {
        Self::with_codec(
            native,
            sdk_source,
            Rc::new(tembra::codec::Json),
            "Tembra.jsonCodec",
        )
    }

    pub fn with_codec(
        native: Host,
        sdk_source: impl Into<String>,
        codec: Rc<dyn tembra::codec::Codec>,
        js_codec: &str,
    ) -> Result<Self> {
        native.delegate_actions();
        let mut js = JsRuntime::new(RuntimeOptions {
            extensions: vec![tembra_ops::init()],
            ..Default::default()
        });
        js.op_state().borrow_mut().put(Rc::new(Bindings {
            kernel: tembra_bridge::Kernel {
                host: native.kernel(),
            },
            native: native.clone(),
            codec: codec.clone(),
            started: Instant::now(),
            timers: RefCell::new(BTreeMap::new()),
        }));
        js.execute_script("tembra:runtime", sdk_source.into())
            .map_err(js_error)?;
        js.execute_script(
            "tembra:host",
            format!("globalThis.host = Tembra.createHost({{ serialization: ({js_codec}) }});"),
        )
        .map_err(js_error)?;
        Ok(Self { js, native, codec })
    }
    pub fn native(&self) -> &Host {
        &self.native
    }

    /// Calls an installed JS or Rust service through the shared managed host. Conversion occurs
    /// once at this language boundary; business logic and validation execute in the owning facet.
    pub async fn call(
        &mut self,
        service: &str,
        key: &[String],
        method: &str,
        arguments: Value,
    ) -> Result<Value> {
        let request =
            json!({ "service": service, "key": key, "method": method, "arguments": arguments });
        let bytes = self.codec.serialize(&request)?;
        {
            deno_core::scope!(scope, self.js);
            let length = bytes.len();
            let backing =
                v8::ArrayBuffer::new_backing_store_from_boxed_slice(bytes.into_boxed_slice())
                    .make_shared();
            let buffer = v8::ArrayBuffer::with_backing_store(scope, &backing);
            let bytes = v8::Uint8Array::new(scope, buffer, 0, length)
                .ok_or_else(|| Error::invalid(Reason::Memory, "cannot allocate boundary value"))?;
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__tembraRequest")
                .ok_or_else(|| Error::invalid(Reason::Memory, "cannot allocate boundary key"))?;
            global.set(scope, key.into(), bytes.into());
        }
        let value = self.resolve_script("(async () => { const codec = host.session.codec; const request = codec.deserialize(globalThis.__tembraRequest); delete globalThis.__tembraRequest; const reference = await host.get(request.service, request.key); try { const result = await reference[request.method](...request.arguments); return codec.serialize(result ?? null); } finally { reference.release(); } })()").await?;
        let bytes = {
            deno_core::scope!(scope, self.js);
            let value = v8::Local::new(scope, value);
            let view = v8::Local::<v8::Uint8Array>::try_from(value).map_err(|_| {
                Error::invalid(Reason::ByteResult, "codec must return a Uint8Array")
            })?;
            let buffer = view
                .buffer(scope)
                .ok_or_else(|| Error::invalid(Reason::ByteResult, "codec result has no buffer"))?;
            if buffer.was_detached() {
                return Err(Error::invalid(
                    Reason::ByteResult,
                    "codec result buffer is detached",
                ));
            }
            let length = view.byte_length();
            // The safe V8 copying API takes an i32-sized destination, including shared views.
            if length > i32::MAX as usize {
                return Err(Error::invalid(
                    Reason::ByteResult,
                    "codec result is too large",
                ));
            }
            let mut bytes = vec![0; length];
            if view.copy_contents(&mut bytes) != length {
                return Err(Error::invalid(
                    Reason::ByteResult,
                    "codec result copy was incomplete",
                ));
            }
            bytes
        };
        self.codec.deserialize(&bytes)
    }

    /// Waits for a JavaScript result while driving V8's asynchronous operations.
    pub async fn evaluate(&mut self, source: impl Into<String>) -> Result<Value> {
        let value = self.resolve_script(source).await?;
        deno_core::scope!(scope, self.js);
        let local = v8::Local::new(scope, value);
        deno_core::serde_v8::from_v8(scope, local).map_err(js_error)
    }

    async fn resolve_script(&mut self, source: impl Into<String>) -> Result<v8::Global<v8::Value>> {
        let value = self
            .js
            .execute_script("tembra:application", source.into())
            .map_err(js_error)?;
        let resolved = self.js.resolve(value);
        self.js
            .with_event_loop_promise(resolved, Default::default())
            .await
            .map_err(js_error)
    }
    pub async fn install(&mut self, name: &str, bundle: String) -> Result<()> {
        self.bundle(name, bundle, false).await
    }
    pub async fn reload(&mut self, name: &str, bundle: String) -> Result<()> {
        self.bundle(name, bundle, true).await
    }
    async fn bundle(&mut self, _name: &str, bundle: String, reload: bool) -> Result<()> {
        // Executable bundles are scripts, so retired facets are not pinned in V8's ESM module map.
        self.js
            .execute_script("tembra:facet", bundle)
            .map_err(js_error)?;
        let operation = if reload {
            "host.reload([globalThis.__tembraFacet])"
        } else {
            "host.install(globalThis.__tembraFacet)"
        };
        self.evaluate(format!("(async () => {{ try {{ await {operation}; return null; }} finally {{ delete globalThis.__tembraFacet; }} }})()")).await?;
        Ok(())
    }
    pub async fn close(&mut self) -> Result<()> {
        self.evaluate("host.close().then(() => null)").await?;
        Ok(())
    }
}
fn js_error(error: impl std::fmt::Display) -> Error {
    Error::invalid(Reason::Javascript, error.to_string())
}
