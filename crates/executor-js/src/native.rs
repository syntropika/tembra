mod value;
pub(crate) use value::{CompletionGuard, NativeData};
pub use value::{
    DeferredNativeCallableFn, NativeCall, NativeCallControl, NativeCallable, NativeCallableFn,
    NativeDecoder, NativeRecord, NativeValue,
};
mod opaque;
use crate::{
    binding,
    engine::{Conversion, Engine, Executor, Output, invalid},
    slots::*,
};
use deno_core::v8;
use futures::{FutureExt, future::LocalBoxFuture};
pub(crate) use opaque::ErasedOpaque;
use serde_json::Value;
use std::{any::Any, rc::Rc};
use tembra_core::{Context, Implementation, Reference, Result, contracts::ValueSchema};

/// Explicit conversion for one Rust type at a registered JavaScript method boundary.
/// Implementations may have external effects; their failures do not promise rollback.
/// Registered codecs can run inside a reentrant V8 operation or on the Driver pump.
/// They must not block or recursively drive the executor. Synchronous same-engine core
/// reentry is supported only through its current scoped evaluator; asynchronous work
/// must use owned queued operations. Unwind panics are mapped to `invalid/codec_panic`.
/// Panic-abort builds, allocation failure and process termination cannot be recovered.
pub trait ScopedCodec<T>: 'static {
    /// Converts one real Rust value to an explicitly chosen JavaScript view.
    /// # Errors
    /// Reports codec-specific refusal; effects performed by the codec are not rolled back.
    fn encode(&self, value: T) -> Result<NativeValue>;
    /// Reconstructs the real Rust value from its checked adapter view.
    /// # Errors
    /// Refuses unsupported views or nominal types according to this codec's contract.
    fn decode(&self, value: NativeValue) -> Result<T>;
}
struct SafeScopedCodec<T>(Rc<dyn ScopedCodec<T>>);
impl<T: 'static> ScopedCodec<T> for SafeScopedCodec<T> {
    fn encode(&self, value: T) -> Result<NativeValue> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.encode(value)))
            .map_err(|_| invalid("codec_panic", "native codec encode panicked"))?
    }
    fn decode(&self, value: NativeValue) -> Result<T> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.decode(value)))
            .map_err(|_| invalid("codec_panic", "native codec decode panicked"))?
    }
}
pub(crate) fn safe_codec<T: 'static>(codec: Rc<dyn ScopedCodec<T>>) -> Rc<dyn ScopedCodec<T>> {
    Rc::new(SafeScopedCodec(codec))
}
/// The complete argument/result conversion pair for one nominal signature.
pub struct NativeMethodCodec<A, R> {
    arguments: Rc<dyn ScopedCodec<A>>,
    result: Rc<dyn ScopedCodec<R>>,
    wrapper: Option<tembra_hooks::MethodWrapper<A, R>>,
}
impl<A, R> NativeMethodCodec<A, R> {
    pub fn new(arguments: Rc<dyn ScopedCodec<A>>, result: Rc<dyn ScopedCodec<R>>) -> Self {
        Self {
            arguments,
            result,
            wrapper: None,
        }
    }
    /// Installs a trusted typed receiver wrapper before any argument encoding.
    /// Wrapper effects and errors follow its own contract; it receives the actual core Context.
    /// This is opt-in for every method with this exact registered argument/result signature;
    /// it is not global interception. Unrelated signatures are unaffected.
    pub fn with_wrapper(mut self, wrapper: tembra_hooks::MethodWrapper<A, R>) -> Self {
        self.wrapper = Some(wrapper);
        self
    }
}
/// Exact native nominal tokens; an absent position means the unit/void contract position.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeMethodSignature {
    arguments: Option<String>,
    result: Option<String>,
}
impl NativeMethodSignature {
    /// Checks nominal spelling without registering or invoking code.
    /// # Errors
    /// Refuses empty/control identifiers and a signature with neither native position.
    pub fn new(arguments: Option<String>, result: Option<String>) -> Result<Self> {
        Self::from_schemas(
            arguments.map(ValueSchema::Native),
            result.map(ValueSchema::Native),
        )
    }
    /// Checks the entire normalized signature, including explicitly portable positions.
    /// # Errors
    /// Refuses invalid schemas/nominal IDs or a signature without any native position.
    pub fn from_schemas(
        arguments: Option<ValueSchema>,
        result: Option<ValueSchema>,
    ) -> Result<Self> {
        if !arguments
            .iter()
            .chain(result.iter())
            .any(|schema| matches!(schema, ValueSchema::Native(_)))
        {
            return Err(invalid(
                "native_signature",
                "signature needs at least one native position",
            ));
        }
        Ok(Self {
            arguments: arguments.map(position_key).transpose()?,
            result: result.map(position_key).transpose()?,
        })
    }
    pub(crate) fn from_member(
        args: &Option<ValueSchema>,
        result: &Option<ValueSchema>,
    ) -> Result<Self> {
        Self::from_schemas(args.clone(), result.clone())
    }
}
pub(crate) fn position_key(schema: ValueSchema) -> Result<String> {
    let value = match schema {
        ValueSchema::Native(id) => {
            if id.is_empty() || id.chars().any(char::is_control) {
                return Err(invalid("native_signature", "invalid nominal identifier"));
            }
            serde_json::json!({"native":id})
        }
        ValueSchema::Portable(schema) => tembra_core::contracts::normalize_schema(&schema)
            .map_err(|e| invalid(e.reason, e.message))?,
    };
    tembra_core::contracts::canonical_json(&value).map_err(|e| invalid(e.reason, e.message))
}
pub(crate) trait ErasedNative {
    fn install(
        &self,
        candidate: Implementation,
        name: &str,
        handler: Rc<Callback>,
        owner: Rc<Owner>,
    ) -> Result<Implementation>;
    fn invoke_operation(
        &self,
        operation: tembra_core::RegisteredOperation,
        input: NativeValue,
        context: Context,
        owner: Rc<Owner>,
    ) -> Result<LocalBoxFuture<'static, Result<binding::asynchronous::NativeOutput>>>;
    fn invoke(
        &self,
        reference: Reference,
        member: String,
        input: NativeValue,
        context: Option<Context>,
        owner: Rc<Owner>,
    ) -> Result<LocalBoxFuture<'static, Result<binding::asynchronous::NativeOutput>>>;
}
struct Adapter<A, R> {
    codec: NativeMethodCodec<A, R>,
    signature: NativeMethodSignature,
}
impl Executor {
    /// Registers one whole signature. Both directions use monomorphized existing core calls.
    /// Absent positions use actual Rust unit and bypass their codecs. Void results are
    /// discarded after physical settlement without inspecting JavaScript values.
    /// # Errors
    /// Refuses duplicate signatures or non-unit Rust types at absent positions. Refusal
    /// invokes no handler and does not replace existing adapters.
    pub fn register_native_method<A: 'static, R: 'static>(
        &self,
        signature: NativeMethodSignature,
        codec: NativeMethodCodec<A, R>,
    ) -> Result<()> {
        if signature.arguments.is_none()
            && std::any::TypeId::of::<A>() != std::any::TypeId::of::<()>()
            || signature.result.is_none()
                && std::any::TypeId::of::<R>() != std::any::TypeId::of::<()>()
        {
            return Err(invalid(
                "native_signature",
                "absent positions require unit Rust types",
            ));
        }
        let mut adapters = self.inner.natives.borrow_mut();
        if adapters.len() + self.inner.opaques.borrow().len() >= self.inner.options.max_callbacks {
            return Err(crate::engine::limit("native_adapters"));
        }
        if adapters.contains_key(&signature) {
            return Err(invalid("native_signature", "duplicate signature"));
        }
        let codec = NativeMethodCodec {
            arguments: safe_codec(codec.arguments),
            result: safe_codec(codec.result),
            wrapper: codec.wrapper,
        };
        adapters.insert(signature.clone(), Rc::new(Adapter { codec, signature }));
        Ok(())
    }
}
impl<A: 'static, R: 'static> ErasedNative for Adapter<A, R> {
    fn install(
        &self,
        candidate: Implementation,
        name: &str,
        handler: Rc<Callback>,
        owner: Rc<Owner>,
    ) -> Result<Implementation> {
        let encode = self.codec.arguments.clone();
        let decode = self.codec.result.clone();
        let signature = self.signature.clone();
        let weak = Rc::downgrade(&owner);
        let invoke: tembra_hooks::MethodHandler<A, R> = Rc::new(move |args: A, context| {
            let mut observation = context.observe(
                tembra_core::observability::Operation::Executor,
                "native_callback",
            );
            let admitted: Result<_> = (|| {
                let owner = weak.upgrade().ok_or_else(crate::slots::expired)?;
                let lease = owner
                    .invocation
                    .as_ref()
                    .ok_or_else(|| invalid("invocation_owner", "missing owner"))?
                    .retain(&context)?;
                let owner = Owner::child(&owner, owner.invocation.clone());
                let cap = Capability::new(&owner.module, Some(&owner), Kind::Context(context))?;
                let mut values = vec![binding::owned_cap(&cap)?];
                let encoded = if signature.arguments.is_some() {
                    check_position(&signature.arguments, encode.encode(args)?, true)?
                } else {
                    drop(args);
                    NativeValue::absent()
                };
                if !encoded.is_absent() {
                    values.push(owned_native(&owner, encoded)?);
                }
                Ok((owner, lease, values))
            })();
            if let (Some(observation), Err(error)) = (&mut observation, &admitted) {
                observation.result::<()>(&Err(error.clone()));
            }
            let handler = handler.clone();
            let decode = decode.clone();
            let result_schema = signature.result.clone();
            async move {
                let (owner, lease, args) = admitted?;
                let conversion = if result_schema.is_none() {
                    Conversion::NativeVoid
                } else {
                    Conversion::Native(Box::new(move |value| {
                        decode
                            .decode(check_position(&result_schema, value, false)?)
                            .map(|value| Box::new(value) as Box<dyn Any>)
                    }))
                };
                match handler
                    .module
                    .engine
                    .clone()
                    .enqueue(
                        handler,
                        (args, observation),
                        Some(owner),
                        Some(lease),
                        conversion,
                    )
                    .await?
                {
                    Output::Native(value) => value
                        .downcast::<R>()
                        .map(|value| *value)
                        .map_err(|_| invalid("native_type", "internal native result mismatch")),
                    _ => Err(invalid("native_type", "invalid native completion")),
                }
            }
            .boxed_local()
        });
        let wrapper = self.codec.wrapper.clone();
        Ok(candidate.method(name, move |args, context| match &wrapper {
            Some(wrapper) => wrapper(args, context, invoke.clone()),
            None => invoke(args, context),
        }))
    }
    fn invoke_operation(
        &self,
        operation: tembra_core::RegisteredOperation,
        input: NativeValue,
        context: Context,
        owner: Rc<Owner>,
    ) -> Result<LocalBoxFuture<'static, Result<binding::asynchronous::NativeOutput>>> {
        let input = decode_arguments(
            &self.signature.arguments,
            self.codec.arguments.as_ref(),
            input,
        )?;
        let encode = self.codec.result.clone();
        let schema = self.signature.result.clone();
        Ok(async move {
            let output = operation.call::<A, R>(&context, input).await?;
            Ok(binding::asynchronous::NativeOutput::Native(
                owner,
                encode_result(&schema, encode.as_ref(), output)?,
            ))
        }
        .boxed_local())
    }
    fn invoke(
        &self,
        reference: Reference,
        member: String,
        input: NativeValue,
        context: Option<Context>,
        owner: Rc<Owner>,
    ) -> Result<LocalBoxFuture<'static, Result<binding::asynchronous::NativeOutput>>> {
        let input = decode_arguments(
            &self.signature.arguments,
            self.codec.arguments.as_ref(),
            input,
        )?;
        let encode = self.codec.result.clone();
        let result_schema = self.signature.result.clone();
        Ok(async move {
            let output = reference
                .call::<A, R>(&member, input, context.as_ref())
                .await?;
            Ok(binding::asynchronous::NativeOutput::Native(
                owner,
                encode_result(&result_schema, encode.as_ref(), output)?,
            ))
        }
        .boxed_local())
    }
}
pub(crate) fn owned_native(owner: &Rc<Owner>, value: NativeValue) -> Result<JsValue> {
    let module = &owner.module;
    let mut runtime = module
        .engine
        .runtime
        .try_borrow_mut()
        .map_err(|_| invalid("engine_busy", "native view requires idle engine"))?;
    deno_core::scope!(scope, runtime);
    v8::tc_scope!(let scope,scope);
    let value = binding::native_view::encode(&module.engine, scope, module, owner, value, 0)?;
    Ok(JsValue {
        value: v8::Global::new(scope, value),
        module: module.clone(),
        native: None,
        setup: Some(owner.ancestry()),
    })
}
pub(crate) fn native_at<'s>(
    engine: &Engine,
    module: &Module,
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<NativeValue> {
    binding::native_view::decode(engine, module, scope, value)
}

