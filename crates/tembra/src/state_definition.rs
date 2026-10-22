//! Immutable typed definitions owned by actual facet registration.
use crate::{
    Result, StateDeclaration, core,
    state::{contract_error, encode},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{marker::PhantomData, rc::Rc};

pub(crate) struct Metadata {
    name: Rc<str>,
    schema: Rc<Value>,
    value_validator: tembra_contracts::Result<tembra_contracts::PreparedSchema>,
    core: core::StateDefinition,
}
impl Metadata {
    pub(crate) fn prepare(name: String, generated: Value) -> Result<Self> {
        if name.is_empty() {
            return Err(crate::RuntimeError::invalid(
                "state_definition",
                "state names must be nonempty",
            ));
        }
        let schema = tembra_contracts::normalize_schema(&generated).map_err(contract_error)?;
        let value_validator = tembra_contracts::PreparedSchema::new(&schema);
        let core = core::StateDefinition::new(schema.clone())?;
        Ok(Self {
            name: name.into(),
            schema: Rc::new(schema),
            value_validator,
            core,
        })
    }
}
/// Shared immutable schema metadata; it retains no cells, values, Host, Context or permission.
/// Definitions can only be prepared through actual [`crate::FacetDeclaration`] setup authority.
pub struct StateDefinition<T> {
    metadata: Rc<Metadata>,
    marker: PhantomData<fn() -> T>,
}
impl<T> Clone for StateDefinition<T> {
    fn clone(&self) -> Self {
        Self {
            metadata: self.metadata.clone(),
            marker: PhantomData,
        }
    }
}
impl<T> StateDefinition<T> {
    pub(crate) fn from_metadata(metadata: Rc<Metadata>) -> Self {
        Self {
            metadata,
            marker: PhantomData,
        }
    }
    /// Returns immutable normalized schema metadata, never an execution capability.
    pub fn schema(&self) -> Rc<Value> {
        self.metadata.schema.clone()
    }
    pub(crate) fn core_definition(&self) -> core::StateDefinition {
        self.metadata.core.clone()
    }
}
impl<T: Serialize + DeserializeOwned + 'static> StateDefinition<T> {
    /// Encodes and validates a fresh initial value without creating a cell.
    /// # Errors
    /// Preserves per-value conversion/profile failures and portable-value precedence before
    /// deferred schema-budget failures. No declaration escapes on error.
    pub fn value(&self, initial: T) -> Result<StateDeclaration<T>> {
        let value = encode(&initial)?;
        match &self.metadata.value_validator {
            Ok(prepared) => prepared.validate(&value).map_err(contract_error)?,
            Err(error) => {
                tembra_contracts::validate(&Value::Bool(true), &value).map_err(contract_error)?;
                return Err(contract_error(error.clone()));
            }
        }
        Ok(StateDeclaration::from_prepared_core(
            self.metadata.name.clone(),
            core::StateDeclaration::from_definition(self.metadata.core.clone(), value),
        ))
    }
    /// Records a fresh initializer without executing it or performing fallible preparation.
    /// Initializer, conversion and validation failures are reported by managed activation.
    pub fn with_initializer<F, Fut>(&self, initialize: F) -> StateDeclaration<T>
    where
        F: Fn(crate::Context) -> Fut + 'static,
        Fut: std::future::Future<Output = Result<T>> + 'static,
    {
        StateDeclaration::from_prepared_core(
            self.metadata.name.clone(),
            core::StateDeclaration::from_definition_initializer(
                self.metadata.core.clone(),
                move |context| {
                    let future = initialize(context.into());
                    Box::pin(async move { encode(&future.await?) })
                },
            ),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use std::{
        borrow::Cow,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static PREPARATIONS: AtomicUsize = AtomicUsize::new(0);
    #[derive(Serialize, Deserialize)]
    struct Counted(u64);
    impl JsonSchema for Counted {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("Counted")
        }
        fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
            PREPARATIONS.fetch_add(1, Ordering::SeqCst);
            schemars::json_schema!({"type":"integer","minimum":0,"maximum":1023})
        }
    }
    #[test]
    fn registration_prepares_once_and_refuses_portable_schema_violations() {
        use crate::{Host, NativeFacet, core};
        PREPARATIONS.store(0, Ordering::SeqCst);
        let driver = core::ManualDriver::new();
        let host = Host::from_core(core::Host::new(
            core::HostOptions::new("definitions", serde_json::json!(null)),
            driver.clone(),
        ));
        let saved = Rc::new(std::cell::RefCell::new(None));
        let observed = saved.clone();
        let startup = host
            .install(
                NativeFacet::new("definitions", vec![], move |_, setup| {
                    *observed.borrow_mut() = Some(setup.state::<Counted>("value")?);
                    Ok(())
                })
                .unwrap(),
                core::LoadOptions::default(),
            )
            .unwrap();
        driver.run_until(startup).unwrap();
        let definition = saved.borrow_mut().take().unwrap();
        for value in 0..1024 {
            definition.value(Counted(value)).unwrap();
        }
        assert_eq!(PREPARATIONS.load(Ordering::SeqCst), 1);
        let error = definition.value(Counted(1024)).err().unwrap();
        assert_eq!(error.reason, "validation");
        assert!(error.message.contains("maximum"));
        assert!(definition.value(Counted(u64::MAX)).is_err());
        assert!(definition.value(Counted(1023)).is_ok());
        assert_eq!(PREPARATIONS.load(Ordering::SeqCst), 1);
        driver.run_until(host.close()).unwrap();
    }
}

#[cfg(test)]
mod instance_names {
    use crate::{Cell, Context, Host, NativeFacet, Readable, Result, core};

    #[crate::contract(local, name = "definition.numeric", version = 1)]
    trait Numeric {
        #[state]
        fn progress(&self) -> u64;
        async fn increment(&self) -> Result<()>;
    }
    #[crate::contract(local, name = "definition.textual", version = 1)]
    trait Textual {
        #[state]
        fn progress(&self) -> String;
    }
    struct NumericState(Cell<u64>);
    struct TextualState(Cell<String>);
    impl NumericProvider for NumericState {
        fn progress(&self) -> Readable<u64> {
            self.0.readable()
        }
        async fn increment(&self, context: Context) -> Result<()> {
            self.0.change(&context, |value| {
                *value += 1;
                Ok(())
            })?;
            Ok(())
        }
    }
    impl TextualProvider for TextualState {
        fn progress(&self) -> Readable<String> {
            self.0.readable()
        }
    }
    #[test]
    fn independent_providers_keep_local_names_and_values() -> Result<()> {
        let driver = core::ManualDriver::new();
        let host = Host::with_driver("definition-names", (), driver.clone())?;
        let facet = NativeFacet::new("providers", vec![], |_, setup| {
            let numeric = setup.state::<u64>("progress")?;
            let textual = setup.state::<String>("progress")?;
            setup.serve(Numeric, move |_, _, declaration| {
                Ok(NumericState(declaration.state(numeric.value(1)?)?))
            })?;
            setup.serve(Textual, move |_, _, declaration| {
                Ok(TextualState(
                    declaration.state(textual.value("separate".into())?)?,
                ))
            })
        })?;
        driver.run_until(async {
            host.install(facet, Default::default())?.await?;
            let numeric = host.get(Numeric, NumericInput {}).await?;
            let textual = host.get(Textual, TextualInput {}).await?;
            assert_eq!(numeric.progress().value()?, 1);
            assert_eq!(textual.progress().value()?, "separate");
            numeric.increment().await?;
            assert_eq!(numeric.progress().value()?, 2);
            assert_eq!(textual.progress().value()?, "separate");
            host.close().await
        })
    }
}
