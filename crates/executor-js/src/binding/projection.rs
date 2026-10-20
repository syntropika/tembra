use super::asynchronous::NativeOutput;
use super::*;

pub(crate) fn resolve(
    engine: &Rc<Engine>,
    module: &Rc<Module>,
    resolver: v8::Global<v8::PromiseResolver>,
    result: Result<NativeOutput>,
) -> Result<()> {
    let mut runtime = engine.runtime.borrow_mut();
    deno_core::scope!(scope, runtime);
    v8::tc_scope!(let scope,scope);
    let resolver = v8::Local::new(scope, resolver);
    match result {
        Err(error) => reject(engine, scope, resolver, &error),
        Ok(output) => {
            let prepared = output_value(engine, module, scope, output)
                .and_then(|value| helper(engine, scope, "safeResolution", &[value]));
            match prepared {
                Ok(value) if resolver.resolve(scope, value) == Some(true) => Ok(()),
                Ok(_) => Err(invalid("native_projection", "promise resolution failed")),
                Err(error) => {
                    scope.reset();
                    let _ = reject(engine, scope, resolver, &error);
                    Err(error)
                }
            }
        }
    }
}
pub(super) fn reject<'s>(
    engine: &Engine,
    scope: &mut Scope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    error: &RuntimeError,
) -> Result<()> {
    let (exception, projection) = match error_value(scope, error) {
        Ok(value) => (value, Ok(())),
        Err(error) => {
            engine.report(error.clone());
            let message = v8::String::empty(scope);
            (v8::Exception::error(scope, message), Err(error))
        }
    };
    if resolver.reject(scope, exception) != Some(true) {
        let error = invalid("error_projection", "promise rejection preparation failed");
        engine.report(error.clone());
        return Err(error);
    }
    projection
}

fn output_value<'s>(
    engine: &Rc<Engine>,
    module: &Rc<Module>,
    scope: &mut Scope<'s, '_>,
    output: NativeOutput,
) -> Result<v8::Local<'s, v8::Value>> {
    match output {
        NativeOutput::Json(value) => to_js(scope, value),
        NativeOutput::Unit => Ok(v8::undefined(scope).into()),
        NativeOutput::Value(value) => {
            if value.module.engine.id != engine.id {
                return Err(invalid(
                    "foreign_runtime",
                    "returned value belongs to another engine",
                ));
            }
            Ok(v8::Local::new(scope, value.value))
        }
        NativeOutput::Cap(cap) => wrap(engine, scope, &cap),
        NativeOutput::Native(owner, value) => {
            native_view::encode(engine, scope, module, &owner, value, 0)
        }
        NativeOutput::NativePayload(owner, value) => {
            let present = !value.is_absent();
            let payload = v8::Object::new(scope);
            let key =
                v8::String::new(scope, "present").ok_or_else(|| limit("native_projection"))?;
            let flag = v8::Boolean::new(scope, present);
            if payload.define_own_property(
                scope,
                key.into(),
                flag.into(),
                v8::PropertyAttribute::NONE,
            ) != Some(true)
            {
                return Err(invalid(
                    "native_projection",
                    "payload presence field failed",
                ));
            }
            if present {
                let value = native_view::encode(engine, scope, module, &owner, value, 0)?;
                let key =
                    v8::String::new(scope, "value").ok_or_else(|| limit("native_projection"))?;
                if payload.define_own_property(
                    scope,
                    key.into(),
                    value,
                    v8::PropertyAttribute::NONE,
                ) != Some(true)
                {
                    return Err(invalid("native_projection", "payload value field failed"));
                }
            }
            Ok(payload.into())
        }
    }
}
fn error_value<'s>(
    scope: &mut Scope<'s, '_>,
    error: &RuntimeError,
) -> Result<v8::Local<'s, v8::Value>> {
    let serialized = serde_json::to_string(error)
        .map_err(|cause| invalid("error_projection", cause.to_string()))?;
    let message =
        v8::String::new(scope, &error.message).ok_or_else(|| limit("error_projection"))?;
    let exception = v8::Exception::error(scope, message);
    let object = v8::Local::<v8::Object>::try_from(exception)
        .map_err(|_| invalid("error_projection", "error object creation failed"))?;
    for (key, value) in [
        ("code", json!(error.code.as_str())),
        ("reason", json!(error.reason)),
        ("message", json!(error.message)),
        ("details", error.details().clone()),
    ] {
        let key = v8::String::new(scope, key).ok_or_else(|| limit("error_projection"))?;
        let value = to_js(scope, value)?;
        if object.define_own_property(scope, key.into(), value, v8::PropertyAttribute::NONE)
            != Some(true)
        {
            return Err(invalid("error_projection", "error field creation failed"));
        }
    }
    let key =
        v8::String::new(scope, "tembraRuntimeError").ok_or_else(|| limit("error_projection"))?;
    let value = v8::String::new(scope, &serialized).ok_or_else(|| limit("error_projection"))?;
    if object.define_own_property(scope, key.into(), value.into(), v8::PropertyAttribute::NONE)
        != Some(true)
    {
        return Err(invalid(
            "error_projection",
            "error metadata creation failed",
        ));
    }
    let symbol = v8::String::new(scope, "errorAdditionalPropertyKeys")
        .ok_or_else(|| limit("error_projection"))?;
    let symbol = v8::Symbol::for_key(scope, symbol);
    let keys = v8::Array::new_with_elements(scope, &[key.into()]);
    if object.define_own_property(
        scope,
        symbol.into(),
        keys.into(),
        v8::PropertyAttribute::NONE,
    ) != Some(true)
    {
        return Err(invalid(
            "error_projection",
            "error metadata creation failed",
        ));
    }
    Ok(exception)
}
