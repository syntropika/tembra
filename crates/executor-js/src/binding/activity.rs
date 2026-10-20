//! Managed activity callbacks use core lifecycle ownership and fresh Contexts.
use super::*;
use futures::future::LocalBoxFuture;

#[derive(Default)]
pub(crate) struct Cache {
    events: RefCell<Vec<Weak<EventBinding>>>,
    selectors: RefCell<Vec<Weak<SelectorBinding>>>,
}

pub(crate) struct EventBinding {
    object: v8::Global<v8::Object>,
    module: Rc<Module>,
    source: EventSource<JsValue>,
    _setups: Vec<Rc<Owner>>,
}

type Select = dyn Fn(Rc<JsValue>) -> Result<EventSource<JsValue>>;

pub(crate) struct SelectorBinding {
    callback: Rc<Callback>,
    source: Rc<Select>,
}

pub(crate) struct Delivery {
    listener: Rc<dyn Fn(JsValue)>,
    // The issued function has weak authority back to this receiver. The detach operation
    // owns the receiver, so retained JavaScript functions cannot keep subscriptions alive.
    _owner: Rc<Owner>,
}

fn event_source(
    engine: &Rc<Engine>,
    module: &Rc<Module>,
    scope: &mut Scope<'_, '_>,
    object: v8::Local<v8::Object>,
    setups: Vec<Rc<Owner>>,
) -> Rc<EventBinding> {
    let candidates = engine.activities.events.borrow().clone();
    for candidate in candidates
        .into_iter()
        .filter_map(|candidate| candidate.upgrade())
    {
        if candidate.module.host == module.host
            && v8::Local::new(scope, &candidate.object).strict_equals(object.into())
        {
            return candidate;
        }
    }
    let object = v8::Global::new(scope, object);
    let binding = Rc::new_cyclic(|weak: &Weak<EventBinding>| {
        let weak = weak.clone();
        EventBinding {
            object,
            module: module.clone(),
            _setups: setups,
            source: EventSource::new(move |listener| {
                weak.upgrade()
                    .ok_or_else(crate::slots::expired)?
                    .subscribe(listener)
            }),
        }
    });
    let mut cache = engine.activities.events.borrow_mut();
    cache.retain(|entry| entry.strong_count() > 0);
    cache.push(Rc::downgrade(&binding));
    binding
}

impl EventBinding {
    fn subscribe(self: &Rc<Self>, listener: Rc<dyn Fn(JsValue)>) -> Result<Box<dyn FnOnce()>> {
        let module = self.module.clone();
        let engine = module.engine.clone();
        let receiver = Owner::new(&module, None);
        let delivery = Rc::new(Delivery {
            listener,
            _owner: receiver.clone(),
        });
        let cap = Capability::new(
            &module,
            Some(&receiver),
            Kind::EventSink(Rc::downgrade(&delivery)),
        )?;
        let detach = engine.with_scope(|scope| {
            v8::tc_scope!(let scope, scope);
            let object = v8::Local::new(scope, &self.object);
            let subscribe = function(get(scope, object, "subscribe")?)?;
            let listener = wrap(&engine, scope, &cap)?;
            let detach = subscribe
                .call(scope, object.into(), &[listener])
                .ok_or_else(|| {
                    let caught = scope.exception();
                    exception(scope, caught, "event subscription threw")
                })?;
            helper(&engine, scope, "synchronous", &[detach])?;
            callback(&module, detach, scope)
        })?;
        let binding = self.clone();
        Ok(Box::new(move || {
            let _keep_source = &binding;
            // Ending the subscription revokes the retained event function even if detach
            // throws. Report the actual error; no raw event replay or rollback is implied.
            if let Err(error) = engine.direct(&detach, vec![], scope::CallKind::Void) {
                engine.report(error);
            }
            drop(delivery);
        }))
    }
}

