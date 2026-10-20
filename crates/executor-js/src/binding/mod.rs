pub(crate) mod activity;
mod adapters;
pub(crate) mod asynchronous;
mod callable;
mod declaration;
mod implementation;
pub(crate) mod native_view;
mod observations;
mod projection;

use crate::{
    engine::{Conversion, Engine, Output, invalid, limit},
    scope,
    slots::*,
};
use deno_core::{OpState, op2, v8};
use futures::FutureExt;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

type Scope<'s, 'i> = v8::PinScope<'s, 'i>;
pub(crate) fn get<'s>(
    scope: &mut Scope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    key: &str,
) -> Result<v8::Local<'s, v8::Value>> {
    v8::tc_scope!(let scope, scope);
    let key = v8::String::new(scope, key).ok_or_else(|| limit("string"))?;
    object.get(scope, key.into()).ok_or_else(|| {
        let caught = scope.exception();
        exception(scope, caught, "property getter threw")
    })
}

fn exception<'s>(
    scope: &mut Scope<'s, '_>,
    caught: Option<v8::Local<'s, v8::Value>>,
    fallback: &str,
) -> RuntimeError {
    caught.map_or_else(
        || invalid("javascript", fallback),
        |value| {
            crate::engine::promise_error(deno_core::error::JsError::from_v8_exception(scope, value))
        },
    )
}
fn function<'s>(value: v8::Local<'s, v8::Value>) -> Result<v8::Local<'s, v8::Function>> {
    v8::Local::try_from(value).map_err(|_| invalid("callback", "expected callable"))
}
fn object<'s>(value: v8::Local<'s, v8::Value>) -> Result<v8::Local<'s, v8::Object>> {
    v8::Local::try_from(value).map_err(|_| invalid("object", "expected object"))
}
fn string<'s>(value: v8::Local<'s, v8::Value>, scope: &mut Scope<'s, '_>) -> Result<String> {
    if !value.is_string() {
        return Err(invalid("string", "expected string"));
    }
    Ok(value.to_rust_string_lossy(scope))
}
pub(crate) fn json_value<'s>(
    value: v8::Local<'s, v8::Value>,
    scope: &mut Scope<'s, '_>,
) -> Result<Value> {
    deno_core::serde_v8::from_v8(scope, value).map_err(|e| invalid("portable", e.to_string()))
}
fn to_js<'s>(
    scope: &mut Scope<'s, '_>,
    value: impl serde::Serialize,
) -> Result<v8::Local<'s, v8::Value>> {
    deno_core::serde_v8::to_v8(scope, value).map_err(|e| invalid("portable", e.to_string()))
}
fn arg<'s>(
    scope: &mut Scope<'s, '_>,
    args: v8::Local<'s, v8::Array>,
    index: u32,
) -> Result<v8::Local<'s, v8::Value>> {
    args.get_index(scope, index)
        .ok_or_else(|| invalid("argument", "argument getter threw"))
}
fn contract(value: Value) -> Result<Rc<Contract>> {
    Contract::from_json(&value.to_string())
        .map(Rc::new)
        .map_err(|e| invalid(e.reason, e.message))
}
fn registry(value: Value) -> Result<Registry> {
    Registry::new(
        value["name"]
            .as_str()
            .ok_or_else(|| invalid("registry", "missing registry name"))?,
        match value["conflict"].as_str() {
            Some("error") => ConflictPolicy::Error,
            Some("last") => ConflictPolicy::Last,
            _ => return Err(invalid("registry", "invalid conflict policy")),
        },
    )
}
fn acquisition(value: Value) -> Result<GetOptions> {
    if let Some(state) = value.get("state")
        && !state.is_boolean()
    {
        return Err(invalid("state", "state option must be boolean"));
    }
    Ok(GetOptions {
        state: value["state"].as_bool().unwrap_or(true),
        ..Default::default()
    })
}
fn callback<'s>(
    module: &Rc<Module>,
    value: v8::Local<'s, v8::Value>,
    scope: &mut Scope<'s, '_>,
) -> Result<Rc<Callback>> {
    Callback::new(module, v8::Global::new(scope, function(value)?))
}

