//! Explicit hook codecs over real typed registry methods, without a second manager.

use super::{
    callable::{self, Body, NativeCall, Projection},
    native::{BrowserMethodCodec, BrowserNativeSignatures, NativeScope, check_position, unit},
    support::{error, js_error},
};
use std::{any::TypeId, rc::Rc};
use tembra_core::{
    Context, Result,
    contracts::{Member, ValueSchema},
};
use tembra_hooks::{HookArgs, HookDescriptor, Next, OutputCompletion};
use wasm_bindgen::JsValue;

type Encode<T> = Rc<dyn Fn(T) -> Result<JsValue>>;
type Decode<T> = Rc<dyn Fn(JsValue) -> Result<T>>;

impl Projection for OutputCompletion {
    fn admit(&mut self) -> Result<()> {
        OutputCompletion::admit(self)
    }
    fn complete(self: Box<Self>, result: Result<()>) {
        (*self).complete(result);
    }
    fn refuse(&mut self, cause: tembra_core::RuntimeError) {
        OutputCompletion::refuse(self, cause);
    }
}

impl BrowserNativeSignatures {
    /// Registers a descriptor's actual contribution signature and trusted receiver wrapper.
    ///
    /// `H` is the handler result (`BailDecision<R>` for bail), not always the semantic hook
    /// result. The input codec receives `A`, while JavaScript receives the checked raw record
    /// `{input, next?: {call, recover}}`. Retained continuations have weak scope authority.
    /// The wrapper binds the real receiver before encoding and joins started continuations
    /// and their output preparation before returning to the core. Registration runs no codec.
    ///
    /// # Errors
    /// Refuses a non-unit absent input, invalid/duplicate signature or adapter capacity without
    /// replacing an existing adapter. Dispatch and continuation failures retain their original
    /// typed causes; a failed final foreign projection remains a pipeline failure.
    pub fn register_hook<A: 'static, H: 'static>(
        &self,
        descriptor: &HookDescriptor,
        encode_input: impl Fn(A) -> Result<JsValue> + 'static,
        decode_input: impl Fn(JsValue) -> Result<A> + 'static,
        encode_output: impl Fn(H) -> Result<JsValue> + 'static,
        decode_output: impl Fn(JsValue) -> Result<H> + 'static,
    ) -> Result<()> {
        if descriptor.input().is_none() && TypeId::of::<A>() != TypeId::of::<()>() {
            return Err(error(
                "hook_signature",
                "absent hook input requires Rust unit",
            ));
        }
        let Member::Method { args, result } = descriptor.contribution_member() else {
            unreachable!("hook contributions are methods")
        };
        let input = descriptor.input().clone();
        let output = result.clone();
        let encoder: Encode<A> = Rc::new(encode_input);
        let decoder: Decode<A> = Rc::new(decode_input);
        let result_encoder: Encode<H> = Rc::new(encode_output);
        let result_decoder: Decode<H> = Rc::new(decode_output);
        let arguments = Arguments {
            input,
            output,
            encoder,
            decoder,
            result_encoder: result_encoder.clone(),
            result_decoder: result_decoder.clone(),
        };
        let codec = BrowserMethodCodec::new(
            |_: HookArgs<A, H>| {
                Err(error(
                    "hook_next_scope",
                    "hook arguments require an entered receiver",
                ))
            },
            |_| {
                Err(error(
                    "hook_next_scope",
                    "hook arguments must originate in the typed manager",
                ))
            },
            move |value| result_encoder(value),
            move |value| result_decoder(value),
        )
        .scoped(Rc::new(move |value, scope, context| {
            arguments.encode(value, scope, context)
        }))
        .with_wrapper(tembra_hooks::hook_wrapper());
        self.register_codec(args, result, codec)
    }
}

