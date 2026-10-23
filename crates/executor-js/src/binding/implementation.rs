//! Checked implementation extraction shared by factory and legacy view adaptation.
use super::declaration::same_host;
use super::*;

pub(super) struct SelectedImplementation {
    pub projection: v8::Global<v8::Object>,
    pub functions: std::collections::BTreeMap<String, v8::Global<v8::Function>>,
    // Drop every isolate-owned Global before this final module/engine owner.
    pub owner: Rc<Owner>,
}

pub(super) struct Aliases {
    pub source: Rc<Implementation>,
    pub anchor: Rc<SelectedImplementation>,
}

pub(super) fn parse<'s>(
    scope: &mut Scope<'s, '_>,
    module: &Rc<Module>,
    owner: &Rc<Owner>,
    token: &Contract,
    implementation: v8::Local<'s, v8::Value>,
    aliases: Option<&Aliases>,
) -> Result<Implementation> {
    helper(&module.engine, scope, "synchronous", &[implementation])?;
    let object = object(implementation)?;
    let variant = get(scope, object, "variant")?;
    let variant = if variant.is_undefined() {
        None
    } else {
        Some(string(variant, scope)?)
    };
    let members = token
        .members(variant.as_deref().unwrap_or(""))
        .ok_or_else(|| invalid("variant", "unknown implementation variant"))?;
    let methods = object_or_empty(get(scope, object, "methods")?, scope)?;
    let states = object_or_empty(get(scope, object, "state")?, scope)?;
    let projection = v8::Object::new(scope);
    let projected_methods = v8::Object::new(scope);
    let projected_states = v8::Object::new(scope);
    let projected_opaques = v8::Object::new(scope);
    let mut functions = std::collections::BTreeMap::new();
    let selected_variant = variant.clone();
    let mut candidate = Implementation::new().retain_owner(owner.clone());
    if let Some(aliases) = aliases {
        candidate = candidate.retain_owner(aliases.source.clone());
    }
    if let Some(variant) = variant {
        candidate = candidate.variant(variant);
    }
    for (name, member) in members {
        match member {
            Member::State { .. } => {
                let value = get(scope, states, name)?;
                put(scope, projected_states, name, value)?;
                let cap = wrapped_capability(&module.engine, value, scope)?;
                same_host(&cap, module)?;
                candidate = candidate.state(name, readable(&cap.kind)?);
            }
            Member::Method { args, result } => {
                let value = get(scope, methods, name)?;
                let function = function(value)?;
                let projected = helper(&module.engine, scope, "projectMethod", &[function.into()])?;
                let projected = super::function(projected)?;
                put(scope, projected_methods, name, projected.into())?;
                functions.insert(name.clone(), v8::Global::new(scope, projected));
                if let Some(aliases) = aliases
                    && let Some(target) =
                        aliases
                            .anchor
                            .functions
                            .iter()
                            .find_map(|(name, original)| {
                                let original = v8::Local::new(scope, original);
                                original.strict_equals(function.into()).then_some(name)
                            })
                {
                    candidate = candidate.project_method(name, aliases.source.clone(), target)?;
                    continue;
                }
                let handler = callback_for(owner, value, scope)?;
                if matches!(args, Some(ValueSchema::Native(_)))
                    || matches!(result, Some(ValueSchema::Native(_)))
                {
                    let signature =
                        crate::native::NativeMethodSignature::from_member(args, result)?;
                    let adapter = module
                        .engine
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
                    candidate = adapter.install(candidate, name, handler, owner.clone())?;
                } else {
                    let weak = Rc::downgrade(owner);
                    let expects_args = args.is_some();
                    let expects_result = result.is_some();
                    candidate = candidate.json_method(name, move |args, context| {
                        let mut observation = context.observe(
                            tembra_core::observability::Operation::Executor,
                            "portable_callback",
                        );
                        let admitted: Result<_> = (|| {
                            let owner = weak.upgrade().ok_or_else(crate::slots::expired)?;
                            let lease = owner
                                .invocation
                                .as_ref()
                                .ok_or_else(|| {
                                    invalid("invocation_owner", "missing invocation owner")
                                })?
                                .retain(&context)?;
                            let owner = Owner::child(&owner, owner.invocation.clone());
                            let context = Capability::new(
                                &owner.module,
                                Some(&owner),
                                Kind::Context(context),
                            )?;
                            let mut values = vec![owned_cap(&context)?];
                            if expects_args {
                                values.push(JsValue::json(
                                    &owner.module,
                                    args.ok_or_else(|| invalid("arguments", "missing arguments"))?,
                                )?);
                            }
                            Ok((owner, lease, values))
                        })();
                        if let (Some(observation), Err(error)) = (&mut observation, &admitted) {
                            observation.result::<()>(&Err(error.clone()));
                        }
                        let handler = handler.clone();
                        async move {
                            let (owner, lease, args) = admitted?;
                            match handler
                                .module
                                .engine
                                .clone()
                                .enqueue(
                                    handler,
                                    (args, observation),
                                    Some(owner),
                                    Some(lease),
                                    Conversion::Portable(expects_result),
                                )
                                .await?
                            {
                                Output::Portable(value) => Ok(value),
                                _ => Err(invalid("method_result", "invalid method completion")),
                            }
                        }
                        .boxed_local()
                    });
                }
            }
            Member::Opaque { id } => {
                let opaques = object_or_empty(get(scope, object, "opaque")?, scope)?;
                let value = get(scope, opaques, name)?;
                put(scope, projected_opaques, name, value)?;
                let cap = wrapped_capability(&module.engine, value, scope)?;
                same_host(&cap, module)?;
                let resource = if let Kind::Resource(resource) = &cap.kind {
                    resource.upgrade()?
                } else {
                    return Err(invalid(
                        "native_capability",
                        "opaque implementation must expose a declared native-backed Resource",
                    ));
                };
                let adapter = module
                    .engine
                    .opaques
                    .borrow()
                    .get(id)
                    .cloned()
                    .ok_or_else(|| {
                        invalid(
                            "native_signature",
                            "opaque nominal id has no registered adapter",
                        )
                    })?;
                candidate = adapter.install(candidate, name, resource)?;
            }
        }
    }
    put(scope, projection, "methods", projected_methods.into())?;
    put(scope, projection, "state", projected_states.into())?;
    put(scope, projection, "opaque", projected_opaques.into())?;
    if let Some(variant) = selected_variant {
        let variant = to_js(scope, variant)?;
        put(scope, projection, "variant", variant)?;
    }
    let projection = helper(
        &module.engine,
        scope,
        "freezeProjection",
        &[projection.into()],
    )?;
    let anchor = Rc::new(SelectedImplementation {
        owner: owner.clone(),
        projection: v8::Global::new(scope, object_value(projection)?),
        functions,
    });
    Ok(candidate.retain_owner(anchor))
}
fn object_or_empty<'s>(
    value: v8::Local<'s, v8::Value>,
    scope: &mut Scope<'s, '_>,
) -> Result<v8::Local<'s, v8::Object>> {
    if value.is_undefined() {
        Ok(v8::Object::new(scope))
    } else {
        object(value)
    }
}

fn object_value(value: v8::Local<v8::Value>) -> Result<v8::Local<v8::Object>> {
    object(value)
}
fn put<'s>(
    scope: &mut Scope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) -> Result<()> {
    let name = v8::String::new(scope, name).ok_or_else(|| limit("string"))?;
    if object.create_data_property(scope, name.into(), value) != Some(true) {
        return Err(invalid(
            "implementation",
            "cannot create immutable view field",
        ));
    }
    Ok(())
}
