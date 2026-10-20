use super::*;
use crate::native::{NativeData, NativeRecord, NativeValue};

pub(crate) fn encode<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    module: &Rc<Module>,
    owner: &Rc<Owner>,
    value: NativeValue,
    depth: usize,
) -> Result<v8::Local<'s, v8::Value>> {
    if depth > 128 {
        return Err(invalid("native_record", "native record depth limit"));
    }
    match value.inner {
        NativeData::Absent => Ok(v8::undefined(scope).into()),
        NativeData::Portable(value) => {
            let value = to_js(scope, value)?;
            helper(engine, scope, "portable", &[value])
        }
        NativeData::Capability(value) => wrap(
            engine,
            scope,
            &Capability::new(module, Some(owner), Kind::Native(value))?,
        ),
        NativeData::Callable(callback) => {
            if let Some(cap) = callback.issued_cap() {
                let cap = cap?;
                check_module(&cap, module)?;
                return wrap(engine, scope, &cap);
            }
            let context = owner.context()?;
            context.check(module.host)?;
            let invocation = owner.invocation.clone().ok_or_else(|| {
                invalid("invocation_context", "callable requires a managed receiver")
            })?;
            drop(invocation.retain(&context)?);
            wrap(
                engine,
                scope,
                &Capability::new(
                    module,
                    Some(owner),
                    Kind::Callable(CallableSlot {
                        callback,
                        context,
                        invocation,
                    }),
                )?,
            )
        }
        NativeData::Record(record) => {
            if depth >= 128 {
                return Err(invalid("native_record", "native record depth limit"));
            }
            let cap = Capability::new(module, Some(owner), Kind::Record(record.clone()))?;
            encode_record(engine, scope, &cap, &record, depth)
        }
    }
}
pub(super) fn encode_record<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    record: &NativeRecord,
    depth: usize,
) -> Result<v8::Local<'s, v8::Value>> {
    let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
    let owner = cap.owner()?;
    let raw = raw_cap(engine, scope, cap)?;
    let object = helper(engine, scope, "emptyRecord", &[])?;
    let object = super::object(object)?;
    for (key, value) in record.entries() {
        let key = v8::String::new(scope, key).ok_or_else(|| limit("string"))?;
        let value = encode(engine, scope, &module, &owner, value.clone(), depth + 1)?;
        if object.define_own_property(
            scope,
            key.into(),
            value,
            v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE,
        ) != Some(true)
        {
            return Err(invalid("native_record", "record field definition failed"));
        }
    }
    helper(engine, scope, "sealRecord", &[raw.into(), object.into()])
}
pub(crate) fn decode<'s>(
    engine: &Engine,
    module: &Module,
    scope: &mut Scope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<NativeValue> {
    let tree = helper(engine, scope, "nativeView", &[value])?;
    decode_tree(engine, module, scope, tree, 0)
}
fn decode_tree<'s>(
    engine: &Engine,
    module: &Module,
    scope: &mut Scope<'s, '_>,
    tree: v8::Local<'s, v8::Value>,
    depth: usize,
) -> Result<NativeValue> {
    if depth > 128 {
        return Err(invalid("native_record", "native record depth limit"));
    }
    let tree = v8::Local::<v8::Array>::try_from(tree)
        .map_err(|_| invalid("native_record", "invalid native view tree"))?;
    match string(arg(scope, tree, 0)?, scope)?.as_str() {
        "absent" => Ok(NativeValue::absent()),
        "portable" => Ok(NativeValue::portable(json_value(
            arg(scope, tree, 1)?,
            scope,
        )?)),
        "cap" => {
            let cap = capability(engine, arg(scope, tree, 1)?, scope)?;
            check_module(&cap, module)?;
            match &cap.kind {
                Kind::Native(value) => Ok(NativeValue {
                    inner: NativeData::Capability(value.clone()),
                }),
                Kind::Callable(value) => Ok(NativeValue::callable(value.callback.issued(&cap))),
                _ => Err(invalid("native_type", "capability is not a native view")),
            }
        }
        "record" => {
            let fields = v8::Local::<v8::Array>::try_from(arg(scope, tree, 1)?)
                .map_err(|_| invalid("native_record", "invalid fields"))?;
            let mut entries = Vec::new();
            for index in 0..fields.length() {
                let field = v8::Local::<v8::Array>::try_from(arg(scope, fields, index)?)
                    .map_err(|_| invalid("native_record", "invalid field"))?;
                let name = string(arg(scope, field, 0)?, scope)?;
                let child = arg(scope, field, 1)?;
                let value = decode_tree(engine, module, scope, child, depth + 1)?;
                entries.push((name, value));
            }
            Ok(NativeValue::record(NativeRecord::new(entries)?))
        }
        _ => Err(invalid("native_record", "invalid view tag")),
    }
}