struct Arguments<A, H> {
    input: Option<ValueSchema>,
    output: Option<ValueSchema>,
    encoder: Encode<A>,
    decoder: Decode<A>,
    result_encoder: Encode<H>,
    result_decoder: Decode<H>,
}
impl<A: 'static, H: 'static> Arguments<A, H> {
    fn encode(
        &self,
        args: HookArgs<A, H>,
        scope: &NativeScope,
        context: &Context,
    ) -> Result<JsValue> {
        let (input, next) = args.into_parts();
        let input = encode(&self.input, &self.encoder, input, true)?;
        let (call, recover) = if let Some(next) = next {
            self.continuation(next, scope, context)?
        } else {
            (JsValue::UNDEFINED, JsValue::UNDEFINED)
        };
        callable::hook_record(&input, &call, &recover).map_err(js_error)
    }
    fn continuation(
        &self,
        next: Next<A, H>,
        scope: &NativeScope,
        context: &Context,
    ) -> Result<(JsValue, JsValue)> {
        let input = self.input.clone();
        let output = self.output.clone();
        let decoder = self.decoder.clone();
        let encoder = self.result_encoder.clone();
        let call = next.clone();
        let body: Body = Rc::new(move |raw| {
            let input = input.clone();
            let output = output.clone();
            let decoder = decoder.clone();
            let encoder = encoder.clone();
            let (future, completion) = call
                .call_with(
                    move || decode(&input, &decoder, raw()?, true),
                    move |value| encode(&output, &encoder, value, false),
                )
                .into_parts();
            Ok(NativeCall::new(future, Box::new(completion)))
        });
        let call = callable::mint(scope, context, body)?;
        let output = self.output.clone();
        let decoder = self.result_decoder.clone();
        let encoder = self.result_encoder.clone();
        let body: Body = Rc::new(move |raw| {
            let decode_schema = output.clone();
            let encode_schema = output.clone();
            let decoder = decoder.clone();
            let encoder = encoder.clone();
            let (future, completion) = next
                .recover_with(
                    move || decode(&decode_schema, &decoder, raw()?, false),
                    move |value| encode(&encode_schema, &encoder, value, false),
                )
                .into_parts();
            Ok(NativeCall::new(future, Box::new(completion)))
        });
        let recover = callable::mint(scope, context, body)?;
        Ok((call, recover))
    }
}
fn encode<T>(
    schema: &Option<ValueSchema>,
    codec: &Encode<T>,
    value: T,
    argument: bool,
) -> Result<JsValue> {
    if schema.is_some() {
        check_position(schema, codec(value)?, argument)
    } else {
        Ok(JsValue::UNDEFINED)
    }
}
fn decode<T: 'static>(
    schema: &Option<ValueSchema>,
    codec: &Decode<T>,
    value: JsValue,
    argument: bool,
) -> Result<T> {
    if schema.is_some() {
        codec(check_position(schema, value, argument)?)
    } else {
        if !value.is_undefined() {
            return Err(error(
                "native_presence",
                "absent hook position requires absence",
            ));
        }
        unit()
    }
}

struct PortableAdapter<'a>(&'a BrowserNativeSignatures);
impl tembra_hooks::portable::PortableHookAdapter for PortableAdapter<'_> {
    fn register<
        A: tembra_hooks::portable::PortableValue,
        H: tembra_hooks::portable::PortableValue,
    >(
        &self,
        descriptor: &HookDescriptor,
    ) -> Result<()> {
        let unit_bail =
            descriptor.strategy() == tembra_hooks::Strategy::Bail && descriptor.result().is_none();
        self.0.register_hook::<A, H>(
            descriptor,
            portable_encode::<A>,
            portable_decode::<A>,
            portable_encode::<H>,
            move |value| {
                if unit_bail {
                    let json =
                        crate::intrinsics::portable_bail_json_owned(&value).map_err(js_error)?;
                    H::decode(Some(
                        tembra_contracts::parse_json(&json)
                            .map_err(|e| error(&e.reason, &e.message))?,
                    ))
                } else {
                    portable_decode::<H>(value)
                }
            },
        )
    }
}
fn portable_encode<T: tembra_hooks::portable::PortableValue>(value: T) -> Result<JsValue> {
    match value.encode()? {
        Some(value) => {
            crate::intrinsics::parse_portable_owned(&value.to_string()).map_err(js_error)
        }
        None => Ok(JsValue::UNDEFINED),
    }
}
fn portable_decode<T: tembra_hooks::portable::PortableValue>(value: JsValue) -> Result<T> {
    T::decode(if value.is_undefined() {
        None
    } else {
        let json = crate::intrinsics::portable_json_owned(&value).map_err(js_error)?;
        Some(tembra_contracts::parse_json(&json).map_err(|e| error(&e.reason, &e.message))?)
    })
}
impl super::WasmExecutor {
    pub(crate) fn register_portable_hook(&self, descriptor: HookDescriptor) -> Result<()> {
        if self
            .session
            .hooks
            .borrow()
            .contains_key(descriptor.fingerprint())
        {
            return Ok(());
        }
        let key = descriptor.fingerprint().to_owned();
        let hook = tembra_hooks::portable::PortableHook::register(
            descriptor,
            &PortableAdapter(&self.session.native),
        )?;
        self.session.hooks.borrow_mut().insert(key, hook);
        Ok(())
    }
}
