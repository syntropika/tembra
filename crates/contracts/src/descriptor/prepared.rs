use std::collections::BTreeMap;

use crate::{CanonicalProfileKey, Descriptor, Member, PreparedSchema, Result, ValueSchema};

/// Derived metadata follows its checked descriptor owner, never a process-global key.
#[derive(Debug)]
pub(super) struct PreparedContract {
    pub input: RetainedSchema,
    variants: BTreeMap<String, BTreeMap<String, PreparedMember>>,
}

#[derive(Debug, Default)]
pub(super) struct PreparedMember {
    pub args: Option<RetainedSchema>,
    pub result: Option<RetainedSchema>,
    pub state: Option<RetainedSchema>,
    pub profiles: Option<(Result<CanonicalProfileKey>, Result<CanonicalProfileKey>)>,
}

impl PreparedContract {
    // Called only after all descriptor schemas and its fingerprint have been checked.
    pub fn new(descriptor: &Descriptor) -> Self {
        let portable = |schema: &Option<ValueSchema>| match schema {
            Some(ValueSchema::Portable(schema)) => Some(RetainedSchema::new(schema)),
            _ => None,
        };
        Self {
            input: RetainedSchema::new(&descriptor.input),
            variants: descriptor
                .variants
                .iter()
                .map(|(variant, members)| {
                    let members = members
                        .iter()
                        .map(|(name, member)| {
                            let prepared = match member {
                                Member::Method { args, result } => PreparedMember {
                                    args: portable(args),
                                    result: portable(result),
                                    state: None,
                                    profiles: Some((
                                        CanonicalProfileKey::new(args),
                                        CanonicalProfileKey::new(result),
                                    )),
                                },
                                Member::State { schema } => PreparedMember {
                                    state: Some(RetainedSchema::new(schema)),
                                    ..Default::default()
                                },
                                Member::Opaque { .. } => PreparedMember::default(),
                            };
                            (name.clone(), prepared)
                        })
                        .collect();
                    (variant.clone(), members)
                })
                .collect(),
        }
    }

    pub fn member(&self, variant: &str, member: &str) -> Option<&PreparedMember> {
        self.variants.get(variant)?.get(member)
    }
}

/// A checked descriptor can expose a normalized tree exceeding second-pass budgets.
/// Retain that pure preparation result without eagerly changing its failure boundary.
#[derive(Debug)]
pub(super) struct RetainedSchema(Result<PreparedSchema>);

impl RetainedSchema {
    fn new(schema: &serde_json::Value) -> Self {
        Self(PreparedSchema::new(schema))
    }

    fn prepared(&self, value: &serde_json::Value) -> Result<&PreparedSchema> {
        match &self.0 {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                crate::schema::portable(value, 0)?;
                Err(error.clone())
            }
        }
    }

    pub fn validate(&self, value: &serde_json::Value) -> Result<()> {
        self.prepared(value)?.validate(value)
    }

    pub fn clean_arguments(&self, value: &serde_json::Value) -> Result<serde_json::Value> {
        self.prepared(value)?.clean_arguments(value)
    }

    pub fn normalize_input(&self, value: &serde_json::Value) -> Result<serde_json::Value> {
        self.prepared(value)?.normalize_input(value)
    }
}
