//! Activity adapters over actual core listeners and task roots, without JS lifecycle state.
use super::{
    Callback, Generation,
    context::WasmSignal,
    factory::{BindingOwner, WasmObjectRecorder},
    handles::{WasmResource, WasmState, generation},
    native::BrowserValue,
    support::*,
};
use crate::{boundary::failure, intrinsics::parse_portable_owned, readable::snapshot};
use futures::{FutureExt, future::LocalBoxFuture};
use js_sys::{Array, Function, Promise};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
use tembra_core::{Context, EventSource, ListenerOptions, Result, RuntimeError, TaskScope};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

type Select = dyn Fn(Rc<BrowserValue>) -> Result<EventSource<BrowserValue>>;
#[derive(Default)]
pub(crate) struct Cache {
    events: RefCell<Vec<Weak<EventBinding>>>,
    selectors: RefCell<Vec<Weak<SelectorBinding>>>,
}
#[derive(Default)]
pub(crate) struct OwnerBindings {
    selectors: Vec<Rc<SelectorBinding>>,
}
pub(crate) struct EventBinding {
    object: JsValue,
    generation: Rc<Generation>,
    source: EventSource<BrowserValue>,
}
pub(crate) struct SelectorBinding {
    callback: Rc<Callback>,
    source: Rc<Select>,
}
struct Delivery {
    listener: Rc<dyn Fn(BrowserValue)>,
}

#[wasm_bindgen(inline_js = r#"
const get = Reflect.get, apply = Reflect.apply;
export function subscribe_managed_source(source, sink) {
    const subscribe = get(source, 'subscribe');
    if (typeof subscribe !== 'function') throw new TypeError('event source requires subscribe');
    return apply(subscribe, source, [value => sink.emit(value)]);
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn subscribe_managed_source(
        source: &JsValue,
        sink: JsValue,
    ) -> std::result::Result<JsValue, JsValue>;
}

/// Weak source delivery. Saving the JS function cannot retain its actual subscription.
#[wasm_bindgen]
pub struct WasmEventSink {
    delivery: Weak<Delivery>,
}
#[wasm_bindgen]
impl WasmEventSink {
    /// # Errors
    /// Refuses delivery after detach. The source remains application-owned; its payload is local.
    pub fn emit(&self, value: JsValue) -> std::result::Result<(), JsValue> {
        let delivery = self
            .delivery
            .upgrade()
            .ok_or_else(|| failure(RuntimeError::aborted("expired", "event subscription ended")))?;
        (delivery.listener)(BrowserValue(value));
        Ok(())
    }
}
fn event_source(generation: &Rc<Generation>, object: JsValue) -> Result<Rc<EventBinding>> {
    if !object.is_object() || object.is_null() {
        return Err(error("event_source", "event source must be an object"));
    }
    let candidates = generation.session.activities.events.borrow().clone();
    for candidate in candidates
        .into_iter()
        .filter_map(|candidate| candidate.upgrade())
    {
        if candidate.object == object {
            return Ok(candidate);
        }
    }
    let binding = Rc::new_cyclic(|weak: &Weak<EventBinding>| {
        let weak = weak.clone();
        EventBinding {
            object,
            generation: generation.clone(),
            source: EventSource::new(move |listener| {
                weak.upgrade()
                    .ok_or_else(|| error("event_source", "event source ended"))?
                    .subscribe(listener)
            }),
        }
    });
    let mut cache = generation.session.activities.events.borrow_mut();
    cache.retain(|entry| entry.strong_count() > 0);
    cache.push(Rc::downgrade(&binding));
    Ok(binding)
}
impl EventBinding {
    fn subscribe(self: &Rc<Self>, listener: Rc<dyn Fn(BrowserValue)>) -> Result<Box<dyn FnOnce()>> {
        let delivery = Rc::new(Delivery { listener });
        let sink = WasmEventSink {
            delivery: Rc::downgrade(&delivery),
        };
        let detach =
            synchronous(subscribe_managed_source(&self.object, sink.into()).map_err(js_error)?)?;
        let detach = self.generation.callback(function(detach)?)?;
        let host = self.generation.session.host.clone();
        let binding = self.clone();
        Ok(Box::new(move || {
            let _keep_source = &binding;
            if let Err(error) = detach.sync(&Array::new())
                && let Some(host) = host.upgrade()
            {
                host.report(error);
            }
            drop(delivery);
        }))
    }
}
fn selector(generation: &Rc<Generation>, function: Function) -> Result<Rc<SelectorBinding>> {
    let candidates = generation.session.activities.selectors.borrow().clone();
    for candidate in candidates
        .into_iter()
        .filter_map(|candidate| candidate.upgrade())
    {
        if candidate.callback.function == function {
            return Ok(candidate);
        }
    }
    let callback = generation.callback(function)?;
    let invoke = callback.clone();
    let binding = Rc::new(SelectorBinding {
        callback,
        source: Rc::new(move |value| {
            let args = Array::new();
            args.push(&value.0);
            let source = invoke.sync(&args)?;
            let source = event_source(&invoke.generation, source)?;
            Ok(source.source.clone().retain_owner(source))
        }),
    });
    let mut cache = generation.session.activities.selectors.borrow_mut();
    cache.retain(|entry| entry.strong_count() > 0);
    cache.push(Rc::downgrade(&binding));
    Ok(binding)
}
fn options(raw: &JsValue) -> Result<ListenerOptions> {
    let raw = strict_json(raw)?;
    let object = raw
        .as_object()
        .ok_or_else(|| error("listener_options", "options must be an object"))?;
    match object.get("maxPending") {
        None => Ok(ListenerOptions::default()),
        Some(value) => ListenerOptions::new(
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    error("listener_options", "maxPending must be a positive integer")
                })?,
        ),
    }
}
fn deliver(
    owner: &Weak<BindingOwner>,
    callback: Rc<Callback>,
    context: Context,
    value: JsValue,
) -> LocalBoxFuture<'static, Result<()>> {
    let admitted = owner
        .upgrade()
        .ok_or_else(|| RuntimeError::aborted("expired", "listener owner ended"))
        .and_then(|owner| {
            owner
                .invocation
                .retain(&context)
                .map(|lease| (owner.invocation.clone(), lease))
        });
    async move {
        let (invocation, lease) = admitted?;
        let args = Array::new();
        args.push(&value);
        callback
            .invoke(Some(context), args, Some(lease), Some(invocation))
            .await?;
        Ok(())
    }
    .boxed_local()
}

