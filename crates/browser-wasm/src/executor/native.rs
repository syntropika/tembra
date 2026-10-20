use super::{
    Generation,
    support::{error, js_error},
};
use crate::intrinsics::{parse_portable_owned, portable_json_owned};
use futures::{FutureExt, future::LocalBoxFuture};
use std::{
    any::{Any, TypeId},
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::contracts::{Contract, Member, ValueSchema, canonical_json};
use tembra_core::{
    Context, Implementation, InvocationOwner, Reference, RegisteredOperation, Resource, Result,
};
use wasm_bindgen::JsValue;

/// A realm-local managed value. This carrier is optional, not a universal native ABI.
pub struct BrowserValue(pub JsValue);
pub(crate) type Invoke = Rc<dyn Fn(JsValue, Context) -> LocalBoxFuture<'static, Result<JsValue>>>;
type Encode<T> = Rc<dyn Fn(T) -> Result<JsValue>>;
type Decode<T> = Rc<dyn Fn(JsValue) -> Result<T>>;
type ScopedEncode<T> = Rc<dyn Fn(T, &NativeScope, &Context) -> Result<JsValue>>;
/// A real typed managed method; the core retains its ordinary admission and ownership.
pub type BrowserMethodHandler<A, R> = Rc<dyn Fn(A, Context) -> LocalBoxFuture<'static, Result<R>>>;
/// An explicitly registered whole-method wrapper, run before argument encoding.
pub type BrowserMethodWrapper<A, R> =
    Rc<dyn Fn(A, Context, BrowserMethodHandler<A, R>) -> LocalBoxFuture<'static, Result<R>>>;

#[derive(Clone)]
pub(crate) struct NativeScope {
    pub generation: Rc<Generation>,
    pub owner: InvocationOwner,
}
/// Explicit native codecs and an optional typed wrapper. It creates no new dispatch runtime.
/// Conversions and wrappers are trusted Rust callbacks: they must not panic. The wasm32 build
/// does not provide unwinding-based recovery from a Rust panic. Their external effects are not
/// rolled back when a later conversion or managed call refuses.
/// Promise-projected native views must have stable ordinary descriptors. The adapter refuses
/// visible thenables without reading their getters; arbitrary proxy behavior is not sandboxed.
pub struct BrowserMethodCodec<A, R> {
    encode_args: Encode<A>,
    decode_args: Decode<A>,
    encode_result: Encode<R>,
    decode_result: Decode<R>,
    wrapper: Option<BrowserMethodWrapper<A, R>>,
    scoped_args: Option<ScopedEncode<A>>,
}
impl<A, R> BrowserMethodCodec<A, R> {
    /// Stores trusted conversions without invoking them.
    pub fn new(
        encode_args: impl Fn(A) -> Result<JsValue> + 'static,
        decode_args: impl Fn(JsValue) -> Result<A> + 'static,
        encode_result: impl Fn(R) -> Result<JsValue> + 'static,
        decode_result: impl Fn(JsValue) -> Result<R> + 'static,
    ) -> Self {
        Self {
            encode_args: Rc::new(encode_args),
            decode_args: Rc::new(decode_args),
            encode_result: Rc::new(encode_result),
            decode_result: Rc::new(decode_result),
            wrapper: None,
            scoped_args: None,
        }
    }
    /// Installs an explicit wrapper around the real typed handler, before any codec executes.
    /// The embedding opts in for this exact registered signature. This does not intercept
    /// arbitrary Host methods, select hook composition, or add a second dispatch runtime.
    pub fn with_wrapper(mut self, wrapper: BrowserMethodWrapper<A, R>) -> Self {
        self.wrapper = Some(wrapper);
        self
    }
    pub(crate) fn scoped(mut self, encode: ScopedEncode<A>) -> Self {
        self.scoped_args = Some(encode);
        self
    }
}

pub(crate) trait Signature {
    fn install(
        &self,
        implementation: Implementation,
        name: String,
        invoke: Invoke,
        scope: NativeScope,
    ) -> Implementation;
    fn call(
        &self,
        reference: Reference,
        name: String,
        input: JsValue,
        context: Option<Context>,
    ) -> LocalBoxFuture<'static, Result<JsValue>>;
    fn call_registered(
        &self,
        operation: RegisteredOperation,
        input: JsValue,
        context: Context,
    ) -> LocalBoxFuture<'static, Result<JsValue>>;
}
struct Typed<A, R> {
    args: Option<ValueSchema>,
    result: Option<ValueSchema>,
    encode_args: ScopedEncode<A>,
    decode_args: Decode<A>,
    encode_result: Encode<R>,
    decode_result: Decode<R>,
    wrapper: Option<BrowserMethodWrapper<A, R>>,
}
impl<A: 'static, R: 'static> Signature for Typed<A, R> {
    fn install(
        &self,
        implementation: Implementation,
        name: String,
        invoke: Invoke,
        scope: NativeScope,
    ) -> Implementation {
        let encode = self.encode_args.clone();
        let decode = self.decode_result.clone();
        let args_schema = self.args.clone();
        let result_schema = self.result.clone();
        let handler: BrowserMethodHandler<A, R> = Rc::new(move |args: A, context: Context| {
            let invoke = invoke.clone();
            let encode = encode.clone();
            let decode = decode.clone();
            let args_schema = args_schema.clone();
            let result_schema = result_schema.clone();
            let scope = scope.clone();
            async move {
                let input = if args_schema.is_some() {
                    check_position(&args_schema, encode(args, &scope, &context)?, true)?
                } else {
                    JsValue::UNDEFINED
                };
                let result = invoke(input, context).await?;
                if result_schema.is_some() {
                    decode(check_position(&result_schema, result, false)?)
                } else {
                    unit()
                }
            }
            .boxed_local()
        });
        let wrapper = self.wrapper.clone();
        implementation.method(name, move |args, context| match &wrapper {
            Some(wrapper) => wrapper(args, context, handler.clone()),
            None => handler(args, context),
        })
    }
    fn call(
        &self,
        reference: Reference,
        name: String,
        input: JsValue,
        context: Option<Context>,
    ) -> LocalBoxFuture<'static, Result<JsValue>> {
        let decode = self.decode_args.clone();
        let encode = self.encode_result.clone();
        let args_schema = self.args.clone();
        let result_schema = self.result.clone();
        async move {
            let args = if args_schema.is_some() {
                decode(check_position(&args_schema, input, true)?)?
            } else {
                unit()?
            };
            let result = reference
                .call::<A, R>(&name, args, context.as_ref())
                .await?;
            if result_schema.is_some() {
                check_position(&result_schema, encode(result)?, false)
            } else {
                Ok(JsValue::UNDEFINED)
            }
        }
        .boxed_local()
    }
    fn call_registered(
        &self,
        operation: RegisteredOperation,
        input: JsValue,
        context: Context,
    ) -> LocalBoxFuture<'static, Result<JsValue>> {
        let decode = self.decode_args.clone();
        let encode = self.encode_result.clone();
        let args_schema = self.args.clone();
        let result_schema = self.result.clone();
        async move {
            let args = if args_schema.is_some() {
                decode(check_position(&args_schema, input, true)?)?
            } else {
                unit()?
            };
            let result = operation.call::<A, R>(&context, args).await?;
            if result_schema.is_some() {
                check_position(&result_schema, encode(result)?, false)
            } else {
                Ok(JsValue::UNDEFINED)
            }
        }
        .boxed_local()
    }
}