fn callback_for<'s>(
    owner: &Rc<Owner>,
    value: v8::Local<'s, v8::Value>,
    scope: &mut Scope<'s, '_>,
) -> Result<Rc<Callback>> {
    Callback::with_setup(
        &owner.module,
        v8::Global::new(scope, function(value)?),
        Some(owner.ancestry()),
    )
}

pub(crate) fn helper<'s>(
    engine: &Engine,
    scope: &mut Scope<'s, '_>,
    name: &str,
    args: &[v8::Local<'s, v8::Value>],
) -> Result<v8::Local<'s, v8::Value>> {
    v8::tc_scope!(let scope, scope);
    let bindings = v8::Local::new(scope, &engine.bindings);
    let function = function(get(scope, bindings, name)?)?;
    let receiver = v8::undefined(scope).into();
    function.call(scope, receiver, args).ok_or_else(|| {
        let caught = scope.exception();
        exception(scope, caught, "binding helper threw")
    })
}
pub(crate) fn portable_result(engine: &Engine, value: v8::Global<v8::Value>) -> Result<Value> {
    let mut runtime = engine.runtime.borrow_mut();
    deno_core::scope!(scope, runtime);
    v8::tc_scope!(let scope,scope);
    let value = v8::Local::new(scope, value);
    let checked = helper(engine, scope, "portable", &[value])?;
    json_value(checked, scope)
}
pub(crate) fn capability<'s>(
    engine: &Engine,
    value: v8::Local<'s, v8::Value>,
    scope: &mut Scope<'s, '_>,
) -> Result<Rc<Capability>> {
    let value = object(value)?;
    let brand = v8::Local::new(scope, &engine.brand);
    let id = value
        .get_private(scope, brand)
        .and_then(|value| value.number_value(scope))
        .filter(|id| id.is_finite() && id.fract() == 0.0)
        .ok_or_else(|| invalid("capability", "backend-issued capability required"))?
        as u64;
    let cap = engine
        .capabilities
        .borrow()
        .get(&id)
        .and_then(Weak::upgrade)
        .ok_or_else(crate::slots::expired)?;
    let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
    if module.engine.id != engine.id {
        return Err(invalid(
            "foreign_capability",
            "capability belongs to another runtime",
        ));
    }
    Ok(cap)
}
fn wrapped_capability<'s>(
    engine: &Engine,
    value: v8::Local<'s, v8::Value>,
    scope: &mut Scope<'s, '_>,
) -> Result<Rc<Capability>> {
    let raw = helper(engine, scope, "unwrap", &[value])?;
    capability(engine, raw, scope)
}
pub(super) fn raw_cap<'s>(
    engine: &Engine,
    scope: &mut Scope<'s, '_>,
    cap: &Capability,
) -> Result<v8::Local<'s, v8::Object>> {
    let raw = v8::Object::new(scope);
    let brand = v8::Local::new(scope, &engine.brand);
    let id = v8::Number::new(scope, cap.id as f64);
    if raw.set_private(scope, brand, id.into()) != Some(true) {
        return Err(invalid("capability", "cannot brand capability"));
    }
    Ok(raw)
}
pub(crate) fn wrap<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
) -> Result<v8::Local<'s, v8::Value>> {
    if let Kind::Record(record) = &cap.kind {
        return native_view::encode_record(engine, scope, cap, record, 0);
    }
    let raw = raw_cap(engine, scope, cap)?;
    let (kind, metadata) = match &cap.kind {
        Kind::Scope(scope) => ("scope", json!({"hostName":scope.host_name()})),
        Kind::TaskScope(_) => ("task_scope", Value::Null),
        Kind::EventSink(_) => ("event_sink", Value::Null),
        Kind::Setup(_) => ("setup", Value::Null),
        Kind::Declaration { principal, .. } => ("declaration", json!({"principal":principal})),
        Kind::Context(ctx) => (
            "context",
            json!({"principal":ctx.principal(),"codeVersion":ctx.code_version()}),
        ),
        Kind::Signal(_) => ("signal", Value::Null),
        Kind::Reference(reference) => (
            "reference",
            json!({"contract":{"descriptor":reference.contract().descriptor()}}),
        ),
        Kind::Cell(_) => ("cell", Value::Null),
        Kind::Readable(_) => ("readable", Value::Null),
        Kind::Resource(resource) => {
            let resource = resource.upgrade()?;
            (
                "resource",
                json!({"version":resource.version(),"exclusive":resource.exclusive()}),
            )
        }
        Kind::RegistryView(_) => ("registry", Value::Null),
        Kind::Operation(_) => ("operation", Value::Null),
        Kind::Native(_) => ("native", Value::Null),
        Kind::Callable(_) => ("callable", Value::Null),
        Kind::Record(_) => {
            return Err(invalid(
                "native_record",
                "record encoder requires owned scope",
            ));
        }
        Kind::Unsubscribe(_) | Kind::StateSubscription(..) | Kind::RegistrySubscription(..) => {
            ("unsubscribe", Value::Null)
        }
    };
    let kind = to_js(scope, kind)?;
    let metadata = to_js(scope, metadata)?;
    helper(engine, scope, "wrap", &[kind, raw.into(), metadata])
}
pub(crate) fn owned_cap(cap: &Rc<Capability>) -> Result<JsValue> {
    let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
    let mut runtime = module
        .engine
        .runtime
        .try_borrow_mut()
        .map_err(|_| invalid("engine_busy", "capability conversion requires idle engine"))?;
    deno_core::scope!(scope, runtime);
    let value = wrap(&module.engine, scope, cap)?;
    Ok(JsValue {
        value: v8::Global::new(scope, value),
        module: module.clone(),
        native: None,
        setup: cap.owner.upgrade().map(|owner| owner.ancestry()),
    })
}
fn snapshot(snapshot: Snapshot) -> Value {
    json!({"revision":snapshot.revision(),"value":snapshot.value()})
}
fn readable(kind: &Kind) -> Result<Readable> {
    match kind {
        Kind::Cell(cell) => Ok(cell.readable()),
        Kind::Readable(readable) => Ok(readable.clone()),
        _ => Err(invalid("readable", "expected readable capability")),
    }
}

