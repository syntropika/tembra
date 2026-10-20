use super::*;
fn status(status: RefStatus) -> &'static str {
    match status {
        RefStatus::Loading => "loading",
        RefStatus::Live => "live",
        RefStatus::Released => "released",
        RefStatus::Gone => "gone",
    }
}
fn shape(shape: Shape) -> Value {
    json!({"variant":shape.variant,"members":shape.members,"contract":shape.contract})
}
fn owner(cap: &Rc<Capability>) -> Option<Rc<Owner>> {
    cap.owner.upgrade()
}
fn notify(module: &Weak<Module>, slot: &Weak<FunctionSlot>, args: Vec<Value>) -> Result<()> {
    let module = module.upgrade().ok_or_else(crate::slots::expired)?;
    let slot = slot.upgrade().ok_or_else(crate::slots::expired)?;
    let callback = Callback {
        function: slot.function.clone(),
        slot,
        module,
    };
    callback
        .module
        .engine
        .direct(&callback, args, scope::CallKind::Void)?;
    Ok(())
}
pub(super) fn dispatch<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    operation: &str,
    args: v8::Local<'s, v8::Array>,
) -> Option<Result<v8::Local<'s, v8::Value>>> {
    let handled = matches!(
        operation,
        "opaque"
            | "snapshot"
            | "replace"
            | "change"
            | "subscribe"
            | "resource_value"
            | "status"
            | "shape"
            | "error"
            | "release"
            | "readable"
            | "on_status"
            | "on_shape"
            | "unsubscribe"
            | "aborted"
            | "reason"
            | "check"
            | "cancel"
            | "task_signal"
            | "scope_signal"
            | "context_signal"
            | "registry"
            | "table"
            | "registry_subscribe"
    );
    if !handled {
        return None;
    }
    Some((|| {
        let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
        match (operation, &cap.kind) {
            ("snapshot", kind) => {
                let source = readable(kind)?;
                let context = arg(scope, args, 0)?;
                let value = if context.is_undefined() {
                    source.snapshot()?
                } else {
                    let context = capability(engine, context, scope)?.context()?;
                    source.snapshot_in(&context)?
                };
                to_js(scope, snapshot(value))
            }
            ("replace", Kind::Cell(cell)) => {
                let ctx = capability(engine, arg(scope, args, 0)?, scope)?.context()?;
                let value = json_value(arg(scope, args, 1)?, scope)?;
                let options = json_value(arg(scope, args, 2)?, scope)?;
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let snapshot = crate::scope::within(engine.id, &mut evaluator, || {
                    cell.replace(
                        &ctx,
                        value,
                        ReplaceOptions {
                            diff: options["diff"].as_bool().unwrap_or(false),
                        },
                    )
                })?;
                to_js(scope, super::snapshot(snapshot))
            }
            ("change", Kind::Cell(cell)) => {
                let ctx = capability(engine, arg(scope, args, 0)?, scope)?.context()?;
                let callback = callback(&module, arg(scope, args, 1)?, scope)?;
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let snapshot = crate::scope::within(engine.id, &mut evaluator, || {
                    cell.change(&ctx, |draft| {
                        let output = callback.module.engine.direct(
                            &callback,
                            vec![json!({"value":draft})],
                            scope::CallKind::Draft,
                        )?;
                        *draft = output
                            .get("value")
                            .cloned()
                            .ok_or_else(|| invalid("draft", "draft.value was removed"))?;
                        Ok(())
                    })
                })?;
                to_js(scope, super::snapshot(snapshot))
            }
            ("subscribe", kind) => {
                let source = readable(kind)?;
                let callback = callback(&module, arg(scope, args, 0)?, scope)?;
                let weak_module = Rc::downgrade(&module);
                let weak_slot = Rc::downgrade(&callback.slot);
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let subscription = crate::scope::within(engine.id, &mut evaluator, || {
                    source.subscribe(move |snapshot| {
                        notify(&weak_module, &weak_slot, vec![snapshot.value().clone()])
                    })
                })?;
                let sub = Capability::new(
                    &module,
                    owner(cap).as_ref(),
                    Kind::StateSubscription(
                        RefCell::new(Some(subscription)),
                        RefCell::new(Some(callback.slot.clone())),
                    ),
                )?;
                wrap(engine, scope, &sub)
            }
            ("resource_value", Kind::Resource(resource)) => {
                let value = resource.upgrade()?.value()?;
                if value.module.host != module.host {
                    return Err(invalid("foreign_host", "resource belongs to another Host"));
                }
                if value.module.engine.id != engine.id {
                    return Err(invalid(
                        "foreign_resource",
                        "resource belongs to another engine",
                    ));
                }
                Ok(v8::Local::new(scope, &value.value))
            }
            ("opaque", Kind::Reference(reference)) => {
                let member = string(arg(scope, args, 0)?, scope)?;
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let variant = crate::scope::within(engine.id, &mut evaluator, || reference.shape())
                    .and_then(|shape| shape.variant)
                    .unwrap_or_default();
                let id = match reference
                    .contract()
                    .members(&variant)
                    .and_then(|members| members.get(&member))
                {
                    Some(Member::Opaque { id }) => id.clone(),
                    _ => return Err(invalid("not_in_variant", "opaque member is absent")),
                };
                let adapter = engine.opaques.borrow().get(&id).cloned().ok_or_else(|| {
                    invalid(
                        "native_signature",
                        "opaque nominal id has no registered adapter",
                    )
                })?;
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let value = crate::scope::within(engine.id, &mut evaluator, || {
                    adapter.read(reference, &member)
                })?;
                if let crate::native::NativeData::Capability(value) = value.inner {
                    let cap = Capability::new(&module, owner(cap).as_ref(), Kind::Native(value))?;
                    wrap(engine, scope, &cap)
                } else {
                    Err(invalid(
                        "native_capability",
                        "opaque adapter did not issue a capability",
                    ))
                }
            }
            ("status", Kind::Reference(reference)) => to_js(scope, status(reference.status())),
            ("shape", Kind::Reference(reference)) => {
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let value = crate::scope::within(engine.id, &mut evaluator, || reference.shape());
                match value {
                    Some(value) => to_js(scope, shape(value)),
                    None => Ok(v8::undefined(scope).into()),
                }
            }
            ("error", Kind::Reference(reference)) => match reference.error() {
                Some(error) => to_js(scope, error),
                None => Ok(v8::undefined(scope).into()),
            },
            ("release", Kind::Reference(reference)) => {
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                crate::scope::within(engine.id, &mut evaluator, || reference.release());
                Ok(v8::undefined(scope).into())
            }
            ("readable", Kind::Reference(reference)) => {
                let member = string(arg(scope, args, 0)?, scope)?;
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let source = crate::scope::within(engine.id, &mut evaluator, || {
                    reference.readable(&member)
                })?;
                let sub = Capability::new(&module, owner(cap).as_ref(), Kind::Readable(source))?;
                wrap(engine, scope, &sub)
            }
            ("on_status" | "on_shape", Kind::Reference(reference)) => {
                let callback = callback(&module, arg(scope, args, 0)?, scope)?;
                let weak_module = Rc::downgrade(&module);
                let weak_slot = Rc::downgrade(&callback.slot);
                let unsubscribe = if operation == "on_status" {
                    reference.on_status(move |value, error| {
                        if let Err(error) = notify(
                            &weak_module,
                            &weak_slot,
                            vec![
                                json!(status(value)),
                                serde_json::to_value(error).unwrap_or(Value::Null),
                            ],
                        ) && let Some(module) = weak_module.upgrade()
                        {
                            module.engine.report(error);
                        }
                    })
                } else {
                    let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                    crate::scope::within(engine.id, &mut evaluator, || {
                        reference.on_shape(move |value| {
                            if let Err(error) = notify(&weak_module, &weak_slot, vec![shape(value)])
                                && let Some(module) = weak_module.upgrade()
                            {
                                module.engine.report(error);
                            }
                        })
                    })
                };
                let retained = callback.slot.clone();
                let unsubscribe = Box::new(move || {
                    let _keep = &retained;
                    unsubscribe();
                });
                let sub = Capability::new(
                    &module,
                    owner(cap).as_ref(),
                    Kind::Unsubscribe(RefCell::new(Some(unsubscribe))),
                )?;
                wrap(engine, scope, &sub)
            }
            ("unsubscribe", Kind::Unsubscribe(callback)) => {
                let callback = callback.borrow_mut().take();
                if let Some(callback) = callback {
                    callback();
                }
                cap.retire();
                Ok(v8::undefined(scope).into())
            }
            ("unsubscribe", Kind::StateSubscription(sub, slot)) => {
                let slot = slot.borrow_mut().take();
                let sub = sub.borrow_mut().take();
                drop(sub);
                drop(slot);
                cap.retire();
                Ok(v8::undefined(scope).into())
            }
            ("unsubscribe", Kind::RegistrySubscription(sub, slot)) => {
                let slot = slot.borrow_mut().take();
                let sub = sub.borrow_mut().take();
                drop(sub);
                drop(slot);
                cap.retire();
                Ok(v8::undefined(scope).into())
            }
            ("aborted", Kind::Signal(signal)) => to_js(scope, signal.aborted()),
            ("reason", Kind::Signal(signal)) => to_js(scope, signal.reason()),
            ("scope_signal", Kind::Scope(facet)) => {
                let signal = Capability::new(
                    &module,
                    Some(&cap.owner()?),
                    Kind::Signal(Signal::Facet(facet.signal())),
                )?;
                wrap(engine, scope, &signal)
            }
            ("task_signal", Kind::TaskScope(task)) => {
                let signal = Capability::new(
                    &module,
                    owner(cap).as_ref(),
                    Kind::Signal(Signal::Facet(task.signal())),
                )?;
                wrap(engine, scope, &signal)
            }
            ("context_signal", Kind::Context(ctx)) => {
                let token = ctx.cancellation().child();
                let expiry = token.clone();
                ctx.on_settle(move || {
                    expiry.cancel(RuntimeError::aborted("expired", "context scope settled"))
                })?;
                let signal = Capability::new(
                    &module,
                    owner(cap).as_ref(),
                    Kind::Signal(Signal::Core(token)),
                )?;
                wrap(engine, scope, &signal)
            }
            ("check", Kind::Context(ctx)) => {
                ctx.check(module.host)?;
                Ok(v8::undefined(scope).into())
            }
            ("cancel", Kind::Context(ctx)) => {
                ctx.check(module.host)?;
                ctx.cancel(string(arg(scope, args, 0)?, scope)?);
                Ok(v8::undefined(scope).into())
            }
            ("registry", Kind::Scope(facet)) => {
                let definition = registry(json_value(arg(scope, args, 0)?, scope)?)?;
                let view = Capability::new(
                    &module,
                    Some(&cap.owner()?),
                    Kind::RegistryView(facet.registry(&definition)?),
                )?;
                wrap(engine, scope, &view)
            }
            ("table", Kind::RegistryView(view)) => {
                let context = arg(scope, args, 0)?;
                let mut table_owner = owner(cap);
                let context = if context.is_undefined() {
                    None
                } else {
                    let context = capability(engine, context, scope)?;
                    table_owner = context.owner.upgrade();
                    Some(context.context()?)
                };
                table(
                    engine,
                    scope,
                    &module,
                    table_owner.as_ref(),
                    view.table(context.as_ref())?,
                )
            }
            ("registry_subscribe", Kind::RegistryView(view)) => {
                let callback = callback(&module, arg(scope, args, 0)?, scope)?;
                let weak_module = Rc::downgrade(&module);
                let weak_slot = Rc::downgrade(&callback.slot);
                let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
                let subscription = crate::scope::within(engine.id, &mut evaluator, || {
                    view.subscribe(move |snapshot| {
                        let module = weak_module.upgrade().ok_or_else(crate::slots::expired)?;
                        let slot = weak_slot.upgrade().ok_or_else(crate::slots::expired)?;
                        module.engine.with_scope(|scope| {
                            let table = table(&module.engine, scope, &module, None, snapshot)?;
                            let function = v8::Local::new(scope, &slot.function);
                            let receiver = v8::undefined(scope).into();
                            let value =
                                function.call(scope, receiver, &[table]).ok_or_else(|| {
                                    invalid("registry_callback", "registry observer threw")
                                })?;
                            helper(&module.engine, scope, "synchronous", &[value])?;
                            Ok(())
                        })
                    })
                })?;
                let sub = Capability::new(
                    &module,
                    owner(cap).as_ref(),
                    Kind::RegistrySubscription(
                        RefCell::new(Some(subscription)),
                        RefCell::new(Some(callback.slot.clone())),
                    ),
                )?;
                wrap(engine, scope, &sub)
            }
            _ => Err(invalid(
                "capability_operation",
                "operation is unavailable on this capability",
            )),
        }
    })())
}
pub(super) fn table<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    module: &Rc<Module>,
    owner: Option<&Rc<Owner>>,
    snapshot: RegistrySnapshot,
) -> Result<v8::Local<'s, v8::Value>> {
    let entries = v8::Array::new(scope, snapshot.len() as i32);
    for (index, (key, entry)) in snapshot.entries().enumerate() {
        let pair = v8::Array::new(scope, 2);
        let key = to_js(scope, key)?;
        pair.set_index(scope, 0, key);
        let value = v8::Object::new(scope);
        let data = to_js(scope, entry.data())?;
        let name = v8::String::new(scope, "data").unwrap();
        value.set(scope, name.into(), data);
        let operation = if let Some(operation) = entry.operation() {
            let operation = Capability::new(module, owner, Kind::Operation(operation.clone()))?;
            wrap(engine, scope, &operation)?
        } else {
            v8::undefined(scope).into()
        };
        let name = v8::String::new(scope, "operation").unwrap();
        value.set(scope, name.into(), operation);
        pair.set_index(scope, 1, value.into());
        entries.set_index(scope, index as u32, pair.into());
    }
    let result = v8::Object::new(scope);
    let key = v8::String::new(scope, "entries").unwrap();
    result.set(scope, key.into(), entries.into());
    let key = v8::String::new(scope, "codeVersion").unwrap();
    let version = to_js(scope, snapshot.code_version())?;
    result.set(scope, key.into(), version);
    let value = helper(engine, scope, "table", &[result.into()])?;
    Ok(value)
}
