use crate::invalid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::rc::Rc;
use tembra_contracts::{Member, ValueSchema, canonical_json, normalize_schema, parse_json};
use tembra_core::{ConflictPolicy, Registry, Result};

/// Composition policy; each variant has a distinct checked run entry point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Series,
    Bail,
    Waterfall,
    Middleware,
    Parallel,
}

/// Ordinary managed terminal method profile. Null positions mean unit/absence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalProfile {
    pub input: Option<ValueSchema>,
    pub result: Option<ValueSchema>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub format: u64,
    pub name: String,
    pub version: u64,
    pub strategy: Strategy,
    pub input: Option<ValueSchema>,
    pub result: Option<ValueSchema>,
    pub terminal: Option<TerminalProfile>,
    pub fingerprint: String,
}
/// Exact canonical equality, including the existing failed-canonicalization semantics.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProfileEquality(Option<tembra_contracts::CanonicalProfileKey>);

impl ProfileEquality {
    pub(crate) fn key(&self) -> Option<&tembra_contracts::CanonicalProfileKey> {
        self.0.as_ref()
    }

    pub(crate) fn new(profile: &Option<ValueSchema>) -> Self {
        Self(tembra_contracts::CanonicalProfileKey::new(profile).ok())
    }
}

#[derive(Clone)]
pub(crate) struct TerminalEquality {
    input: ProfileEquality,
    result: ProfileEquality,
}

impl TerminalEquality {
    fn new(profile: &TerminalProfile) -> Self {
        Self {
            input: ProfileEquality::new(&profile.input),
            result: ProfileEquality::new(&profile.result),
        }
    }

    pub(crate) fn matches(&self, input: &ProfileEquality, result: &ProfileEquality) -> bool {
        self.input == *input && self.result == *result
    }
}

/// Validated immutable descriptor; authoring and peer decoding are separate boundaries.
#[derive(Clone)]
pub struct HookDescriptor {
    metadata: Rc<DescriptorMetadata>,
}

struct DescriptorMetadata {
    descriptor: Descriptor,
    canonical: String,
    registry: Registry,
    terminal_equality: Option<TerminalEquality>,
}