#[wasm_bindgen]
impl WasmObjectRecorder {
    /// # Errors
    /// Refuses expired recording, a foreign state, checked limits or core declaration errors.
    #[wasm_bindgen(js_name = onState)]
    pub fn on_state(
        &self,
        name: JsValue,
        source: &WasmState,
        handler: Function,
        raw_options: JsValue,
    ) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        source.check_host(&self.owner.host).map_err(failure)?;
        let callback = self
            .owner
            .generation()
            .map_err(failure)?
            .callback(handler)
            .map_err(failure)?;
        let owner = Rc::downgrade(&self.owner);
        self.owner
            .recorder
            .on_with_options(
                string(&name).map_err(failure)?,
                source.source.clone(),
                move |value, context| {
                    let value =
                        parse_portable_owned(&snapshot(&value).to_string()).map_err(js_error);
                    let callback = callback.clone();
                    let owner = owner.clone();
                    async move { deliver(&owner, callback, context, value?).await }.boxed_local()
                },
                options(&raw_options).map_err(failure)?,
            )
            .map_err(failure)
    }
    /// # Errors
    /// Refuses expired recording, invalid source/limits and duplicate declarations. No subscribe
    /// callback runs until core activation/preparation admits the actual listener.
    #[wasm_bindgen(js_name = onEvent)]
    pub fn on_event(
        &self,
        name: JsValue,
        source: JsValue,
        handler: Function,
        raw_options: JsValue,
    ) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        let generation = self.owner.generation().map_err(failure)?;
        let source = event_source(&generation, source).map_err(failure)?;
        let callback = generation.callback(handler).map_err(failure)?;
        let owner = Rc::downgrade(&self.owner);
        self.owner
            .recorder
            .on_event_with_options(
                string(&name).map_err(failure)?,
                source.source.clone().retain_owner(source),
                move |value, context| deliver(&owner, callback.clone(), context, value.0),
                options(&raw_options).map_err(failure)?,
            )
            .map_err(failure)
    }
    /// # Errors
    /// Refuses expired recording, foreign resource, invalid selector/limits or core admission.
    /// Selection reads the actual resource only after the synchronous factory has ended.
    #[wasm_bindgen(js_name = onResource)]
    pub fn on_resource(
        &self,
        name: JsValue,
        resource: &WasmResource,
        select: Function,
        handler: Function,
        raw_options: JsValue,
    ) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        if !Weak::ptr_eq(&resource.host, &self.owner.host) {
            return Err(failure(error(
                "foreign_host",
                "resource belongs to another Host",
            )));
        }
        let generation = self.owner.generation().map_err(failure)?;
        let select = selector(&generation, select).map_err(failure)?;
        let callback = generation.callback(handler).map_err(failure)?;
        let owner = Rc::downgrade(&self.owner);
        self.owner
            .recorder
            .on_resource_with_options(
                string(&name).map_err(failure)?,
                resource.resource.upgrade().map_err(failure)?,
                select.source.clone(),
                move |value, context| deliver(&owner, callback.clone(), context, value.0),
                options(&raw_options).map_err(failure)?,
            )
            .map_err(failure)?;
        self.owner.activities.borrow_mut().selectors.push(select);
        Ok(())
    }
    /// # Errors
    /// Refuses expired recording, callback capacity or checked core task admission.
    pub fn task(&self, start: Function) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        let generation = self.owner.generation().map_err(failure)?;
        let callback = generation.callback(start).map_err(failure)?;
        self.owner
            .recorder
            .task(move |scope| {
                let callback = callback.clone();
                async move {
                    let args = Array::new();
                    args.push(
                        &WasmTaskScope {
                            scope,
                            generation: Rc::downgrade(&callback.generation),
                        }
                        .into(),
                    );
                    callback.invoke(None, args, None, None).await?;
                    Ok(())
                }
                .boxed_local()
            })
            .map_err(failure)
    }
}

/// Weak activity authority. Every root selects the Host's actual admitted current epoch.
#[wasm_bindgen]
pub struct WasmTaskScope {
    scope: TaskScope,
    generation: Weak<Generation>,
}
#[wasm_bindgen]
impl WasmTaskScope {
    pub fn signal(&self) -> WasmSignal {
        WasmSignal::facet(self.scope.signal())
    }
    /// # Errors
    /// Refuses an ended generation, callback capacity or actual stopped-task/Host admission.
    /// Dropping the observer does not terminate independently admitted callback work.
    pub fn root(&self, callback: Function) -> std::result::Result<Promise, JsValue> {
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(callback)
            .map_err(failure)?;
        let lease = self.scope.retain().map_err(failure)?;
        let future = self.scope.root(move |context| {
            async move {
                callback
                    .invoke(Some(context), Array::new(), Some(lease), None)
                    .await
            }
            .boxed_local()
        });
        Ok(future_to_promise(
            async move { future.await.map_err(failure) },
        ))
    }
}
