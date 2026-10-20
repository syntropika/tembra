//! Explicit typed adapter for contribution methods. Composition remains in tembra-hooks.
use crate::{
    Executor, NativeCall, NativeCallControl, NativeCallable, NativeMethodCodec,
    NativeMethodSignature, NativeRecord, NativeValue, ScopedCodec, engine::invalid,
};
use std::{
    any::{Any, TypeId},
    rc::Rc,
};
use tembra_core::{Result, contracts::Member};
use tembra_hooks::{HookArgs, HookDescriptor, Next};

impl Executor {
    /// Registers the descriptor's contribution signature and trusted receiver wrapper.
    /// `H` is the handler result (including `BailDecision<R>` for bail), not semantic `R`.
    /// Absent positions use actual unit and bypass user codecs. Registration invokes no
    /// callback and acquires no Host. The manager still checks actual selected method types.
    /// # Errors
    /// Refuses duplicate/invalid signatures, non-unit absent input and adapter capacity.
    /// Later codec or handler effects are never rolled back by this registration.
    pub fn register_hook<A: 'static, H: 'static>(
        &self,
        descriptor: &HookDescriptor,
        input: Rc<dyn ScopedCodec<A>>,
        output: Rc<dyn ScopedCodec<H>>,
    ) -> Result<()> {
        let Member::Method { args, result } = descriptor.contribution_member() else {
            unreachable!("hook contribution is a method");
        };
        let input_schema = descriptor
            .input()
            .clone()
            .map(crate::native::position_key)
            .transpose()?;
        let result_schema = result
            .clone()
            .map(crate::native::position_key)
            .transpose()?;
        let input_present = input_schema.is_some();
        if !input_present && TypeId::of::<A>() != TypeId::of::<()>() {
            return Err(invalid("hook_signature", "absent hook input requires unit"));
        }
        let input = crate::native::safe_codec(input);
        let output = crate::native::safe_codec(output);
        self.register_native_method(
            NativeMethodSignature::from_schemas(args, result)?,
            NativeMethodCodec::new(
                Rc::new(Arguments {
                    input,
                    result: output.clone(),
                    input_schema,
                    result_schema,
                }),
                output,
            )
            .with_wrapper(tembra_hooks::hook_wrapper()),
        )
    }
}
struct Arguments<A, H> {
    input: Rc<dyn ScopedCodec<A>>,
    result: Rc<dyn ScopedCodec<H>>,
    input_schema: Option<String>,
    result_schema: Option<String>,
}
impl<A: 'static, H: 'static> ScopedCodec<HookArgs<A, H>> for Arguments<A, H> {
    fn encode(&self, args: HookArgs<A, H>) -> Result<NativeValue> {
        let (input, next) = args.into_parts();
        let mut fields = vec![(
            "input".into(),
            encode(&self.input_schema, self.input.as_ref(), input, true)?,
        )];
        if let Some(next) = next {
            fields.push(("next".into(), self.continuation(next)?));
        }
        Ok(NativeValue::record(NativeRecord::new(fields)?))
    }
    fn decode(&self, _: NativeValue) -> Result<HookArgs<A, H>> {
        Err(invalid(
            "hook_next_scope",
            "hook arguments must originate in the typed manager",
        ))
    }
}
impl<A: 'static, H: 'static> Arguments<A, H> {
    fn continuation(&self, next: Next<A, H>) -> Result<NativeValue> {
        let (input, output) = (self.input.clone(), self.result.clone());
        let call = next.clone();
        let input_schema = self.input_schema.clone();
        let result_schema = self.result_schema.clone();
        let call_result_schema = result_schema.clone();
        let call = NativeCallable::deferred(Rc::new(move |raw| {
            let (input, output) = (input.clone(), output.clone());
            let input_schema = input_schema.clone();
            let result_schema = call_result_schema.clone();
            let (future, completion) = call
                .call_with(
                    move || decode(&input_schema, input.as_ref(), raw()?, true),
                    move |value| encode(&result_schema, output.as_ref(), value, false),
                )
                .into_parts();
            NativeCall::with_control(future, Box::new(HookControl(completion)))
        }));
        let output = self.result.clone();
        let recover = NativeCallable::deferred(Rc::new(move |raw| {
            let (decoder, encoder) = (output.clone(), output.clone());
            let decode_schema = result_schema.clone();
            let encode_schema = result_schema.clone();
            let (future, completion) = next
                .recover_with(
                    move || decode(&decode_schema, decoder.as_ref(), raw()?, false),
                    move |value| encode(&encode_schema, encoder.as_ref(), value, false),
                )
                .into_parts();
            NativeCall::with_control(future, Box::new(HookControl(completion)))
        }));
        Ok(NativeValue::record(NativeRecord::new([
            ("call".into(), NativeValue::callable(call)),
            ("recover".into(), NativeValue::callable(recover)),
        ])?))
    }
}
struct HookControl(tembra_hooks::OutputCompletion);
impl NativeCallControl for HookControl {
    fn admit(&mut self) -> Result<()> {
        self.0.admit()
    }
    fn refuse(&mut self, cause: tembra_core::RuntimeError) {
        self.0.refuse(cause);
    }
    fn complete(self: Box<Self>, projection: Result<()>) {
        self.0.complete(projection);
    }
}
fn encode<T: 'static>(
    schema: &Option<String>,
    codec: &dyn ScopedCodec<T>,
    value: T,
    arguments: bool,
) -> Result<NativeValue> {
    if schema.is_some() {
        crate::native::check_position(schema, codec.encode(value)?, arguments)
    } else {
        drop(value);
        Ok(NativeValue::absent())
    }
}
fn decode<T: 'static>(
    schema: &Option<String>,
    codec: &dyn ScopedCodec<T>,
    value: NativeValue,
    arguments: bool,
) -> Result<T> {
    let value = crate::native::check_position(schema, value, arguments)?;
    if schema.is_some() {
        return codec.decode(value);
    }
    if !value.is_absent() {
        return Err(invalid(
            "native_presence",
            "absent hook position requires absence",
        ));
    }
    (Box::new(()) as Box<dyn Any>)
        .downcast::<T>()
        .map(|value| *value)
        .map_err(|_| invalid("hook_signature", "absent hook position requires unit"))
}
