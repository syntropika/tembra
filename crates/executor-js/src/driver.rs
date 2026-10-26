//! Compose existing kernel control operations and retain native submission results.
use super::bindings::{Bindings, encode_answer, execute_native, number_id};
use super::*;

/// Select the completion and control path before the SDK constructs its adapter.
#[op2(fast)]
fn op_tembra_driver_enabled(state: &mut OpState) -> bool {
    state.borrow::<Rc<Bindings>>().driver.is_some()
}

/// Fuse context recheck, admission and ownership routing at the adapter.
#[op2]
#[string]
fn op_tembra_admit(state: &mut OpState, #[string] request: String) -> String {
    let bindings = state.borrow::<Rc<Bindings>>();
    let result = (|| -> Result<Value> {
        let call = bindings.kernel.admit_call_batch(&request)?;
        let orb = call["orb"]
            .as_u64()
            .expect("kernel invocation has an orb identity");
        Ok(json!({"call":call,"native":bindings.native.has_orb(orb)}))
    })();
    metadata_answer(result)
}

/// Fuse finish bookkeeping, action drain and next-deadline read.
#[op2]
#[string]
fn op_tembra_finish(state: &mut OpState, call: f64, #[string] now: String) -> String {
    let bindings = state.borrow::<Rc<Bindings>>();
    let result = number_id(call)
        .map_err(|e| Error::invalid(Reason::Invocation, e.to_string()))
        .and_then(|call| {
            let now = now.parse::<u64>().map_err(|_| {
                Error::invalid(
                    Reason::Metadata,
                    "now must be an unsigned revision or clock",
                )
            })?;
            bindings.kernel.finish_call_batch(call, now)
        });
    metadata_answer(result)
}
fn metadata_answer(result: Result<Value>) -> String {
    match result {
        Ok(value)=>json!({"ok":value}),
        Err(error)=>json!({"error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}}),
    }.to_string()
}

/// Submit work without registering an async Deno waiter for each invocation.
#[op2(fast)]
fn op_tembra_submit(
    state: &mut OpState,
    #[string] operation: String,
    #[buffer] payload: &[u8],
) -> std::result::Result<f64, deno_error::JsErrorBox> {
    use futures::task::LocalSpawnExt;
    let bindings = state.borrow::<Rc<Bindings>>().clone();
    let driver = bindings
        .driver
        .clone()
        .ok_or_else(|| deno_error::JsErrorBox::generic("cooperative driver is not attached"))?;
    let id = bindings
        .native
        .kernel()
        .allocate_handle()
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    let payload = payload.to_vec();
    let completion_wake = driver.clone();
    driver
        .spawn_local(async move {
            let result = execute_native(&bindings, operation, &payload).await;
            let encoded = encode_answer(&bindings, result).map_err(|e| e.to_string());
            bindings.completions.borrow_mut().push((id, encoded));
            completion_wake.wake_runtime();
        })
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    Ok(id as f64)
}

extension!(
    tembra_driver_ops,
    ops = [
        op_tembra_driver_enabled,
        op_tembra_admit,
        op_tembra_finish,
        op_tembra_submit
    ]
);