pub(crate) fn check_position(
    schema: &Option<String>,
    value: NativeValue,
    arguments: bool,
) -> Result<NativeValue> {
    let Some(schema) = schema else {
        if !value.is_absent() {
            return Err(invalid(
                "native_presence",
                "absent signature position requires absence",
            ));
        }
        return Ok(value);
    };
    let schema: Value =
        serde_json::from_str(schema).map_err(|e| invalid("native_signature", e.to_string()))?;
    if schema.get("native").is_some() {
        if value.is_absent() {
            return Err(invalid(
                "native_presence",
                "present native position requires a value",
            ));
        }
        return Ok(value);
    }
    let value = value.into_portable()?;
    let value = if arguments {
        let cleaned = tembra_core::contracts::clean_arguments(&schema, &value)
            .map_err(|e| invalid(e.reason, e.message))?;
        tembra_core::contracts::validate(&schema, &cleaned)
            .map_err(|e| invalid(e.reason, e.message))?;
        cleaned
    } else {
        tembra_core::contracts::validate(&schema, &value)
            .map_err(|e| invalid(e.reason, e.message))?;
        value
    };
    Ok(NativeValue::portable(value))
}

fn decode_arguments<A: 'static>(
    schema: &Option<String>,
    codec: &dyn ScopedCodec<A>,
    input: NativeValue,
) -> Result<A> {
    let input = check_position(schema, input, true)?;
    if schema.is_some() {
        codec.decode(input)
    } else {
        (Box::new(()) as Box<dyn Any>)
            .downcast::<A>()
            .map(|value| *value)
            .map_err(|_| invalid("native_signature", "absent arguments require unit"))
    }
}
fn encode_result<R: 'static>(
    schema: &Option<String>,
    codec: &dyn ScopedCodec<R>,
    output: R,
) -> Result<NativeValue> {
    if schema.is_some() {
        check_position(schema, codec.encode(output)?, false)
    } else {
        drop(output);
        Ok(NativeValue::absent())
    }
}