pub(crate) fn prepare_definition(
    engine: &Rc<Engine>,
    host: u64,
    module_id: usize,
    export: &str,
) -> Result<ExecutableFacet> {
    let namespace = engine
        .runtime
        .borrow_mut()
        .get_module_namespace(module_id)
        .map_err(|e| invalid("javascript", e.to_string()))?;
    let (name, uses, setup) = {
        let mut runtime = engine.runtime.borrow_mut();
        deno_core::scope!(scope, runtime);
        let namespace = v8::Local::new(scope, namespace);
        let definition = object(get(scope, namespace, export)?)?;
        let name = string(get(scope, definition, "name")?, scope)?;
        let uses = json_value(get(scope, definition, "uses")?, scope)?;
        let uses = uses
            .as_array()
            .ok_or_else(|| invalid("facet_uses", "uses must be array"))?
            .iter()
            .map(|value| contract(value.get("descriptor").unwrap_or(value).clone()))
            .collect::<Result<Vec<_>>>()?;
        let setup = function(get(scope, definition, "setup")?)?;
        let setup = v8::Global::new(scope, setup);
        (name, uses, setup)
    };
    let module = Rc::new(Module {
        persistent: RefCell::default(),
        host,
        generation: module_id as u64 + 1,
        engine: engine.clone(),
    });
    let setup = Callback::new(&module, setup)?;
    ExecutableFacet::foreign(name, uses, move |scope, declaration| {
        if scope.host_id() != module.host {
            return Err(invalid(
                "foreign_host",
                "prepared facet belongs to another Host",
            ));
        }
        let setup_owner = Owner::new(&module, None);
        declaration
            .recorder()
            .retain_run_owner(setup_owner.clone())?;
        let scope = Capability::new(&module, Some(&setup_owner), Kind::Scope(scope.clone()))?;
        let declaration = Capability::new(
            &module,
            Some(&setup_owner),
            Kind::Setup(declaration.recorder()),
        )?;
        struct Retire(Rc<Capability>);
        impl Drop for Retire {
            fn drop(&mut self) {
                self.0.retire();
            }
        }
        let _retire = Retire(declaration.clone());
        let mut runtime = module
            .engine
            .runtime
            .try_borrow_mut()
            .map_err(|_| invalid("engine_busy", "setup requires idle engine"))?;
        deno_core::scope!(v8scope, runtime);
        v8::tc_scope!(let v8scope,v8scope);
        let scope = wrap(&module.engine, v8scope, &scope)?;
        let declaration = wrap(&module.engine, v8scope, &declaration)?;
        let function = v8::Local::new(v8scope, &setup.function);
        let receiver = v8::undefined(v8scope).into();
        let result = function
            .call(v8scope, receiver, &[scope, declaration])
            .ok_or_else(|| {
                v8scope
                    .exception()
                    .map(|exception| {
                        crate::engine::promise_error(deno_core::error::JsError::from_v8_exception(
                            v8scope, exception,
                        ))
                    })
                    .unwrap_or_else(|| invalid("facet_setup", "setup threw"))
            })?;
        if !result.is_undefined() {
            return Err(invalid(
                "facet_setup",
                "setup must synchronously return undefined",
            ));
        }
        Ok(())
    })
}