pub(crate) fn check_position(
    schema: &Option<ValueSchema>,
    value: JsValue,
    argument: bool,
) -> Result<JsValue> {
    if matches!(schema, Some(ValueSchema::Native(_))) && value.is_undefined() {
        return Err(error(
            "native_presence",
            "declared native position requires a present value",
        ));
    }
    let Some(ValueSchema::Portable(schema)) = schema else {
        return Ok(value);
    };
    // Capture descriptors before serialization can erase getters, symbols, holes or
    // nonportable numbers. The shared Rust profile remains the schema authority.
    let text = portable_json_owned(&value).map_err(js_error)?;
    let value = tembra_contracts::parse_json(&text).map_err(|e| error(&e.reason, &e.message))?;
    let value = if argument {
        tembra_contracts::clean_arguments(schema, &value).map_err(|e| error("args", &e.message))?
    } else {
        value
    };
    tembra_contracts::validate(schema, &value)
        .map_err(|e| error(if argument { "args" } else { "result" }, &e.message))?;
    let text = canonical_json(&value).map_err(|e| error(&e.reason, &e.message))?;
    parse_portable_owned(&text).map_err(js_error)
}

pub(crate) fn unit<T: 'static>() -> Result<T> {
    let value: Box<dyn Any> = Box::new(());
    value.downcast::<T>().map(|value| *value).map_err(|_| {
        error(
            "native_signature",
            "absent signature position requires the Rust unit type",
        )
    })
}