fn selector<'s>(
    engine: &Rc<Engine>,
    module: &Rc<Module>,
    scope: &mut Scope<'s, '_>,
    function: v8::Local<'s, v8::Value>,
    owner: &Rc<Owner>,
) -> Result<Rc<SelectorBinding>> {
    let function = super::function(function)?;
    let candidates = engine.activities.selectors.borrow().clone();
    for candidate in candidates
        .into_iter()
        .filter_map(|candidate| candidate.upgrade())
    {
        if candidate.callback.module.host == module.host
            && v8::Local::new(scope, &candidate.callback.function).strict_equals(function.into())
        {
            return Ok(candidate);
        }
    }
    let callback = callback_for(owner, function.into(), scope)?;
    let binding = Rc::new_cyclic(|weak: &Weak<SelectorBinding>| {
        let weak = weak.clone();
        SelectorBinding {
            callback,
            source: Rc::new(move |value| {
                weak.upgrade()
                    .ok_or_else(crate::slots::expired)?
                    .select(value)
            }),
        }
    });
    let mut cache = engine.activities.selectors.borrow_mut();
    cache.retain(|entry| entry.strong_count() > 0);
    cache.push(Rc::downgrade(&binding));
    Ok(binding)
}

impl SelectorBinding {
    fn select(&self, value: Rc<JsValue>) -> Result<EventSource<JsValue>> {
        let module = &self.callback.module;
        if value.module.host != module.host || value.module.engine.id != module.engine.id {
            return Err(invalid(
                "foreign_resource",
                "resource selector requires the same engine and Host",
            ));
        }
        let binding = module.engine.with_scope(|scope| {
            v8::tc_scope!(let scope, scope);
            let function = v8::Local::new(scope, &self.callback.function);
            let mut setups = Vec::new();
            if let Some(setup) = &self.callback.slot.setup {
                setups.push(setup.clone());
            }
            if let Some(setup) = &value.setup {
                setups.push(setup.clone());
            }
            let value = v8::Local::new(scope, &value.value);
            let receiver = v8::undefined(scope).into();
            let source = function.call(scope, receiver, &[value]).ok_or_else(|| {
                let caught = scope.exception();
                exception(scope, caught, "resource event selector threw")
            })?;
            helper(&module.engine, scope, "synchronous", &[source])?;
            let source = object(source)?;
            Ok(event_source(&module.engine, module, scope, source, setups))
        })?;
        let source = binding.source.clone().retain_owner(binding);
        Ok(source)
    }
}

fn deliver(
    owner: &Weak<Owner>,
    callback: Rc<Callback>,
    context: Context,
    event: JsValue,
) -> LocalBoxFuture<'static, Result<()>> {
    let result: Result<_> = (|| {
        let installed = owner.upgrade().ok_or_else(crate::slots::expired)?;
        let invocation = installed
            .invocation
            .clone()
            .ok_or_else(crate::slots::expired)?;
        let lease = invocation.retain(&context)?;
        let owner = Owner::child(&installed, Some(invocation));
        let cap = Capability::new(&owner.module, Some(&owner), Kind::Context(context))?;
        let context = owned_cap(&cap)?;
        Ok((owner, lease, context))
    })();
    async move {
        let (owner, lease, context) = result?;
        callback
            .module
            .engine
            .clone()
            .deliver_event(callback, context, event, owner, lease)
            .await?;
        Ok(())
    }
    .boxed_local()
}

