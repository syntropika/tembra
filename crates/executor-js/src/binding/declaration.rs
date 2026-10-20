use super::*;

pub(super) fn dispatch<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    operation: &str,
    args: v8::Local<'s, v8::Array>,
) -> Option<Result<v8::Local<'s, v8::Value>>> {
    match (&cap.kind, operation) {
        (Kind::Setup(recorder), "serve") => Some((|| {
            let token = contract(json_value(arg(scope, args, 0)?, scope)?)?;
            let factory = callback_for(&cap.owner()?, arg(scope, args, 1)?, scope)?;
            let options = json_value(arg(scope, args, 2)?, scope)?;
            let mut serve = ServeOptions::default();
            if options
                .as_object()
                .is_none_or(|flags| flags.keys().any(|key| key != "lifetime" && key != "linger"))
            {
                return Err(invalid("serve_options", "unknown or invalid serve options"));
            }
            let adapters = arg(scope, args, 3)?;
            serve.accepts =
                super::adapters::declarations(engine, scope, &cap.owner()?, &token, adapters)?;
            if let Some(lifetime) = options.get("lifetime") {
                serve.lifetime = match lifetime.as_str() {
                    Some("host") => Lifetime::Host,
                    Some("ref") => Lifetime::Referenced,
                    _ => return Err(invalid("lifetime", "invalid lifetime")),
                };
            }
            if let Some(linger) = options.get("linger") {
                let millis = linger
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or_else(|| invalid("linger", "linger must be nonnegative milliseconds"))?;
                serve.linger = std::time::Duration::try_from_secs_f64(millis / 1000.0)
                    .map_err(|e| invalid("linger", e.to_string()))?;
            }
            let provider = token.clone();
            recorder.serve(
                token,
                Rc::new(move |input, principal, declaration| {
                    factory_implementation(&factory, &provider, input, principal, declaration)
                }),
                serve,
            )?;
            Ok(v8::undefined(scope).into())
        })()),
        (
            Kind::Declaration {
                recorder, owner, ..
            },
            _,
        ) if matches!(
            operation,
            "cell" | "get" | "resource" | "derive" | "register" | "register_method"
        ) =>
        {
            Some((|| {
                let owner = owner.upgrade().ok_or_else(crate::slots::expired)?;
                let module = &owner.module;
                let kind = match operation {
                    "cell" => {
                        let name = string(arg(scope, args, 0)?, scope)?;
                        let schema = json_value(arg(scope, args, 1)?, scope)?;
                        let version = json_value(arg(scope, args, 2)?, scope)?
                            .as_u64()
                            .ok_or_else(|| invalid("state_version", "invalid version"))?;
                        let immediate = arg(scope, args, 3)?.is_true();
                        let mut declaration = if immediate {
                            StateDeclaration::new(schema, json_value(arg(scope, args, 4)?, scope)?)
                        } else {
                            let initialize = callback_for(&owner, arg(scope, args, 5)?, scope)?;
                            let weak = Rc::downgrade(&owner);
                            StateDeclaration::with_initializer(schema, move |ctx| {
                                let result: Result<_> = (|| {
                                    let owner = weak.upgrade().ok_or_else(crate::slots::expired)?;
                                    let owner = Owner::child(&owner, None);
                                    let context = Capability::new(
                                        &owner.module,
                                        Some(&owner),
                                        Kind::Context(ctx),
                                    )?;
                                    let args = vec![owned_cap(&context)?];
                                    Ok((owner, args))
                                })();
                                let initialize = initialize.clone();
                                async move {
                                    let (owner, args) = result?;
                                    match initialize
                                        .module
                                        .engine
                                        .clone()
                                        .enqueue(
                                            initialize,
                                            args,
                                            Some(owner),
                                            None,
                                            Conversion::Portable(true),
                                        )
                                        .await?
                                    {
                                        Output::Portable(Some(value)) => Ok(value),
                                        _ => Err(invalid(
                                            "initializer",
                                            "initializer produced no value",
                                        )),
                                    }
                                }
                                .boxed_local()
                            })
                        }
                        .version(version);
                        let migrate = arg(scope, args, 6)?;
                        if !migrate.is_undefined() {
                            let migrate = callback_for(&owner, migrate, scope)?;
                            declaration = declaration.with_migrate(move |value, previous| {
                                migrate.module.engine.direct(
                                    &migrate,
                                    vec![value, json!(previous)],
                                    scope::CallKind::Value,
                                )
                            });
                        }
                        Kind::Cell(recorder.state(name, declaration)?)
                    }
                    "get" => {
                        let contract = contract(json_value(arg(scope, args, 0)?, scope)?)?;
                        let input = json_value(arg(scope, args, 1)?, scope)?;
                        let options = acquisition(json_value(arg(scope, args, 2)?, scope)?)?;
                        let mut evaluator =
                            |action: &mut dyn crate::scope::Action| action.run(scope);
                        Kind::Reference(crate::scope::within(engine.id, &mut evaluator, || {
                            recorder.get(contract, input, options)
                        })?)
                    }
                    "derive" => {
                        let sources = object(arg(scope, args, 0)?)?;
                        let sources = v8::Local::<v8::Array>::try_from(sources)
                            .map_err(|_| invalid("sources", "sources must be array"))?;
                        let mut readables = Vec::new();
                        for index in 0..sources.length() {
                            let source = capability(engine, arg(scope, sources, index)?, scope)?;
                            same_host(&source, module)?;
                            readables.push(readable(&source.kind)?);
                        }
                        let schema = json_value(arg(scope, args, 1)?, scope)?;
                        let compute = callback_for(&owner, arg(scope, args, 2)?, scope)?;
                        let mut evaluator =
                            |action: &mut dyn crate::scope::Action| action.run(scope);
                        Kind::Readable(crate::scope::within(engine.id, &mut evaluator, || {
                            recorder.derive(readables, schema, move |snapshots| {
                                compute.module.engine.direct(
                                    &compute,
                                    vec![Value::Array(
                                        snapshots.iter().cloned().map(snapshot).collect(),
                                    )],
                                    scope::CallKind::Value,
                                )
                            })
                        })?)
                    }
                    "resource" => {
                        let name = string(arg(scope, args, 0)?, scope)?;
                        let version = json_value(arg(scope, args, 1)?, scope)?
                            .as_u64()
                            .ok_or_else(|| invalid("resource_version", "invalid version"))?;
                        let exclusive = arg(scope, args, 2)?.is_true();
                        let deps = v8::Local::<v8::Array>::try_from(arg(scope, args, 3)?)
                            .map_err(|_| invalid("resource_dependency", "deps must be array"))?;
                        let mut dependencies = Vec::new();
                        for index in 0..deps.length() {
                            let dep = capability(engine, arg(scope, deps, index)?, scope)?;
                            same_host(&dep, module)?;
                            dependencies.push(match &dep.kind {
                                Kind::Reference(reference) => reference.dependency(),
                                Kind::Resource(resource) => resource.upgrade()?.dependency(),
                                _ => {
                                    return Err(invalid(
                                        "resource_dependency",
                                        "invalid dependency",
                                    ));
                                }
                            });
                        }
                        let open = callback_for(&owner, arg(scope, args, 4)?, scope)?;
                        let close = arg(scope, args, 5)?;
                        let close = if close.is_undefined() {
                            None
                        } else {
                            Some(callback_for(&owner, close, scope)?)
                        };
                        let weak = Rc::downgrade(&owner);
                        let adopting_host = module.host;
                        let adopting_engine = engine.id;
                        let mut declaration = ResourceDeclaration::new(move |signal| {
                            let result: Result<_> = (|| {
                                let owner = weak.upgrade().ok_or_else(crate::slots::expired)?;
                                let signal = Capability::new(
                                    &owner.module,
                                    Some(&owner),
                                    Kind::Signal(Signal::Core(signal)),
                                )?;
                                Ok((owner, vec![owned_cap(&signal)?]))
                            })();
                            let open = open.clone();
                            async move {
                                let (owner, args) = result?;
                                match open
                                    .module
                                    .engine
                                    .clone()
                                    .enqueue(open, args, Some(owner), None, Conversion::Owned)
                                    .await?
                                {
                                    Output::Owned(value) => Ok(value),
                                    _ => Err(invalid("resource", "missing resource value")),
                                }
                            }
                            .boxed_local()
                        })
                        .with_adoption_check(move |value: &JsValue| {
                            if value.module.host != adopting_host
                                || value.module.engine.id != adopting_engine
                            {
                                return Err(invalid(
                                    "foreign_resource",
                                    concat!(
                                        "kept resource requires the same engine and Host; ",
                                        "restart it explicitly"
                                    ),
                                ));
                            }
                            Ok(())
                        });
                        if let Some(close) = close {
                            declaration = declaration.with_close(move |value| {
                                let close = close.clone();
                                async move {
                                    close
                                        .module
                                        .engine
                                        .clone()
                                        .close_resource(close, value)
                                        .await?;
                                    Ok(())
                                }
                                .boxed_local()
                            });
                        }
                        declaration.version = version;
                        declaration.exclusive = exclusive;
                        declaration.deps = dependencies;
                        Kind::Resource(recorder.resource(name, declaration)?.downgrade())
                    }
                    "register" | "register_method" => {
                        let registry = registry(json_value(arg(scope, args, 0)?, scope)?)?;
                        let key = string(arg(scope, args, 1)?, scope)?;
                        let data = json_value(arg(scope, args, 2)?, scope)?;
                        if operation == "register" {
                            recorder.register(&registry, key, data)?;
                        } else {
                            recorder.register_method(
                                &registry,
                                key,
                                data,
                                string(arg(scope, args, 3)?, scope)?,
                            )?;
                        }
                        return Ok(v8::undefined(scope).into());
                    }
                    _ => return Err(invalid("declaration", "unsupported operation")),
                };
                let capability = Capability::new(module, Some(&owner), kind)?;
                wrap(engine, scope, &capability)
            })())
        }
        _ => None,
    }
}

