use super::*;
use futures::future::LocalBoxFuture;

pub(crate) enum NativeOutput {
    Json(Value),
    Value(JsValue),
    Cap(Rc<Capability>),
    Native(Rc<Owner>, crate::native::NativeValue),
    NativePayload(Rc<Owner>, crate::native::NativeValue),
    Unit,
}
enum RootAuthority {
    Facet(FacetScope),
    Task(TaskScope),
}
impl RootAuthority {
    fn retain(&self) -> Result<Option<InvocationLease>> {
        match self {
            Self::Facet(_) => Ok(None),
            Self::Task(scope) => scope.retain().map(Some),
        }
    }
    fn root<R: 'static>(
        self,
        body: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<R>> + 'static,
    ) -> LocalBoxFuture<'static, Result<R>> {
        match self {
            Self::Facet(scope) => scope.root(body),
            Self::Task(scope) => scope.root(body),
        }
    }
}
fn spawn<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    module: Rc<Module>,
    owner: Option<Rc<Owner>>,
    lease: Option<InvocationLease>,
    work: LocalBoxFuture<'static, Result<NativeOutput>>,
) -> Result<v8::Local<'s, v8::Value>> {
    engine.admit_physical()?;
    let resolver = v8::PromiseResolver::new(scope).ok_or_else(|| limit("promises"))?;
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);
    let engine = engine.clone();
    let driver = engine.driver.clone();
    driver.spawn(
        async move {
            let result = work.await;
            let request = crate::engine::Request::Resolve {
                resolver,
                module,
                result,
                owner,
                lease,
                completion: None,
            };
            if engine.resolve(request).is_err() {
                engine.update(|c| c.physical_jobs -= 1);
            }
        }
        .boxed_local(),
    );
    Ok(promise.into())
}
pub(super) fn dispatch<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    operation: &str,
    args: v8::Local<'s, v8::Array>,
) -> Result<v8::Local<'s, v8::Value>> {
    let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
    let owner = cap.owner.upgrade();
    let mut lease = None;
    let work: LocalBoxFuture<'static, Result<NativeOutput>> = match (operation, &cap.kind) {
        ("root", Kind::Scope(_) | Kind::TaskScope(_)) => {
            let facet = match &cap.kind {
                Kind::Scope(scope) => RootAuthority::Facet(scope.clone()),
                Kind::TaskScope(scope) => RootAuthority::Task(scope.clone()),
                _ => unreachable!("root kind was matched"),
            };
            let root_lease = facet.retain()?;
            let setup = cap.owner()?.ancestry();
            let callback = callback_for(&cap.owner()?, arg(scope, args, 0)?, scope)?;
            let module = module.clone();
            async move {
                facet
                    .root(move |context| {
                        let admitted: Result<_> = (|| {
                            let owner = Owner::child(&setup, None);
                            let context =
                                Capability::new(&module, Some(&owner), Kind::Context(context))?;
                            Ok((owner, vec![owned_cap(&context)?]))
                        })();
                        async move {
                            let (owner, args) = admitted?;
                            match callback
                                .module
                                .engine
                                .clone()
                                .enqueue(callback, args, Some(owner), root_lease, Conversion::Owned)
                                .await?
                            {
                                Output::Owned(value) => Ok(NativeOutput::Value(value)),
                                _ => Err(invalid("root", "invalid root result")),
                            }
                        }
                        .boxed_local()
                    })
                    .await
            }
            .boxed_local()
        }
        ("context_get", Kind::Context(context)) => {
            context.check(module.host)?;
            let contract = contract(json_value(arg(scope, args, 0)?, scope)?)?;
            let input = json_value(arg(scope, args, 1)?, scope)?;
            let options = acquisition(json_value(arg(scope, args, 2)?, scope)?)?;
            let context = context.clone();
            let owner = cap.owner()?;
            if let Some(invocation) = &owner.invocation {
                lease = Some(invocation.retain(&context)?);
            }
            async move {
                let reference = context.get(contract, input, options).await?;
                Ok(NativeOutput::Cap(Capability::new(
                    &owner.module,
                    Some(&owner),
                    Kind::Reference(reference),
                )?))
            }
            .boxed_local()
        }
        ("ready", Kind::Reference(reference)) => {
            let reference = reference.clone();
            async move {
                reference.ready().await?;
                Ok(NativeOutput::Unit)
            }
            .boxed_local()
        }
        ("cancelled", Kind::Signal(signal)) => {
            let signal = signal.clone();
            async move {
                signal.cancelled().await;
                Ok(NativeOutput::Unit)
            }
            .boxed_local()
        }
        ("call", Kind::Reference(reference)) => {
            let member = string(arg(scope, args, 0)?, scope)?;
            let present = arg(scope, args, 1)?.is_true();
            let input = arg(scope, args, 2)?;
            let context = arg(scope, args, 3)?;
            let mut boundary_owner = None;

            let context = if context.is_undefined() {
                None
            } else {
                let cap = capability(engine, context, scope)?;
                let context = cap.context()?;
                boundary_owner = Some(cap.owner()?);
                if let Some(invocation) = cap.owner()?.invocation.as_ref() {
                    lease = Some(invocation.retain(&context)?);
                }
                Some(context)
            };
            let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
            let variant = crate::scope::within(engine.id, &mut evaluator, || reference.shape())
                .and_then(|shape| shape.variant)
                .unwrap_or_default();
            let metadata = reference
                .contract()
                .members(&variant)
                .and_then(|members| members.get(&member))
                .cloned()
                .or_else(|| {
                    reference
                        .contract()
                        .variants()
                        .values()
                        .find_map(|members| members.get(&member).cloned())
                })
                .ok_or_else(|| invalid("not_in_variant", "method is absent from consumer"))?;
            if let Member::Method {
                args: arguments,
                result,
            } = &metadata
            {
                if matches!(arguments, Some(ValueSchema::Native(_)))
                    || matches!(result, Some(ValueSchema::Native(_)))
                {
                    if context.is_none() {
                        return Err(invalid(
                            "native_context",
                            "foreign native calls require an explicit Context",
                        ));
                    }
                    let signature =
                        crate::native::NativeMethodSignature::from_member(arguments, result)?;
                    let adapter = engine
                        .natives
                        .borrow()
                        .get(&signature)
                        .cloned()
                        .ok_or_else(|| {
                            invalid(
                                "native_signature",
                                "native method signature has no registered codec",
                            )
                        })?;
                    let input = if present {
                        crate::native::native_at(engine, &module, scope, input)?
                    } else {
                        crate::native::NativeValue::absent()
                    };
                    adapter.invoke(
                        reference.clone(),
                        member,
                        input,
                        context,
                        boundary_owner.ok_or_else(crate::slots::expired)?,
                    )?
                } else {
                    let input = if present {
                        let portable = helper(engine, scope, "portable", &[input])?;
                        Some(json_value(portable, scope)?)
                    } else {
                        None
                    };
                    let reference = reference.clone();
                    async move {
                        let value = reference
                            .call_json(&member, input, context.as_ref())
                            .await?;
                        Ok(match value {
                            Some(value) => NativeOutput::Json(value),
                            None => NativeOutput::Unit,
                        })
                    }
                    .boxed_local()
                }
            } else {
                return Err(invalid("method", "member is not a method"));
            }
        }
        ("operation", Kind::Operation(operation)) => {
            let context_cap = capability(engine, arg(scope, args, 0)?, scope)?;
            let context = context_cap.context()?;
            let context_owner = context_cap.owner()?;
            if let Some(invocation) = &context_owner.invocation {
                lease = Some(invocation.retain(&context)?);
            }
            let payload = object(arg(scope, args, 1)?)?;
            let present_value = get(scope, payload, "present")?;
            if !present_value.is_boolean() {
                return Err(invalid(
                    "presence",
                    "operation payload needs boolean present",
                ));
            }
            let present = present_value.is_true();
            let value = get(scope, payload, "value")?;
            let metadata = operation
                .contract()
                .members("")
                .and_then(|members| members.get(operation.method()))
                .ok_or_else(|| invalid("not_in_variant", "registered method absent"))?;
            if let Member::Method {
                args: arguments,
                result,
            } = metadata
            {
                if matches!(arguments, Some(ValueSchema::Native(_)))
                    || matches!(result, Some(ValueSchema::Native(_)))
                {
                    let signature =
                        crate::native::NativeMethodSignature::from_member(arguments, result)?;
                    let adapter = engine
                        .natives
                        .borrow()
                        .get(&signature)
                        .cloned()
                        .ok_or_else(|| {
                            invalid(
                                "native_signature",
                                "registered operation has no native adapter",
                            )
                        })?;
                    let input = if present {
                        crate::native::native_at(engine, &module, scope, value)?
                    } else {
                        crate::native::NativeValue::absent()
                    };
                    {
                        let work = adapter.invoke_operation(
                            operation.clone(),
                            input,
                            context,
                            context_owner,
                        )?;
                        async move {
                            match work.await? {
                                NativeOutput::Native(owner, value) => {
                                    Ok(NativeOutput::NativePayload(owner, value))
                                }
                                _ => Err(invalid(
                                    "operation_result",
                                    "invalid native operation output",
                                )),
                            }
                        }
                        .boxed_local()
                    }
                } else {
                    let input = if present {
                        let value = helper(engine, scope, "portable", &[value])?;
                        Some(json_value(value, scope)?)
                    } else {
                        None
                    };
                    let operation = operation.clone();
                    async move {
                        let result = operation.call_json(&context, input).await?;
                        Ok(NativeOutput::Json(match result {
                            Some(value) => json!({"present":true,"value":value}),
                            None => json!({"present":false}),
                        }))
                    }
                    .boxed_local()
                }
            } else {
                return Err(invalid("not_in_variant", "registered entry is not method"));
            }
        }
        _ => {
            return Err(invalid(
                "capability_operation",
                "operation unavailable on capability",
            ));
        }
    };
    spawn(engine, scope, module, owner, lease, work)
}
pub(crate) use super::projection::resolve;
