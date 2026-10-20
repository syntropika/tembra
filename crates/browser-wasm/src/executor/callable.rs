//! Deferred local capability calls. Logical revocation and physical completion are separate.

use super::{Generation, Job, Session, native::NativeScope, support::*};
use crate::boundary::failure;
use futures::future::LocalBoxFuture;
use js_sys::Promise;
use std::rc::{Rc, Weak};
use tembra_core::{Context, InvocationOwner, Result, RuntimeError};
use wasm_bindgen::{JsValue, closure::Closure, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

pub(crate) type Decoder = Box<dyn FnOnce() -> Result<JsValue>>;
pub(crate) type Body = Rc<dyn Fn(Decoder) -> Result<NativeCall>>;
pub(crate) trait Projection {
    fn admit(&mut self) -> Result<()>;
    fn refuse(&mut self, cause: RuntimeError);
    fn complete(self: Box<Self>, result: Result<()>);
}
pub(crate) struct NativeCall {
    future: LocalBoxFuture<'static, Result<JsValue>>,
    completion: Finish,
}
impl NativeCall {
    pub(crate) fn new(
        future: LocalBoxFuture<'static, Result<JsValue>>,
        completion: Box<dyn Projection>,
    ) -> Self {
        Self {
            future,
            completion: Finish(Some(completion)),
        }
    }
}
struct Finish(Option<Box<dyn Projection>>);
impl Finish {
    fn admit(&mut self) -> Result<()> {
        self.0
            .as_mut()
            .map_or(Ok(()), |completion| completion.admit())
    }
    fn complete(mut self, result: Result<()>) {
        if let Some(callback) = self.0.take() {
            callback.complete(result);
        }
    }
    fn refuse(&mut self, cause: RuntimeError) {
        if let Some(completion) = self.0.as_mut() {
            completion.refuse(cause);
        }
    }
}
impl Drop for Finish {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            callback.complete(Err(RuntimeError::aborted(
                "native_output_abandoned",
                "native output preparation was abandoned",
            )));
        }
    }
}
pub(crate) struct Slot {
    generation: Weak<Generation>,
    session: Weak<Session>,
    owner: InvocationOwner,
    context: Context,
    body: Body,
}
impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(session) = self.session.upgrade() {
            session.callbacks.set(session.callbacks.get() - 1);
        }
    }
}

#[wasm_bindgen(inline_js = r#"
const apply = Reflect.apply, create = Object.create, freeze = Object.freeze;
const define = Object.defineProperty, descriptor = Object.getOwnPropertyDescriptor;
const prototype = Object.getPrototypeOf, hasOwn = Object.hasOwn;
const setPrototype = Object.setPrototypeOf;
export function issued_callable(callback) {
  const callable = value => apply(callback, undefined, [value]);
  setPrototype(callable, null);
  return freeze(callable);
}
export function hook_record(input, call, recover) {
  const result = create(null);
  define(result, 'input', {value: input, enumerable: true});
  if (call !== undefined) {
    const next = create(null);
    define(next, 'call', {value:call, enumerable:true});
    define(next, 'recover', {value:recover, enumerable:true});
    define(result, 'next', {value:freeze(next), enumerable:true});
  }
  return freeze(result);
}
export function prepared_output(value) {
  if (value === null || (typeof value !== 'object' && typeof value !== 'function')) return value;
  let current = value;
  for (let count = 0; current !== null && count < 128; count++) {
    const property = descriptor(current, 'then');
    if (property !== undefined) {
      if (!hasOwn(property, 'value') || typeof property.value === 'function')
        throw {code:'invalid',reason:'native_thenable',
          message:'native output must not be a thenable',detailsJson:'null'};
      return value;
    }
    current = prototype(current);
  }
  if (current !== null)
    throw {code:'invalid',reason:'native_thenable',
      message:'native output prototype chain exceeds limit',detailsJson:'null'};
  return value;
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn issued_callable(callback: &JsValue) -> std::result::Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    pub(crate) fn hook_record(
        input: &JsValue,
        call: &JsValue,
        recover: &JsValue,
    ) -> std::result::Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    fn prepared_output(value: &JsValue) -> std::result::Result<JsValue, JsValue>;
}

