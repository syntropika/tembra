//! Host operations exposed to the embedded JS realm.

use super::*;

pub(super) struct Bindings {
    pub(super) kernel: tembra_bridge::Kernel,
    pub(super) native: Host,
    pub(super) codec: Rc<dyn tembra::codec::Codec>,
    pub(super) started: Instant,
    pub(super) timers: RefCell<BTreeMap<u32, AbortHandle>>,
}

/// Execute metadata requests against the shared Rust kernel.
#[op2]
#[string]
fn op_tembra_kernel(state: &mut OpState, #[string] request: String) -> String {
    state.borrow::<Rc<Bindings>>().kernel.request(&request)
}

/// Check whether Rust owns the requested implementation.
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

/// Encode a native value using the host codec.
#[op2]
#[buffer]
fn op_tembra_value(
    state: &mut OpState,
    id: f64,
) -> std::result::Result<Vec<u8>, deno_error::JsErrorBox> {
    let bindings = state.borrow::<Rc<Bindings>>();
    encode_answer(bindings, bindings.native.portable_value(number_id(id)?))
}

/// Read the codec identity required by the JS adapter.
#[op2]
#[string]
fn op_tembra_codec(state: &mut OpState) -> String {
    state.borrow::<Rc<Bindings>>().codec.id().into()
}

/// Release a native bridge value after kernel retirement.
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

/// Reject numeric identities outside the shared exact-integer range.
fn number_id(id: f64) -> std::result::Result<tembra::core::Id, deno_error::JsErrorBox> {
    if !(0.0..=tembra::core::MAX_ID as f64).contains(&id) || id.fract() != 0.0 {
        return Err(deno_error::JsErrorBox::generic(
            "identity must be a safe unsigned integer",
        ));
    }
    Ok(id as tembra::core::Id)
}

/// Read elapsed monotonic time from the runtime origin.
#[op2(fast)]
fn op_tembra_now(state: &mut OpState) -> f64 {
    state
        .borrow::<Rc<Bindings>>()
        .started
        .elapsed()
        .as_secs_f64()
        * 1000.0
}

/// Cancel a scheduled adapter timer.
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

/// Wait for a bounded delay or timer cancellation.
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

/// Execute one native callback selected by the JS adapter.
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

/// Encode a native result or its structured error envelope.
pub(super) fn encode_answer(
    bindings: &Bindings,
    result: Result<Value>,
) -> std::result::Result<Vec<u8>, deno_error::JsErrorBox> {
    let value = match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => {
            json!({
                "error": {"code": error.code.as_str(), "reason": error.reason, "message": error.message},
            })
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
