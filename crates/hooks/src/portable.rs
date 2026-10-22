//! Generic portable hook admission for language bindings. Composition stays in HookManager.
use crate::*;
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra_contracts::ValueSchema;
use tembra_core::{Context, RegisteredOperation, Result};

/// Presence-preserving portable carrier shared by all language bindings.
/// An absent position uses actual Rust unit, never JSON null.
pub trait PortableValue: Clone + 'static {
    /// # Errors
    /// Refuses values that cannot cross the portable boundary.
    fn encode(self) -> Result<Option<Value>>;
    /// # Errors
    /// Refuses incorrect presence or payload shape.
    fn decode(value: Option<Value>) -> Result<Self>;
    /// # Errors
    /// Refuses a profile inconsistent with this carrier.
    fn profile(schema: Option<ValueSchema>) -> Result<HookType<Self>>;
}
impl PortableValue for () {
    fn encode(self) -> Result<Option<Value>> {
        Ok(None)
    }
    fn decode(value: Option<Value>) -> Result<Self> {
        if value.is_none() {
            Ok(())
        } else {
            Err(invalid("hook_value", "absent position requires absence"))
        }
    }
    fn profile(schema: Option<ValueSchema>) -> Result<HookType<Self>> {
        if schema.is_none() {
            Ok(HookType::absent())
        } else {
            Err(invalid("hook_signature", "expected absent profile"))
        }
    }
}
impl PortableValue for Value {
    fn encode(self) -> Result<Option<Value>> {
        Ok(Some(self))
    }
    fn decode(value: Option<Value>) -> Result<Self> {
        value.ok_or_else(|| invalid("hook_value", "expected present portable value"))
    }
    fn profile(schema: Option<ValueSchema>) -> Result<HookType<Self>> {
        match schema {
            Some(ValueSchema::Portable(schema)) => HookType::serde(schema),
            _ => Err(invalid("hook_signature", "expected portable profile")),
        }
    }
}
impl<R: PortableValue> PortableValue for BailDecision<R> {
    fn encode(self) -> Result<Option<Value>> {
        Ok(Some(match self {
            Self::Continue => json!({"kind":"continue"}),
            Self::Stop(value) => match value.encode()? {
                Some(value) => json!({"kind":"stop","value":value}),
                None => json!({"kind":"stop"}),
            },
        }))
    }
    fn decode(value: Option<Value>) -> Result<Self> {
        let value = Value::decode(value)?;
        match value.get("kind").and_then(Value::as_str) {
            Some("continue") if value.as_object().is_some_and(|o| o.len() == 1) => {
                Ok(Self::Continue)
            }
            Some("stop")
                if value.as_object().is_some_and(|o| {
                    o.len() == if value.get("value").is_some() { 2 } else { 1 }
                }) =>
            {
                Ok(Self::Stop(R::decode(value.get("value").cloned())?))
            }
            _ => Err(invalid("hook_value", "invalid bail decision")),
        }
    }
    fn profile(_: Option<ValueSchema>) -> Result<HookType<Self>> {
        Err(invalid(
            "hook_signature",
            "bail decisions use the checked hook descriptor",
        ))
    }
}

/// A binding installs this exact monomorphized contribution channel and trusted hook wrapper.
/// This trait does not run hooks or turn nominal application types into JSON.
pub trait PortableHookAdapter {
    /// # Errors
    /// Refuses unsupported, duplicate or exhausted binding adapter registrations.
    fn register<A: PortableValue, H: PortableValue>(
        &self,
        descriptor: &HookDescriptor,
    ) -> Result<()>;
}
type Run = Rc<
    dyn Fn(
        HookManager,
        Option<Value>,
        Context,
        Option<RegisteredOperation>,
    ) -> LocalBoxFuture<'static, Result<Option<Value>>>,