fn factory_implementation(
    factory: &Rc<Callback>,
    token: &Rc<Contract>,
    input: Value,
    principal: Value,
    declaration: &mut Declaration,
) -> Result<Implementation> {
    let module = &factory.module;
    let owner = Owner::with_setup(
        module,
        Some(declaration.invocation_owner()),
        factory.slot.setup.clone(),
    );
    let cap = Capability::new(
        module,
        Some(&owner),
        Kind::Declaration {
            recorder: declaration.recorder(),
            owner: Rc::downgrade(&owner),
            principal,
        },
    )?;
    let mut runtime = module
        .engine
        .runtime
        .try_borrow_mut()
        .map_err(|_| invalid("engine_busy", "factory requires idle engine"))?;
    deno_core::scope!(scope, runtime);
    v8::tc_scope!(let scope,scope);
    let input = to_js(scope, input)?;
    let declaration = wrap(&module.engine, scope, &cap)?;
    let function = v8::Local::new(scope, &factory.function);
    let receiver = v8::undefined(scope).into();
    let implementation = function
        .call(scope, receiver, &[input, declaration])
        .ok_or_else(|| {
            scope
                .exception()
                .map(|exception| {
                    crate::engine::promise_error(deno_core::error::JsError::from_v8_exception(
                        scope, exception,
                    ))
                })
                .unwrap_or_else(|| invalid("implementation", "factory threw"))
        })?;
    super::implementation::parse(scope, module, &owner, token, implementation, None)
}

pub(super) fn same_host(cap: &Capability, module: &Module) -> Result<()> {
    if cap.module.upgrade().ok_or_else(crate::slots::expired)?.host != module.host {
        return Err(invalid(
            "foreign_host",
            "declaration capability belongs to another Host",
        ));
    }
    Ok(())
}
