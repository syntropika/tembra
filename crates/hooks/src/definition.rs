use crate::{
    HookDescriptor, TerminalProfile,
    descriptor::{normalize, same},
    invalid,
};
use futures::FutureExt;
use serde::Serialize;
use serde_json::Value;
use std::{any::TypeId, marker::PhantomData, rc::Rc};
use tembra_contracts::{ValueSchema, to_portable_value, validate};
use tembra_core::{Declaration, DeclarationRecorder, RegisteredOperation, Result};

pub(crate) type Validator<T> = Rc<dyn Fn(&T) -> Result<()>>;
/// An explicit typed value profile. Native values never implicitly become JSON.
///
/// Encoders and validators must be deterministic and free of side effects. Checked values
/// may cross several trust boundaries (handler, continuation, terminal or foreign codec),
/// each of which can validate again; no exact invocation count is promised.
pub struct HookType<T> {
    pub(crate) profile: Option<ValueSchema>,
    pub(crate) validate: Validator<T>,
}
impl<T> Clone for HookType<T> {
    fn clone(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            validate: self.validate.clone(),
        }
    }
}
impl HookType<()> {
    pub fn absent() -> Self {
        Self {
            profile: None,
            validate: Rc::new(|_| Ok(())),
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
        let schema = schema.clone();
        Ok(Self {
            profile,
            validate: Rc::new(move |value| {
                let value = encode(value)?;
                validate(&schema, &value).map_err(|e| invalid("hook_value", e.message))
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
        Ok(Self {
            profile: normalize(Some(ValueSchema::Native(id.into())))?,
            validate: Rc::new(validate),
        })
    }
    pub fn profile(&self) -> &Option<ValueSchema> {
        &self.profile
    }
    pub(crate) fn check(&self, value: &T) -> Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.validate)(value)))
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
        if !same(descriptor.input(), &input.profile)
            || !same(descriptor.result(), &result.profile)
            || (input.profile.is_none() && TypeId::of::<A>() != TypeId::of::<()>())
            || (result.profile.is_none() && TypeId::of::<R>() != TypeId::of::<()>())
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
/// A typed terminal retaining only ordinary weak managed operation admission.
pub struct HookTerminal<A, R> {
    pub(crate) operation: RegisteredOperation,
    pub(crate) input: HookType<A>,
    pub(crate) result: HookType<R>,
    _types: PhantomData<fn(A) -> R>,
}
impl<A, R> Clone for HookTerminal<A, R> {
    fn clone(&self) -> Self {
        Self {
            operation: self.operation.clone(),
            input: self.input.clone(),
            result: self.result.clone(),
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
        let member = operation
            .contract()
            .members("")
            .and_then(|m| m.get(operation.method()));
        let matches_profile = match member {
            Some(tembra_contracts::Member::Method {
                args,
                result: output,
            }) => same(args, &input.profile) && same(output, &result.profile),
            _ => false,
        };
        if !matches_profile
            || (input.profile.is_none() && TypeId::of::<A>() != TypeId::of::<()>())
            || (result.profile.is_none() && TypeId::of::<R>() != TypeId::of::<()>())
        {
            return Err(invalid("hook_signature", "terminal profile differs"));
        }
        Ok(Self {
            operation,
            input,
            result,
            _types: PhantomData,
        })
    }
    pub(crate) fn profile(&self) -> TerminalProfile {
        TerminalProfile {
            input: self.input.profile.clone(),
            result: self.result.profile.clone(),
        }
    }
    pub(crate) async fn invoke(&self, input: A, context: &tembra_core::Context) -> Result<R> {
        self.input.check(&input)?;
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
