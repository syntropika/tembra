//! Deliver native replies only outside an active V8 event-loop poll.
use super::*;

pub(super) type Completion = (tembra::core::Id, std::result::Result<Vec<u8>, String>);

pub(super) fn receiver(js: &mut JsRuntime) -> Option<v8::Global<v8::Function>> {
    deno_core::scope!(scope, js);
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "__tembraDeliver")?;
    let value = global.get(scope, key.into())?;
    global.delete(scope, key.into());
    v8::Local::<v8::Function>::try_from(value)
        .ok()
        .map(|function| v8::Global::new(scope, function))
}

pub(super) fn deliver(
    js: &mut JsRuntime,
    receiver: &v8::Global<v8::Function>,
    completed: Vec<Completion>,
) -> Result<usize> {
    let count = completed.len();
    deno_core::scope!(scope, js);
    let batch = v8::Array::new(scope, count as i32);
    for (index, (id, result)) in completed.into_iter().enumerate() {
        let row = v8::Array::new(scope, 3);
        let id = v8::Number::new(scope, id as f64);
        row.set_index(scope, 0, id.into());
        let success = result.is_ok();
        match result {
            Ok(bytes) => {
                let length = bytes.len();
                let backing =
                    v8::ArrayBuffer::new_backing_store_from_boxed_slice(bytes.into_boxed_slice())
                        .make_shared();
                let buffer = v8::ArrayBuffer::with_backing_store(scope, &backing);
                let view = v8::Uint8Array::new(scope, buffer, 0, length).ok_or_else(|| {
                    Error::invalid(Reason::Memory, "completion allocation failed")
                })?;
                row.set_index(scope, 1, view.into());
            }
            Err(message) => {
                let message = v8::String::new(scope, &message).ok_or_else(|| {
                    Error::invalid(Reason::Memory, "completion error allocation failed")
                })?;
                row.set_index(scope, 1, message.into());
            }
        }
        let success = v8::Boolean::new(scope, success);
        row.set_index(scope, 2, success.into());
        batch.set_index(scope, index as u32, row.into());
    }
    let function = v8::Local::new(scope, receiver);
    let this = v8::undefined(scope);
    function
        .call(scope, this.into(), &[batch.into()])
        .ok_or_else(|| Error::invalid(Reason::Javascript, "completion delivery failed"))?;
    Ok(count)
}