pub(crate) fn normalize(profile: Option<ValueSchema>) -> Result<Option<ValueSchema>> {
    profile
        .map(|profile| match profile {
            ValueSchema::Portable(value) => normalize_schema(&value)
                .map(ValueSchema::Portable)
                .map_err(|e| invalid("hook_definition", e.message)),
            ValueSchema::Native(id) => {
                // Reuse the actual checked native marker boundary.
                tembra_contracts::Contract::local(
                    "hook.profile",
                    1,
                    serde_json::json!({}),
                    std::collections::BTreeMap::from([(
                        "method".into(),
                        Member::Method {
                            args: Some(ValueSchema::Native(id.clone())),
                            result: None,
                        },
                    )]),
                )
                .map_err(|e| invalid("hook_definition", e.message))?;
                Ok(ValueSchema::Native(id))
            }
        })
        .transpose()
}
pub(crate) fn same(left: &Option<ValueSchema>, right: &Option<ValueSchema>) -> bool {
    canonical_json(&serde_json::to_value(left).expect("profile serialization")).ok()
        == canonical_json(&serde_json::to_value(right).expect("profile serialization")).ok()
}
impl HookDescriptor {
    /// Normalizes profiles and computes the fingerprint without invoking any handler.
    /// # Errors
    /// Refuses invalid names/versions/schemas and inconsistent strategy or terminal positions.
    ///
    /// ```
    /// use serde_json::json;
    /// use tembra_contracts::ValueSchema;
    /// use tembra_hooks::{HookDescriptor, Strategy};
    /// let input = Some(ValueSchema::Portable(json!({"type":"integer"})));
    /// let descriptor = HookDescriptor::new("saved", 1, Strategy::Series,
    ///     input.clone(), None, None)?;
    /// let bytes = descriptor.canonical().to_owned();
    /// assert!(HookDescriptor::new("saved", 1, Strategy::Waterfall,
    ///     input, None, None).is_err());
    /// assert_eq!(descriptor.canonical(), bytes);
    /// let peer = serde_json::to_string(&descriptor.descriptor()).unwrap();
    /// assert_eq!(HookDescriptor::from_json(&peer)?.fingerprint(), descriptor.fingerprint());
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn new(
        name: impl Into<String>,
        version: u64,
        strategy: Strategy,
        input: Option<ValueSchema>,
        result: Option<ValueSchema>,
        terminal: Option<TerminalProfile>,
    ) -> Result<Self> {
        let name = name.into();
        if name.is_empty()
            || name.chars().any(char::is_control)
            || version == 0
            || version > 9_007_199_254_740_991
        {
            return Err(invalid("hook_definition", "invalid hook name or version"));
        }
        let input = normalize(input)?;
        let result = normalize(result)?;
        let terminal = terminal
            .map(|terminal| {
                Ok::<_, tembra_core::RuntimeError>(TerminalProfile {
                    input: normalize(terminal.input)?,
                    result: normalize(terminal.result)?,
                })
            })
            .transpose()?;
        if strategy == Strategy::Series && result.is_some() {
            return Err(invalid("hook_definition", "series result must be absent"));
        }
        if strategy == Strategy::Waterfall && !same(&input, &result) {
            return Err(invalid("hook_definition", "waterfall profiles must agree"));
        }
        if strategy == Strategy::Middleware && terminal.is_none() {
            return Err(invalid("hook_definition", "middleware requires a terminal"));
        }
        if strategy == Strategy::Parallel
            && let Some(terminal) = &terminal
        {
            match &terminal.input {
                Some(ValueSchema::Native(_)) => {}
                Some(ValueSchema::Portable(schema)) => {
                    let Some(ValueSchema::Portable(items)) = &result else {
                        return Err(invalid(
                            "hook_definition",
                            "portable terminal vector needs a portable result",
                        ));
                    };
                    let expected = normalize(Some(ValueSchema::Portable(
                        serde_json::json!({"type":"array","items":items}),
                    )))?;
                    if !same(&Some(ValueSchema::Portable(schema.clone())), &expected) {
                        return Err(invalid(
                            "hook_definition",
                            "parallel terminal vector differs",
                        ));
                    }
                }
                None => {
                    return Err(invalid(
                        "hook_definition",
                        "parallel terminal requires a vector input",
                    ));
                }
            }
        }
        if let Some(terminal) = &terminal {
            if strategy != Strategy::Parallel && !same(&input, &terminal.input) {
                return Err(invalid("hook_definition", "terminal input differs"));
            }
            if matches!(strategy, Strategy::Bail | Strategy::Middleware)
                && !same(&result, &terminal.result)
            {
                return Err(invalid("hook_definition", "terminal result differs"));
            }
        }
        let registry = Registry::new(
            format!("tembra.hook/{name}@{version}"),
            ConflictPolicy::Error,
        )?;
        let mut descriptor = Descriptor {
            format: 1,
            name,
            version,
            strategy,
            input,
            result,
            terminal,
            fingerprint: String::new(),
        };
        let mut value = serde_json::to_value(&descriptor).expect("descriptor serialization");
        value
            .as_object_mut()
            .expect("descriptor object")
            .remove("fingerprint");
        let canonical =
            canonical_json(&value).map_err(|e| invalid("hook_definition", e.message))?;
        let fingerprint = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        descriptor.fingerprint = fingerprint;
        let terminal_equality = descriptor.terminal.as_ref().map(TerminalEquality::new);
        Ok(Self {
            metadata: Rc::new(DescriptorMetadata {
                descriptor,
                canonical,
                registry,
                terminal_equality,
            }),
        })
    }
    /// Decodes strict portable peer JSON with required fields and a matching fingerprint.
    /// # Errors
    /// Refuses malformed JSON, unknown/missing fields, unsupported profiles or a false digest.
    pub fn from_json(text: &str) -> Result<Self> {
        let value = parse_json(text).map_err(|e| invalid("hook_definition", e.message))?;
        let fields = value
            .as_object()
            .ok_or_else(|| invalid("hook_definition", "descriptor must be an object"))?;
        for key in [
            "format",
            "name",
            "version",
            "strategy",
            "input",
            "result",
            "terminal",
            "fingerprint",
        ] {
            if !fields.contains_key(key) {
                return Err(invalid("hook_definition", "missing descriptor field"));
            }
        }
        if let Some(terminal) = fields.get("terminal").filter(|value| !value.is_null())
            && terminal.as_object().is_none_or(|fields| {
                !fields.contains_key("input") || !fields.contains_key("result")
            })
        {
            return Err(invalid("hook_definition", "missing terminal position"));
        }
        let descriptor: Descriptor =
            serde_json::from_value(value).map_err(|e| invalid("hook_definition", e.to_string()))?;
        if descriptor.format != 1 {
            return Err(invalid("hook_definition", "unsupported hook format"));
        }
        let provided = descriptor.fingerprint;
        let checked = Self::new(
            descriptor.name,
            descriptor.version,
            descriptor.strategy,
            descriptor.input,
            descriptor.result,
            descriptor.terminal,
        )?;
        if provided != checked.fingerprint() {
            return Err(invalid("hook_definition", "hook fingerprint mismatch"));
        }
        Ok(checked)
    }
    pub fn descriptor(&self) -> &Descriptor {
        &self.metadata.descriptor
    }
    pub fn fingerprint(&self) -> &str {
        &self.metadata.descriptor.fingerprint
    }
    pub fn canonical(&self) -> &str {
        &self.metadata.canonical
    }
    pub fn registry(&self) -> &Registry {
        &self.metadata.registry
    }
    pub fn strategy(&self) -> Strategy {
        self.metadata.descriptor.strategy
    }
    pub fn input(&self) -> &Option<ValueSchema> {
        &self.metadata.descriptor.input
    }
    pub fn result(&self) -> &Option<ValueSchema> {
        &self.metadata.descriptor.result
    }
    pub fn terminal(&self) -> Option<&TerminalProfile> {
        self.metadata.descriptor.terminal.as_ref()
    }
    pub(crate) fn terminal_equality(&self) -> Option<&TerminalEquality> {
        self.metadata.terminal_equality.as_ref()
    }