pub(super) fn dispatch<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    operation: &str,
    args: v8::Local<'s, v8::Array>,
) -> Option<Result<v8::Local<'s, v8::Value>>> {
    if let (Kind::EventSink(delivery), "event_emit") = (&cap.kind, operation) {
        return Some((|| {
            let delivery = delivery.upgrade().ok_or_else(crate::slots::expired)?;
            let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
            let value = arg(scope, args, 0)?;
            let event = JsValue {
                value: v8::Global::new(scope, value),
                module,
                native: None,
                setup: cap.owner.upgrade().map(|owner| owner.ancestry()),
            };
            let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
            crate::scope::within(engine.id, &mut evaluator, || (delivery.listener)(event));
            Ok(v8::undefined(scope).into())
        })());
    }
    if !matches!(operation, "on" | "on_event" | "on_resource" | "task") {
        return None;
    }
    let Kind::Declaration {
        recorder, owner, ..
    } = &cap.kind
    else {
        return None;
    };
    Some((|| {
        let owner = owner.upgrade().ok_or_else(crate::slots::expired)?;
        let module = owner.module.clone();
        if operation == "task" {
            let callback = callback_for(&owner, arg(scope, args, 0)?, scope)?;
            recorder.task(move |task| {
                let callback = callback.clone();
                async move {
                    let owner =
                        Owner::with_setup(&callback.module, None, callback.slot.setup.clone());
                    let cap = Capability::new(&owner.module, Some(&owner), Kind::TaskScope(task))?;
                    let args = vec![owned_cap(&cap)?];
                    callback
                        .module
                        .engine
                        .clone()
                        .enqueue(
                            callback,
                            args,
                            Some(owner),
                            None,
                            Conversion::Portable(false),
                        )
                        .await?;
                    Ok(())
                }
                .boxed_local()
            })?;
            return Ok(v8::undefined(scope).into());
        }
        let name = string(arg(scope, args, 0)?, scope)?;
        let source = arg(scope, args, 1)?;
        let options = json_value(
            arg(scope, args, if operation == "on_resource" { 4 } else { 3 })?,
            scope,
        )?;
        let options = options
            .as_object()
            .ok_or_else(|| invalid("listener_options", "listener options must be an object"))?;
        let options = match options.get("maxPending") {
            None => ListenerOptions::default(),
            Some(value) => ListenerOptions::new(
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| {
                        invalid("listener_options", "maxPending must be a positive integer")
                    })?,
            )?,
        };
        if operation == "on" {
            let source = capability(engine, source, scope)?;
            super::declaration::same_host(&source, &module)?;
            let source = readable(&source.kind)?;
            let callback = callback_for(&owner, arg(scope, args, 2)?, scope)?;
            let weak = Rc::downgrade(&owner);
            recorder.on_with_options(
                name,
                source,
                move |snapshot, context| {
                    let event = JsValue::json(&callback.module, super::snapshot(snapshot));
                    let callback = callback.clone();
                    let weak = weak.clone();
                    async move { deliver(&weak, callback, context, event?).await }.boxed_local()
                },
                options,
            )?;
        } else if operation == "on_event" {
            let source = event_source(
                engine,
                &module,
                scope,
                object(source)?,
                vec![owner.ancestry()],
            );
            let callback = callback_for(&owner, arg(scope, args, 2)?, scope)?;
            let weak = Rc::downgrade(&owner);
            recorder.on_event_with_options(
                name,
                source.source.clone().retain_owner(source.clone()),
                move |event, context| deliver(&weak, callback.clone(), context, event),
                options,
            )?;
        } else {
            let resource = capability(engine, source, scope)?;
            super::declaration::same_host(&resource, &module)?;
            let Kind::Resource(resource) = &resource.kind else {
                return Err(invalid(
                    "resource",
                    "resource event source requires an issued resource",
                ));
            };
            let function = arg(scope, args, 2)?;
            let select = selector(engine, &module, scope, function, &owner)?;
            let callback = callback_for(&owner, arg(scope, args, 3)?, scope)?;
            let weak = Rc::downgrade(&owner);
            recorder.on_resource_with_options(
                name,
                resource.upgrade()?,
                select.source.clone(),
                move |event, context| deliver(&weak, callback.clone(), context, event),
                options,
            )?;
            owner.selectors.borrow_mut().push(select);
        }
        Ok(v8::undefined(scope).into())
    })())
}