pub(crate) trait OpaqueAdapter {
    fn read(&self, reference: &Reference, name: &str) -> Result<JsValue>;
    fn install(
        &self,
        implementation: Implementation,
        name: String,
        resource: Resource<BrowserValue>,
    ) -> Implementation;
}
struct TypedOpaque<T> {
    encode: Encode<Rc<T>>,
    decode: Decode<Rc<T>>,
}
impl<T: 'static> OpaqueAdapter for TypedOpaque<T> {
    fn read(&self, reference: &Reference, name: &str) -> Result<JsValue> {
        (self.encode)(reference.opaque::<T>(name)?)
    }
    fn install(
        &self,
        implementation: Implementation,
        name: String,
        resource: Resource<BrowserValue>,
    ) -> Implementation {
        let decode = self.decode.clone();
        implementation.map_opaque(name, resource, move |value| decode(value.0.clone()))
    }
}

/// Explicit embedding-provided, whole-method typed adapters. No new erased core dispatch.
#[derive(Default)]
pub struct BrowserNativeSignatures {
    methods: RefCell<BTreeMap<String, Rc<dyn Signature>>>,
    opaque: RefCell<BTreeMap<String, Rc<dyn OpaqueAdapter>>>,
}
impl BrowserNativeSignatures {
    /// Registers one exact argument/result schema pair using existing monomorphized channels.
    ///
    /// Conversion closures are trusted embedding capabilities and must preserve the declared
    /// values/nominal identity. Portable-only methods use the ordinary validated JSON path.
    /// Absent argument/result positions require Rust `()` and never call their codecs. A
    /// provider return value for an absent result is discarded after actual settlement.
    /// Portable positions are captured as strict data before each codec/callback edge,
    /// cleaned without defaults for arguments, validated in Rust, and passed as detached data.
    /// Refusal leaves the previous registry unchanged and never invokes a conversion.
    ///
    /// # Errors
    /// Refuses invalid metadata, a non-unit absent position, a portable-only pair, duplicate
    /// registrations or capacity.
    pub fn register<A: 'static, R: 'static>(
        &self,
        args: Option<ValueSchema>,
        result: Option<ValueSchema>,
        encode_args: impl Fn(A) -> Result<JsValue> + 'static,
        decode_args: impl Fn(JsValue) -> Result<A> + 'static,
        encode_result: impl Fn(R) -> Result<JsValue> + 'static,
        decode_result: impl Fn(JsValue) -> Result<R> + 'static,
    ) -> Result<()> {
        self.register_codec(
            args,
            result,
            BrowserMethodCodec::new(encode_args, decode_args, encode_result, decode_result),
        )
    }
    /// Registers the same checked native signature with an explicit optional whole-method wrapper.
    /// # Errors
    /// Refuses invalid metadata, non-unit absent positions, duplicate pairs or capacity before
    /// invoking codecs/wrappers.
    pub fn register_codec<A: 'static, R: 'static>(
        &self,
        args: Option<ValueSchema>,
        result: Option<ValueSchema>,
        codec: BrowserMethodCodec<A, R>,
    ) -> Result<()> {
        let args_present = args.is_some();
        let result_present = result.is_some();
        if (!args_present && TypeId::of::<A>() != TypeId::of::<()>())
            || (!result_present && TypeId::of::<R>() != TypeId::of::<()>())
        {
            return Err(error(
                "native_signature",
                "absent signature position requires the Rust unit type",
            ));
        }
        let member = Member::Method { args, result };
        let contract = Contract::local(
            "browser.signature",
            1,
            serde_json::json!({}),
            BTreeMap::from([("call".into(), member.clone())]),
        )
        .map_err(|e| error(&e.reason, &e.message))?;
        let member = contract
            .members("")
            .and_then(|members| members.get("call"))
            .ok_or_else(|| error("native_signature", "checked signature is missing"))?;
        if !has_native(member) {
            return Err(error(
                "native_signature",
                "native adapter requires a nominal signature",
            ));
        }
        let key = signature_key(member)?;
        let Member::Method { args, result } = member else {
            return Err(error("native_signature", "checked member is not a method"));
        };
        let mut methods = self.methods.borrow_mut();
        if methods.contains_key(&key) || methods.len() >= 1024 {
            return Err(error(
                "native_signature",
                "duplicate or exhausted native signature registry",
            ));
        }
        methods.insert(
            key,
            Rc::new(Typed {
                args: args.clone(),
                result: result.clone(),
                encode_args: codec.scoped_args.unwrap_or_else(|| {
                    let encode = codec.encode_args;
                    Rc::new(move |value, _, _| encode(value))
                }),
                decode_args: codec.decode_args,
                encode_result: codec.encode_result,
                decode_result: codec.decode_result,
                wrapper: codec.wrapper,
            }),
        );
        Ok(())
    }
    /// Registers an explicit resource-backed opaque capability conversion.
    ///
    /// # Errors
    /// Refuses invalid nominal metadata, duplicate registration or capacity without conversion.
    pub fn register_opaque<T: 'static>(
        &self,
        nominal: &str,
        encode: impl Fn(Rc<T>) -> Result<JsValue> + 'static,
        decode: impl Fn(JsValue) -> Result<Rc<T>> + 'static,
    ) -> Result<()> {
        Contract::local(
            "browser.opaque",
            1,
            serde_json::json!({}),
            BTreeMap::from([("value".into(), Member::Opaque { id: nominal.into() })]),
        )
        .map_err(|e| error(&e.reason, &e.message))?;
        let mut opaque = self.opaque.borrow_mut();
        if opaque.contains_key(nominal) || opaque.len() >= 1024 {
            return Err(error(
                "native_signature",
                "duplicate or exhausted opaque registry",
            ));
        }
        opaque.insert(
            nominal.into(),
            Rc::new(TypedOpaque {
                encode: Rc::new(encode),
                decode: Rc::new(decode),
            }),
        );
        Ok(())
    }
    pub(crate) fn method(&self, member: &Member) -> Result<Rc<dyn Signature>> {
        self.methods
            .borrow()
            .get(&signature_key(member)?)
            .cloned()
            .ok_or_else(|| {
                error(
                    "native_signature",
                    "native signature pair has no registered adapter",
                )
            })
    }
    pub(crate) fn opaque(&self, id: &str) -> Result<Rc<dyn OpaqueAdapter>> {
        self.opaque.borrow().get(id).cloned().ok_or_else(|| {
            error(
                "native_signature",
                "opaque nominal type has no registered adapter",
            )
        })
    }
}
pub(crate) fn has_native(member: &Member) -> bool {
    matches!(
        member,
        Member::Method {
            args: Some(ValueSchema::Native(_)),
            ..
        } | Member::Method {
            result: Some(ValueSchema::Native(_)),
            ..
        }
    )
}
fn signature_key(member: &Member) -> Result<String> {
    let value = serde_json::to_value(member)
        .map_err(|_| error("native_signature", "invalid signature metadata"))?;
    canonical_json(&value).map_err(|e| error(&e.reason, &e.message))
}