    pub fn contribution_member(&self) -> Member {
        Member::Method {
            args: Some(ValueSchema::Native(format!(
                "tembra.hook.args/{}@1",
                self.fingerprint()
            ))),
            result: match self.strategy() {
                Strategy::Series => None,
                Strategy::Bail => Some(ValueSchema::Native(format!(
                    "tembra.hook.bail/{}@1",
                    self.fingerprint()
                ))),
                _ => self.result().clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retained_profile_equality_matches_existing_canonical_comparison() {
        let profiles = [
            None,
            Some(ValueSchema::Native("integer@1".into())),
            Some(ValueSchema::Native("different@1".into())),
            Some(ValueSchema::Portable(json!(true))),
            Some(ValueSchema::Portable(json!({"type":"integer", "minimum":1}))),
            Some(ValueSchema::Portable(json!({"minimum":1.0, "type":"integer"}))),
            Some(ValueSchema::Portable(serde_json::from_str(
                r#"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"integer"}}}"#,
            ).unwrap())),
            Some(ValueSchema::Portable(serde_json::from_str(
                r#"{"properties":{"b":{"type":"integer"},"a":{"type":"string"}},"type":"object"}"#,
            ).unwrap())),
            Some(ValueSchema::Portable(json!({"minimum":9_007_199_254_740_992_u64}))),
            Some(ValueSchema::Portable(json!({"maximum":9_007_199_254_740_993_u64}))),
        ];
        for left in &profiles {
            for right in &profiles {
                assert_eq!(
                    ProfileEquality::new(left) == ProfileEquality::new(right),
                    same(left, right)
                );
            }
        }
        assert!(ProfileEquality::new(&profiles[8]).0.is_none());
        assert!(ProfileEquality::new(&profiles[9]).0.is_none());
        assert!(ProfileEquality::new(&profiles[0]).0.is_some());
    }

    #[test]
    fn descriptor_terminal_equality_follows_immutable_normalized_owner() {
        let profile = Some(ValueSchema::Portable(json!({"type":"integer","minimum":0})));
        let descriptor = HookDescriptor::new(
            "retained",
            1,
            Strategy::Middleware,
            profile.clone(),
            profile.clone(),
            Some(TerminalProfile {
                input: profile.clone(),
                result: profile.clone(),
            }),
        )
        .unwrap();
        let restored =
            HookDescriptor::from_json(&serde_json::to_string(descriptor.descriptor()).unwrap())
                .unwrap();
        let mut external = descriptor.descriptor().clone();
        external.terminal.as_mut().unwrap().input = None;
        let copied = descriptor.clone();
        drop(descriptor);
        let equality = ProfileEquality::new(&profile);
        for owner in [&copied, &restored] {
            let retained = owner.terminal_equality().unwrap();
            assert!(retained.matches(&equality, &equality));
            assert!(!retained.matches(&ProfileEquality::new(&None), &equality));
            assert!(!retained.matches(&equality, &ProfileEquality::new(&None)));
        }
        assert!(
            HookDescriptor::new(
                "bad",
                1,
                Strategy::Middleware,
                profile.clone(),
                profile,
                Some(TerminalProfile {
                    input: None,
                    result: None
                })
            )
            .is_err()
        );
    }
}