>;
/// Validated portable hook and its generic binding adapter. Owns no Host or reference claim.
#[derive(Clone)]
pub struct PortableHook {
    run: Run,
}
impl PortableHook {
    /// Validates all profiles and installs one exact typed contribution signature.
    /// # Errors
    /// Refuses native application profiles or binding registration failure before any handler runs.
    pub fn register(
        descriptor: HookDescriptor,
        adapter: &impl PortableHookAdapter,
    ) -> Result<Self> {
        for schema in [descriptor.input(), descriptor.result()]
            .into_iter()
            .chain(
                descriptor
                    .terminal()
                    .into_iter()
                    .flat_map(|p| [&p.input, &p.result]),
            )
            .flatten()
        {
            if matches!(schema, ValueSchema::Native(_)) {
                return Err(invalid(
                    "hook_signature",
                    "native payloads require an explicit Rust adapter",
                ));
            }
        }
        match (descriptor.input().is_some(), descriptor.result().is_some()) {
            (false, false) => build::<(), ()>(descriptor, adapter),
            (false, true) => build::<(), Value>(descriptor, adapter),
            (true, false) => build::<Value, ()>(descriptor, adapter),
            (true, true) => build::<Value, Value>(descriptor, adapter),
        }
    }
    /// Runs through the existing manager with a live managed Context and optional managed terminal.
    /// # Errors
    /// Reports manager admission, schema, cancellation, handler and continuation errors unchanged.
    pub fn run(
        &self,
        manager: HookManager,
        input: Option<Value>,
        context: Context,
        terminal: Option<RegisteredOperation>,
    ) -> LocalBoxFuture<'static, Result<Option<Value>>> {
        (self.run)(manager, input, context, terminal)
    }
}
fn terminal<A: Clone + 'static, R: PortableValue>(
    operation: RegisteredOperation,
    input: HookType<A>,
    output: Option<ValueSchema>,
    encode: impl Fn(A) -> Result<Option<Value>> + 'static,
) -> Result<HookTerminal<A, R>> {
    HookTerminal::portable(operation, input, R::profile(output)?, encode, R::decode)
}
fn build<A: PortableValue, R: PortableValue>(
    descriptor: HookDescriptor,
    adapter: &impl PortableHookAdapter,
) -> Result<PortableHook> {
    match descriptor.strategy() {
        Strategy::Series => adapter.register::<A, ()>(&descriptor)?,
        Strategy::Bail => adapter.register::<A, BailDecision<R>>(&descriptor)?,
        Strategy::Waterfall => adapter.register::<A, A>(&descriptor)?,
        _ => adapter.register::<A, R>(&descriptor)?,
    }
    Ok(PortableHook {
        run: Rc::new(move |manager, input, context, operation| {
            let d = descriptor.clone();
            async move {
                let input = A::decode(input)?;
                let a = A::profile(d.input().clone())?;
                let r = R::profile(d.result().clone())?;
                let output = d.terminal().map(|p| p.result.clone()).unwrap_or(None);
                // Terminal presence is checked before launching, including an empty contribution plan.
                if d.terminal().is_some() != operation.is_some() {
                    return Err(invalid("hook_signature", "terminal presence differs"));
                }
                match d.strategy() {
                    Strategy::Series => {
                        let definition =
                            RustHookDefinition::from_descriptor(d, a.clone(), HookType::absent())?;
                        if let Some(op) = operation {
                            if output.is_some() {
                                manager
                                    .run_dynamic_series_then(
                                        &definition,
                                        input,
                                        &context,
                                        terminal::<A, Value>(op, a, output, A::encode)?,
                                    )
                                    .await?
                                    .encode()
                            } else {
                                manager
                                    .run_dynamic_series_then(
                                        &definition,
                                        input,
                                        &context,
                                        terminal::<A, ()>(op, a, output, A::encode)?,
                                    )
                                    .await?
                                    .encode()
                            }
                        } else {
                            manager
                                .run_dynamic_series(&definition, input, &context)
                                .await?
                                .encode()
                        }
                    }
                    Strategy::Bail => {
                        let definition = RustHookDefinition::from_descriptor(d, a.clone(), r)?;
                        let terminal = operation
                            .map(|op| terminal::<A, R>(op, a, output, A::encode))
                            .transpose()?;
                        Ok(Some(
                            match manager
                                .run_dynamic_bail(&definition, input, &context, terminal)
                                .await?
                            {
                                BailOutcome::Unresolved => json!({"kind":"unresolved"}),
                                BailOutcome::Resolved(value) => match value.encode()? {
                                    Some(value) => json!({"kind":"resolved","value":value}),
                                    None => json!({"kind":"resolved"}),
                                },
                            },
                        ))
                    }
                    Strategy::Waterfall => {
                        let result = A::profile(d.result().clone())?;
                        let definition = RustHookDefinition::from_descriptor(d, a.clone(), result)?;
                        if let Some(op) = operation {
                            if output.is_some() {
                                manager
                                    .run_dynamic_waterfall_then(
                                        &definition,
                                        input,
                                        &context,
                                        terminal::<A, Value>(op, a, output, A::encode)?,
                                    )
                                    .await?
                                    .encode()
                            } else {
                                manager
                                    .run_dynamic_waterfall_then(
                                        &definition,
                                        input,
                                        &context,
                                        terminal::<A, ()>(op, a, output, A::encode)?,
                                    )
                                    .await?
                                    .encode()
                            }
                        } else {
                            manager
                                .run_dynamic_waterfall(&definition, input, &context)
                                .await?
                                .encode()
                        }
                    }
                    Strategy::Middleware => {
                        let definition = RustHookDefinition::from_descriptor(d, a.clone(), r)?;
                        let op = operation.ok_or_else(|| {
                            invalid("hook_signature", "middleware requires terminal")
                        })?;
                        manager
                            .run_dynamic_middleware(
                                &definition,
                                input,
                                &context,
                                terminal::<A, R>(op, a, output, A::encode)?,
                            )
                            .await?
                            .encode()
                    }
                    Strategy::Parallel => {
                        let profile = d.terminal().map(|p| p.input.clone()).unwrap_or(None);
                        let definition = RustHookDefinition::from_descriptor(d, a, r)?;
                        let encode = |values: Vec<R>| -> Result<Option<Value>> {
                            Ok(Some(Value::Array(
                                values
                                    .into_iter()
                                    .map(|v| v.encode().map(|v| v.unwrap_or(Value::Null)))
                                    .collect::<Result<_>>()?,
                            )))
                        };
                        if let Some(op) = operation {
                            let Some(ValueSchema::Portable(schema)) = profile else {
                                return Err(invalid(
                                    "hook_signature",
                                    "parallel terminal requires portable vector",
                                ));
                            };
                            let vector = HookType::portable(schema, move |values: &Vec<R>| {
                                encode(values.clone())?
                                    .ok_or_else(|| invalid("hook_value", "missing vector"))
                            })?;
                            if output.is_some() {
                                manager
                                    .run_dynamic_parallel_then(
                                        &definition,
                                        input,
                                        &context,
                                        terminal::<Vec<R>, Value>(op, vector, output, encode)?,
                                    )
                                    .await?
                                    .encode()
                            } else {
                                manager
                                    .run_dynamic_parallel_then(
                                        &definition,
                                        input,
                                        &context,
                                        terminal::<Vec<R>, ()>(op, vector, output, encode)?,
                                    )
                                    .await?
                                    .encode()
                            }
                        } else {
                            encode(
                                manager
                                    .run_dynamic_parallel(&definition, input, &context)
                                    .await?,
                            )
                        }
                    }
                }
            }
            .boxed_local()
        }),
    })
}