#[op2(reentrant)]
fn op_tembra<'s, 'i>(
    scope: &mut v8::PinScope<'s, 'i>,
    state: Rc<RefCell<OpState>>,
    cap: v8::Local<'s, v8::Object>,
    #[string] operation: String,
    args: v8::Local<'s, v8::Array>,
) -> v8::Local<'s, v8::Value> {
    let engine = state.borrow().borrow::<Weak<Engine>>().upgrade();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = engine.ok_or_else(crate::engine::closed)?;
        let cap = capability(&engine, cap.into(), scope)?;
        // The evaluator closure owns only the current borrowed V8 scope. No OpState or
        // authority/table borrow survives into JavaScript or core callbacks.
        dispatch(&engine, scope, &cap, &operation, args)
    }))
    .unwrap_or_else(|_| {
        Err(invalid(
            "foreign_callback_panic",
            "a synchronous native callback panicked at the JavaScript boundary",
        ))
    });
    match result {
        Ok(value) => value,
        Err(error) => {
            let error = json!({
                "__tembra_error":true,"code":error.code.as_str(),
                "reason":error.reason,"message":error.message,"details":error.details()
            });
            to_js(scope, error).unwrap_or_else(|_| v8::undefined(scope).into())
        }
    }
}
fn dispatch<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    operation: &str,
    args: v8::Local<'s, v8::Array>,
) -> Result<v8::Local<'s, v8::Value>> {
    if operation == "native_callable" {
        return callable::dispatch(engine, scope, cap, args);
    }
    if let Some(result) = activity::dispatch(engine, scope, cap, operation, args) {
        return result;
    }
    if let Some(result) = declaration::dispatch(engine, scope, cap, operation, args) {
        return result;
    }
    if let Some(result) = observations::dispatch(engine, scope, cap, operation, args) {
        return result;
    }
    asynchronous::dispatch(engine, scope, cap, operation, args)
}
deno_core::extension!(executor_ops, ops = [op_tembra]);

pub(crate) fn native_capability_result(
    engine: &Engine,
    module: &Module,
    value: &v8::Global<v8::Value>,
) -> Result<Option<Rc<dyn std::any::Any>>> {
    let mut runtime = engine.runtime.borrow_mut();
    deno_core::scope!(scope, runtime);
    let value = v8::Local::new(scope, value);
    let raw = helper(engine, scope, "maybeRaw", &[value])?;
    if raw.is_null_or_undefined() {
        return Ok(None);
    }
    let cap = capability(engine, raw, scope)?;
    check_module(&cap, module)?;
    match &cap.kind {
        Kind::Native(value) => Ok(Some(value.clone())),
        _ => Ok(None),
    }
}
pub(crate) fn native_result(
    engine: &Engine,
    module: &Module,
    value: v8::Global<v8::Value>,
) -> Result<crate::native::NativeValue> {
    let mut runtime = engine.runtime.borrow_mut();
    deno_core::scope!(scope, runtime);
    v8::tc_scope!(let scope, scope);
    let value = v8::Local::new(scope, value);
    native_view::decode(engine, module, scope, value)
}

pub(crate) fn check_module(cap: &Capability, expected: &Module) -> Result<()> {
    let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
    if module.host != expected.host
        || module.generation != expected.generation
        || module.engine.id != expected.engine.id
    {
        return Err(invalid(
            "foreign_generation",
            "capability belongs to another Host or module generation",
        ));
    }
    Ok(())
}
