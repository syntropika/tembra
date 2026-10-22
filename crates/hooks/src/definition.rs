use crate::{
    HookDescriptor,
    descriptor::{ProfileEquality, TerminalEquality, normalize, same},
    invalid,
};
use futures::FutureExt;
use serde::Serialize;
use serde_json::Value;
use std::{any::TypeId, marker::PhantomData, rc::Rc};
use tembra_contracts::{PreparedSchema, ValueSchema, to_portable_value};
use tembra_core::{Declaration, DeclarationRecorder, RegisteredOperation, Result};

pub(crate) type Validator<T> = Rc<dyn Fn(&T) -> Result<()>>;
/// An explicit typed value profile. Native values never implicitly become JSON.
///
/// Encoders and validators must be deterministic and free of side effects. Checked values
/// may cross several trust boundaries (handler, continuation, terminal or foreign codec),
/// each of which can validate again; no exact invocation count is promised.
pub struct HookType<T> {
    metadata: Rc<TypeMetadata<T>>,
}

struct TypeMetadata<T> {
    profile: Option<ValueSchema>,
    equality: ProfileEquality,
    validate: Validator<T>,
}
impl<T> Clone for HookType<T> {
    fn clone(&self) -> Self {
        Self {
            metadata: self.metadata.clone(),
        }
    }
}
impl HookType<()> {
    pub fn absent() -> Self {
        Self {
            metadata: Rc::new(TypeMetadata {
                profile: None,
                equality: ProfileEquality::new(&None),
                validate: Rc::new(|_| Ok(())),
            }),
        }
    }
}
impl<T: 'static> HookType<T> {
    /// Stores explicit portable conversion and validates its output on each use.
    /// # Errors
    /// Refuses unsupported schemas; later validation can fail after encoder effects.
    pub fn portable(schema: Value, encode: impl Fn(&T) -> Result<Value> + 'static) -> Result<Self> {
        let profile = normalize(Some(ValueSchema::Portable(schema)))?;
        let Some(ValueSchema::Portable(schema)) = &profile else {
            unreachable!()
        };
        let prepared = PreparedSchema::new(schema);
        let equality = ProfileEquality::new(&profile);
        Ok(Self {
            metadata: Rc::new(TypeMetadata {
                profile,
                equality,
                validate: Rc::new(move |value| {
                    let value = encode(value)?;
                    match &prepared {
                        Ok(schema) => schema.validate(&value),
                        Err(error) => tembra_contracts::validate(&Value::Bool(true), &value)
                            .and_then(|()| Err(error.clone())),
                    }
                    .map_err(|e| invalid("hook_value", e.message))
                }),
            }),
        })
    }
    /// Uses checked Serde conversion, rejecting nonfinite values before validation.
    /// # Errors
    /// Refuses unsupported schemas and later serialization/validation failures.
    pub fn serde(schema: Value) -> Result<Self>
    where
        T: Serialize,
    {
        Self::portable(schema, |value| {
            to_portable_value(value).map_err(|e| invalid("hook_value", e.message))
        })
    }
    /// Stores an exact nominal marker and direct typed validator.
    /// # Errors
    /// Refuses invalid nominal spelling. User validator effects are not rolled back.
    pub fn native(
        id: impl Into<String>,
        validate: impl Fn(&T) -> Result<()> + 'static,
    ) -> Result<Self> {
        let profile = normalize(Some(ValueSchema::Native(id.into())))?;
        let equality = ProfileEquality::new(&profile);
        Ok(Self {
            metadata: Rc::new(TypeMetadata {
                profile,
                equality,
                validate: Rc::new(validate),
            }),
        })
    }
    pub fn profile(&self) -> &Option<ValueSchema> {
        &self.metadata.profile
    }
    pub(crate) fn check(&self, value: &T) -> Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.metadata.validate)(value)
        }))
        .unwrap_or_else(|_| Err(invalid("hook_panic", "hook value validator panicked")))
    }
}
/// One validated hook descriptor associated with real Rust input/payload types.
pub struct RustHookDefinition<A, R> {
    pub(crate) descriptor: HookDescriptor,
    pub(crate) input: HookType<A>,
    pub(crate) result: HookType<R>,
}
impl<A, R> Clone for RustHookDefinition<A, R> {
    fn clone(&self) -> Self {
        Self {
            descriptor: self.descriptor.clone(),
            input: self.input.clone(),
            result: self.result.clone(),
        }
    }
}
impl<A: 'static, R: 'static> RustHookDefinition<A, R> {
    /// Associates exact normalized profiles without acquiring any operation.
    /// # Errors
    /// Refuses metadata disagreement or a non-unit type at an absent position.
    pub fn from_descriptor(
        descriptor: HookDescriptor,
        input: HookType<A>,
        result: HookType<R>,
    ) -> Result<Self> {
        if !same(descriptor.input(), &input.metadata.profile)
            || !same(descriptor.result(), &result.metadata.profile)
            || (input.metadata.profile.is_none() && TypeId::of::<A>() != TypeId::of::<()>())
            || (result.metadata.profile.is_none() && TypeId::of::<R>() != TypeId::of::<()>())
        {
            return Err(invalid("hook_definition", "typed hook profiles differ"));
        }
        Ok(Self {
            descriptor,
            input,
            result,
        })
    }
    pub fn descriptor(&self) -> &HookDescriptor {
        &self.descriptor
    }
    /// Registers an ordinary owned method with strict contribution metadata.
    /// # Errors
    /// Refuses invalid priority and existing core declaration/owner failures.
    /// The manager validates the complete typed target profile before pipeline entry.
    pub fn register(
        &self,
        declaration: &mut Declaration,
        key: impl Into<String>,
        priority: i64,
        member: &str,
    ) -> Result<()> {
        let data = self.metadata(priority)?;
        declaration.register_method(self.descriptor.registry(), key, data, member)
    }
    /// Registers immediately through the existing exact-pass recorder.
    /// # Errors
    /// Refuses invalid priority or expired/declaration/owner admission.
    pub fn register_recorded(
        &self,
        declaration: &DeclarationRecorder,
        key: impl Into<String>,
        priority: i64,
        member: &str,
    ) -> Result<()> {
        declaration.register_method(
            self.descriptor.registry(),
            key,
            self.metadata(priority)?,
            member,
        )
    }
    fn metadata(&self, priority: i64) -> Result<Value> {
        if priority.unsigned_abs() > 9_007_199_254_740_991 {
            return Err(invalid(
                "hook_priority",
                "priority exceeds safe integer range",
            ));
        }
        Ok(serde_json::json!({
            "hook": self.descriptor.fingerprint(),
            "priority": priority,
            "profile": 1,
        }))
    }
}
type PortableInvocation<A, R> =
    Rc<dyn Fn(A, tembra_core::Context) -> futures::future::LocalBoxFuture<'static, Result<R>>>;