/// Issuance uses a real entered scope. A saved JS trampoline retains no receiver or generation.
pub(crate) fn mint(scope: &NativeScope, context: &Context, body: Body) -> Result<JsValue> {
    let session = &scope.generation.session;
    context.check(scope.generation.host()?.host.id())?;
    if session.callbacks.get() >= 1024 {
        return Err(RuntimeError::unavailable(
            "callbacks_limit",
            "native callable capacity exhausted",
        ));
    }
    let id = session
        .next_slot
        .get()
        .checked_add(1)
        .filter(|id| *id <= 9_007_199_254_740_991)
        .ok_or_else(|| {
            RuntimeError::unavailable("callbacks_limit", "native callable identity exhausted")
        })?;
    session.next_slot.set(id);
    session.callbacks.set(session.callbacks.get() + 1);
    let slot = Rc::new(Slot {
        generation: Rc::downgrade(&scope.generation),
        session: Rc::downgrade(session),
        owner: scope.owner.clone(),
        context: context.clone(),
        body,
    });
    session.callables.borrow_mut().insert(id, slot);
    let weak = Rc::downgrade(session);
    if let Err(error) = context.on_settle(move || remove(&weak, id)) {
        remove(&Rc::downgrade(session), id);
        return Err(error);
    }
    let weak = Rc::downgrade(session);
    let callback = Closure::wrap(Box::new(move |input: JsValue| -> Promise {
        match prepare(&weak, id) {
            Ok((slot, generation, physical)) => {
                let session = generation.session.clone();
                if session.jobs.get() >= 128 || session.queued.get() >= 64 {
                    let error = RuntimeError::unavailable(
                        "jobs_limit",
                        "native callable job capacity exhausted",
                    )
                    .with_details(serde_json::json!({"callablePhase":"ingress"}));
                    return reject(error);
                }
                // Bounded ingress precedes the core attempt. A refused enqueue has no
                // continuation or projection to poison; a later accepted call may retry.
                let mut call = match (slot.body)(Box::new(move || Ok(input))) {
                    Ok(call) => call,
                    Err(error) => return reject(error),
                };
                if let Err(error) = call.completion.admit() {
                    // A rejected admission was represented successfully; it is not
                    // a failure of foreign output preparation.
                    call.completion.refuse(error.clone());
                    call.completion.complete(Ok(()));
                    drop(call.future);
                    return reject(error);
                }
                session.jobs.set(session.jobs.get() + 1);
                session.queued.set(session.queued.get() + 1);
                future_to_promise(async move {
                    let _generation = generation;
                    let _slot = slot;
                    let _job = Job {
                        session: session.clone(),
                        _physical: Some(physical),
                    };
                    session.queued.set(session.queued.get() - 1);
                    match call.future.await {
                        Ok(value) => match prepared_output(&value).map_err(js_error) {
                            Ok(value) => {
                                call.completion.complete(Ok(()));
                                Ok(value)
                            }
                            Err(error) => {
                                call.completion.complete(Err(error.clone()));
                                Err(failure(error))
                            }
                        },
                        Err(error) => {
                            let rejection = failure(error);
                            // A recorded downstream failure with a prepared rejection is
                            // recoverable. It is not a second sticky projection failure.
                            call.completion.complete(Ok(()));
                            Err(rejection)
                        }
                    }
                })
            }
            Err(error) => reject(error),
        }
    }) as Box<dyn Fn(JsValue) -> Promise>);
    let callback = callback.into_js_value();
    match issued_callable(&callback).map_err(js_error) {
        Ok(value) => Ok(value),
        Err(error) => {
            remove(&Rc::downgrade(session), id);
            Err(error)
        }
    }
}
fn remove(session: &Weak<Session>, id: u64) {
    if let Some(session) = session.upgrade() {
        let removed = session.callables.borrow_mut().remove(&id);
        drop(removed);
    }
}
type Prepared = (Rc<Slot>, Rc<Generation>, tembra_core::InvocationLease);
fn prepare(session: &Weak<Session>, id: u64) -> Result<Prepared> {
    let session = session.upgrade().ok_or_else(expired)?;
    let slot = session
        .callables
        .borrow()
        .get(&id)
        .cloned()
        .ok_or_else(expired)?;
    let generation = slot.generation.upgrade().ok_or_else(expired)?;
    slot.context.check(generation.host()?.host.id())?;
    let physical = slot.owner.retain(&slot.context)?;
    Ok((slot, generation, physical))
}
fn expired() -> RuntimeError {
    RuntimeError::aborted("expired", "native callable scope ended")
}
fn reject(error: RuntimeError) -> Promise {
    future_to_promise(async move { Err(failure(error)) })
}
