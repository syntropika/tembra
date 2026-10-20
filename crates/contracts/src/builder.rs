use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    Contract, ContractError, ContractKind, Descriptor, Member, Placement, Result, ValueSchema,
    normalize_schema,
};

/// Generate source data from Rust, adapting only recognized Schemars numeric formats.
///
/// Known integer storage formats become bounds intersected with the portable safe
/// integer domain. Float/double use the global portable number domain, without an
/// exact f32 precision promise. Source annotations are retained.
///
/// # Errors
/// Rejects unknown formats, recursive/unsupported generated schemas, and profile
/// violations. No native value is serialized or registered by schema generation.
pub fn schema_for<T: JsonSchema + ?Sized>() -> Result<Value> {
    let mut source = schemars::schema_for!(T).to_value();
    adapt_numeric_formats(&mut source)?;
    normalize_schema(&source)
}

fn adapt_numeric_formats(schema: &mut Value) -> Result<()> {
    let Some(object) = schema.as_object_mut() else {
        return Ok(());
    };
    if let Some(format) = object
        .get("format")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let kind = object.get("type").and_then(|kind| {
            if let Some(kind) = kind.as_str() {
                return Some(kind);
            }
            let kinds = kind.as_array()?;
            if kinds.len() != 2 || !kinds.iter().any(|kind| kind.as_str() == Some("null")) {
                return None;
            }
            kinds
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| *kind != "null")
        });
        let bounds = match (kind, format.as_str()) {
            (Some("integer"), "int8") => Some((-128_i64, 127_i64)),
            (Some("integer"), "int16") => Some((-32768, 32767)),
            (Some("integer"), "int32") => Some((-2147483648, 2147483647)),
            (Some("integer"), "int64" | "int128" | "int") => {
                Some((-9_007_199_254_740_991, 9_007_199_254_740_991))
            }
            (Some("integer"), "uint8") => Some((0, 255)),
            (Some("integer"), "uint16") => Some((0, 65535)),
            (Some("integer"), "uint32") => Some((0, 4294967295)),
            (Some("integer"), "uint64" | "uint128" | "uint") => Some((0, 9_007_199_254_740_991)),
            // The global finite/safe-integral transport domain is narrower than either
            // storage type's range. This does not claim exact f32 decimal precision.
            (Some("number"), "float" | "double") => None,
            _ => {
                return Err(ContractError::new(
                    "unsupported_format",
                    format!("unsupported Schemars format {format}"),
                ));
            }
        };
        object.remove("format");
        if let Some((minimum, maximum)) = bounds {
            for keyword in ["minimum", "maximum"] {
                if object.get(keyword).is_some_and(|value| !value.is_number()) {
                    return Err(ContractError::new(
                        "schema",
                        format!("{keyword} must be numeric before format adaptation"),
                    ));
                }
            }
            let existing_minimum = object
                .get("minimum")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NEG_INFINITY);
            let existing_maximum = object
                .get("maximum")
                .and_then(Value::as_f64)
                .unwrap_or(f64::INFINITY);
            object.insert(
                "minimum".into(),
                json!(existing_minimum.max(minimum as f64)),
            );
            object.insert(
                "maximum".into(),
                json!(existing_maximum.min(maximum as f64)),
            );
        }
    }
    for key in ["properties", "$defs"] {
        if let Some(properties) = object.get_mut(key).and_then(Value::as_object_mut) {
            for schema in properties.values_mut() {
                adapt_numeric_formats(schema)?;
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        adapt_numeric_formats(items)?;
    }
    if let Some(branches) = object.get_mut("anyOf").and_then(Value::as_array_mut) {
        for branch in branches {
            adapt_numeric_formats(branch)?;
        }
    }
    Ok(())
}

/// Typed declarations include member metadata, variants and placement, not only DTO schemas.
///
/// The builder owns unchecked authoring data until [`Self::build`]. Duplicate
/// members/variants record a refusal; build cannot expose the overwritten draft.
/// No object/provider is registered as a side effect.
///
/// ```
/// use tembra_contracts::ContractBuilder;
/// let contract = ContractBuilder::local("example.counter", 1)
///     .typed_query::<u32>("read")?
///     .method("reset", None, None)
///     .build()?;
/// assert_eq!(contract.members("").unwrap().len(), 2);
/// assert!(ContractBuilder::local("duplicate", 1)
///     .method("reset", None, None).method("reset", None, None).build().is_err());
/// # Ok::<(), tembra_contracts::ContractError>(())
/// ```
pub struct ContractBuilder {
    descriptor: Descriptor,
    variant: String,
    error: Option<ContractError>,
}

impl ContractBuilder {
    pub fn local(name: impl Into<String>, version: u64) -> Self {
        Self::new(name.into(), version, ContractKind::Local)
    }
    pub fn remote(name: impl Into<String>, version: u64) -> Self {
        Self::new(name.into(), version, ContractKind::Remote)
    }

    fn new(name: String, version: u64, kind: ContractKind) -> Self {
        let variant = if kind == ContractKind::Local {
            String::new()
        } else {
            "main".into()
        };
        let variants = if kind == ContractKind::Local {
            BTreeMap::from([(variant.clone(), BTreeMap::new())])
        } else {
            BTreeMap::new()
        };
        Self {
            descriptor: Descriptor {
                descriptor_version: 1,
                name,
                version,
                kind,
                input: json!({"type":"object"}),
                placement: None,
                variants,
                fingerprint: String::new(),
            },
            variant,
            error: None,
        }
    }

    /// Replaces the draft acquisition schema with metadata generated from `T`.
    ///
    /// # Errors
    /// Returns [`schema_for`] failures for unsupported or nonportable generated schemas.
    pub fn input<T: JsonSchema + ?Sized>(mut self) -> Result<Self> {
        self.descriptor.input = schema_for::<T>()?;
        Ok(self)
    }
    pub fn input_schema(mut self, schema: Value) -> Self {
        self.descriptor.input = schema;
        self
    }
    pub fn placement(mut self, placement: Placement) -> Self {
        self.descriptor.placement = Some(placement);
        self
    }

    /// Select a named variant for subsequent member declarations.
    pub fn select_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = variant.into();
        self.descriptor
            .variants
            .entry(self.variant.clone())
            .or_default();
        self
    }

    pub fn variant(mut self, name: impl Into<String>, members: BTreeMap<String, Member>) -> Self {
        let name = name.into();
        if self.descriptor.variants.insert(name, members).is_some() {
            self.error = Some(ContractError::new("duplicate", "variant already declared"));
        }
        self
    }

    pub fn method(
        self,
        name: impl Into<String>,
        args: Option<ValueSchema>,
        result: Option<ValueSchema>,
    ) -> Self {
        self.member(name.into(), Member::Method { args, result })
    }
    pub fn state(self, name: impl Into<String>, schema: Value) -> Self {
        self.member(name.into(), Member::State { schema })
    }
    pub fn opaque(self, name: impl Into<String>, id: impl Into<String>) -> Self {
        self.member(name.into(), Member::Opaque { id: id.into() })
    }

    /// Adds a method with present arguments and results generated from Rust types.
    ///
    /// # Errors
    /// Returns [`schema_for`] failures for either signature. Duplicate names are recorded
    /// for [`Self::build`] to reject; no runtime registration occurs.
    pub fn typed_method<A: JsonSchema + ?Sized, R: JsonSchema + ?Sized>(
        self,
        name: impl Into<String>,
    ) -> Result<Self> {
        Ok(self.method(
            name,
            Some(ValueSchema::Portable(schema_for::<A>()?)),
            Some(ValueSchema::Portable(schema_for::<R>()?)),
        ))
    }
    /// Adds a method with typed arguments and an absent result.
    ///
    /// # Errors
    /// Returns [`schema_for`] failures for the argument type.
    pub fn typed_command<A: JsonSchema + ?Sized>(self, name: impl Into<String>) -> Result<Self> {
        Ok(self.method(name, Some(ValueSchema::Portable(schema_for::<A>()?)), None))
    }
    /// Adds a method with absent arguments and a typed result.
    ///
    /// # Errors
    /// Returns [`schema_for`] failures for the result type.
    pub fn typed_query<R: JsonSchema + ?Sized>(self, name: impl Into<String>) -> Result<Self> {
        Ok(self.method(name, None, Some(ValueSchema::Portable(schema_for::<R>()?))))
    }
    /// Adds a state member whose schema is generated from `T`.
    ///
    /// # Errors
    /// Returns [`schema_for`] failures for the state type.
    pub fn typed_state<T: JsonSchema + ?Sized>(self, name: impl Into<String>) -> Result<Self> {
        Ok(self.state(name, schema_for::<T>()?))
    }

    /// Serializes and normalizes an input value against this draft's acquisition schema.
    ///
    /// This consumes the draft and returns a value, without building or registering a contract.
    ///
    /// # Errors
    /// Returns `serialization` if Serde conversion fails, or [`crate::normalize_input`]
    /// errors for unsupported schemas, nonportable values or invalid normalized input.
    pub fn default_input<T: Serialize>(self, value: &T) -> Result<Value> {
        let value = crate::to_portable_value(value)?;
        crate::normalize_input(&self.descriptor.input, &value)
    }

    fn member(mut self, name: String, member: Member) -> Self {
        if self
            .descriptor
            .variants
            .entry(self.variant.clone())
            .or_default()
            .insert(name, member)
            .is_some()
        {
            self.error = Some(ContractError::new(
                "duplicate",
                "member already declared in variant",
            ));
        }
        self
    }
    /// Validate the complete description and return an immutable contract.
    ///
    /// # Errors
    /// Rejects accumulated duplicates and all [`Contract::from_descriptor`] failures.
    /// The consumed builder never registers partial data in a Host.
    pub fn build(self) -> Result<Contract> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Contract::from_descriptor(self.descriptor)
    }
}