/// A typed terminal retaining only ordinary weak managed operation admission.
pub struct HookTerminal<A, R> {
    pub(crate) operation: RegisteredOperation,
    pub(crate) input: HookType<A>,
    pub(crate) result: HookType<R>,
    portable: Option<PortableInvocation<A, R>>,
    _types: PhantomData<fn(A) -> R>,
}
impl<A, R> Clone for HookTerminal<A, R> {
    fn clone(&self) -> Self {
        Self {
            operation: self.operation.clone(),
            input: self.input.clone(),
            result: self.result.clone(),
            portable: self.portable.clone(),
            _types: PhantomData,
        }
    }
}
impl<A: 'static, R: 'static> HookTerminal<A, R> {
    /// Checks an actual managed member profile; dispatch remains Context/owner checked.
    /// # Errors
    /// Refuses incompatible positions, absent member and non-unit absent types.
    pub fn new(
        operation: RegisteredOperation,
        input: HookType<A>,
        result: HookType<R>,
    ) -> Result<Self> {
        let matches_profile = match (
            input.metadata.equality.key(),
            result.metadata.equality.key(),
        ) {
            (Some(args), Some(output)) => {
                operation
                    .contract()
                    .method_profiles_match("", operation.method(), args, output)
            }
            // Preserve the previous `.ok()` equality on deferred canonical-error paths.
            _ => match operation
                .contract()
                .members("")
                .and_then(|m| m.get(operation.method()))
            {
                Some(tembra_contracts::Member::Method {
                    args,
                    result: output,
                }) => same(args, input.profile()) && same(output, result.profile()),
                _ => false,
            },
        };
        if !matches_profile
            || (input.metadata.profile.is_none() && TypeId::of::<A>() != TypeId::of::<()>())
            || (result.metadata.profile.is_none() && TypeId::of::<R>() != TypeId::of::<()>())
        {
            return Err(invalid("hook_signature", "terminal profile differs"));
        }
        Ok(Self {
            operation,
            input,
            result,
            portable: None,
            _types: PhantomData,
        })
    }
    /// Adapts a portable managed operation with explicit presence-preserving conversions.
    /// The operation keeps its actual owner, epoch and validated JSON dispatch channel.
    /// # Errors
    /// Refuses nominal profiles, mismatched metadata and absent non-unit positions.
    pub fn portable(
        operation: RegisteredOperation,
        input: HookType<A>,
        result: HookType<R>,
        encode: impl Fn(A) -> Result<Option<Value>> + 'static,
        decode: impl Fn(Option<Value>) -> Result<R> + 'static,
    ) -> Result<Self> {
        if [&input.metadata.profile, &result.metadata.profile]
            .into_iter()
            .flatten()
            .any(|p| matches!(p, ValueSchema::Native(_)))
        {
            return Err(invalid(
                "hook_signature",
                "portable terminal requires portable profiles",
            ));
        }
        let mut terminal = Self::new(operation.clone(), input, result)?;
        let encode = Rc::new(encode);
        let decode = Rc::new(decode);
        terminal.portable = Some(Rc::new(move |input, context| {
            let operation = operation.clone();
            let encode = encode.clone();
            let decode = decode.clone();
            async move {
                let input =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode(input)))
                        .unwrap_or_else(|_| {
                            Err(invalid(
                                "hook_panic",
                                "portable terminal input conversion panicked",
                            ))
                        })?;
                let call = operation.call_json_tracked(&context, input);
                let receipt = call.receipt();
                let result = std::panic::AssertUnwindSafe(call)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(invalid("hook_panic", "hook terminal panicked")));
                receipt.settled().await;
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(result?)))
                    .unwrap_or_else(|_| {
                        Err(invalid(
                            "hook_panic",
                            "portable terminal output conversion panicked",
                        ))
                    })
            }
            .boxed_local()
        }));
        Ok(terminal)
    }
    pub(crate) fn check(&self, context: &tembra_core::Context) -> Result<()> {
        if self.portable.is_some() {
            self.operation.check_portable(context)
        } else {
            self.operation.check_native::<A, R>(context)
        }
    }
    pub(crate) fn matches_profile(&self, expected: &TerminalEquality) -> bool {
        expected.matches(
            &self.input.metadata.equality,
            &self.result.metadata.equality,
        )
    }
    pub(crate) async fn invoke(&self, input: A, context: &tembra_core::Context) -> Result<R> {
        self.input.check(&input)?;
        if let Some(invoke) = &self.portable {
            let value = invoke(input, context.clone()).await?;
            self.result.check(&value)?;
            return Ok(value);
        }
        let call = self.operation.call_tracked::<A, R>(context, input);
        let receipt = call.receipt();
        let result = std::panic::AssertUnwindSafe(call)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(invalid("hook_panic", "hook terminal panicked")));
        receipt.settled().await;
        let value = result?;
        self.result.check(&value)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use serde_json::json;

    #[test]
    fn retained_type_equality_does_not_cache_value_acceptance_or_mutable_source() {
        let mut source = json!({"type":"integer", "minimum":0});
        let owner = HookType::<i32>::serde(source.clone()).unwrap();
        let retained = owner.clone();
        source["minimum"] = json!(10);
        let replacement = HookType::<i32>::serde(source).unwrap();
        assert!(owner.metadata.equality == retained.metadata.equality);
        assert!(owner.metadata.equality != replacement.metadata.equality);
        drop(owner);
        retained.check(&3).unwrap();
        assert_eq!(retained.check(&-1).unwrap_err().reason, "hook_value");
        retained.check(&4).unwrap();
        assert_eq!(replacement.check(&3).unwrap_err().reason, "hook_value");
        let native = HookType::<i32>::native("bounded@1", |value| {
            if *value >= 0 {
                Ok(())
            } else {
                Err(invalid("refused", "negative value"))
            }
        })
        .unwrap();
        let copied = native.clone();
        copied.check(&1).unwrap();
        assert_eq!(copied.check(&-1).unwrap_err().reason, "refused");
        assert!(HookType::<i32>::native("", |_| Ok(())).is_err());
    }

    #[test]
    fn deferred_schema_budget_refusal_follows_each_encoder_and_portable_value_check() {
        let properties = (0..9_000)
            .map(|index| (format!("a{index}"), json!({"type":"array"})))
            .collect::<serde_json::Map<_, _>>();
        let source = json!({"type":"object", "properties":properties});
        let normalized = tembra_contracts::normalize_schema(&source).unwrap();
        let expected = tembra_contracts::validate(&normalized, &json!({})).unwrap_err();
        assert_eq!(expected.reason, "schema_budget");
        let calls = Rc::new(Cell::new(0));
        let encoded = calls.clone();
        let profile = HookType::portable(source, move |value: &Value| {
            encoded.set(encoded.get() + 1);
            if value == &json!("encoder-error") {
                return Err(invalid("encoder", "conversion refused"));
            }
            Ok(value.clone())
        })
        .unwrap();
        assert_eq!(calls.get(), 0);
        for value in [json!({}), json!(9_007_199_254_740_992_u64)] {
            let expected = tembra_contracts::validate(&normalized, &value).unwrap_err();
            let error = profile.check(&value).unwrap_err();
            assert_eq!(error.reason, "hook_value");
            assert_eq!(error.message, expected.message);
        }
        assert_eq!(
            profile.check(&json!("encoder-error")).unwrap_err().reason,
            "encoder"
        );
        assert_eq!(calls.get(), 3);
    }
}
