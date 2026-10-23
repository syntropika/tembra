use std::{cell::OnceCell, collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::schema::{canonical_json, identity_value, normalize_schema};
use crate::{ContractError, Result};

mod prepared;

use prepared::PreparedContract;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContractKind {
    Local,
    Remote,
}

/// Native markers describe local signatures; they never encode native values.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueSchema {
    Portable(Value),
    Native(String),
}

impl Serialize for ValueSchema {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Portable(value) => value.serialize(serializer),
            Self::Native(id) => serde_json::json!({"native": id}).serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ValueSchema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if let Some(object) = value
            .as_object()
            .filter(|object| object.contains_key("native"))
        {
            if object.len() != 1 {
                return Err(serde::de::Error::custom("native marker has extra fields"));
            }
            return object["native"]
                .as_str()
                .map(|id| Self::Native(id.to_owned()))
                .ok_or_else(|| serde::de::Error::custom("native id must be a string"));
        }
        Ok(Self::Portable(value))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Member {
    Method {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_schema"
        )]
        args: Option<ValueSchema>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_schema"
        )]
        result: Option<ValueSchema>,
    },
    State {
        schema: Value,
    },
    Opaque {
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Placement {
    Named { name: String },
    Family { prefix: String, key: String },
}

/// Unchecked descriptor data. Use a [`Contract`] constructor before registration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descriptor {
    pub descriptor_version: u64,
    pub name: String,
    pub version: u64,
    pub kind: ContractKind,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    pub variants: BTreeMap<String, BTreeMap<String, Member>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
}

/// Immutable validated description.
///
/// Serde deserialization requires a valid supplied fingerprint and checks the semantic
/// portable value. [`Contract::from_json`] additionally checks lexical JSON details such
/// as duplicate keys and numeric spelling. Trusted authoring uses [`Contract::from_descriptor`].
#[derive(Clone, Serialize)]
#[serde(transparent)]
pub struct Contract {
    descriptor: Descriptor,
    #[serde(skip)]
    prepared: Arc<PreparedContract>,
}

impl std::fmt::Debug for Contract {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Contract")
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

impl PartialEq for Contract {
    fn eq(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor
    }
}

impl<'de> Deserialize<'de> for Contract {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::from_peer_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Contract {
    /// Decode a peer token through strict JSON and require its verified fingerprint.
    ///
    /// # Errors
    /// Rejects malformed/duplicate JSON, nonportable data, missing or incorrect
    /// fingerprints, unsupported schemas, and descriptor invariant violations.
    /// A digest authenticates no publisher; it only verifies canonical descriptor data.
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use serde_json::json;
    /// use tembra_contracts::Contract;
    /// let local = Contract::local("example", 1, json!({}), BTreeMap::new())?;
    /// let token = serde_json::to_string(&local)?;
    /// assert_eq!(Contract::from_json(&token)?, local);
    /// let original = local.fingerprint().to_owned();
    /// let mut forged = serde_json::to_value(&local)?;
    /// forged["version"] = json!(2);
    /// assert!(Contract::from_json(&forged.to_string()).is_err());
    /// assert_eq!(local.fingerprint(), original);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_json(input: &str) -> Result<Self> {
        Self::from_peer_value(crate::parse_json(input)?)
    }

    fn from_peer_value(mut value: Value) -> Result<Self> {
        canonical_json(&value)?;
        crate::json::normalize_numbers(&mut value);
        let digest = value
            .get("fingerprint")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ContractError::new("fingerprint", "peer descriptor requires a fingerprint")
            })?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContractError::new(
                "fingerprint",
                "peer fingerprint must be 64 lowercase hexadecimal characters",
            ));
        }
        if value.get("placement").is_some_and(Value::is_null) {
            return Err(ContractError::new("placement", "placement cannot be null"));
        }
        let descriptor = serde_json::from_value(value)
            .map_err(|error| ContractError::new("descriptor", error.to_string()))?;
        Self::from_descriptor(descriptor)
    }
    /// Construct a local contract; native signature metadata remains local.
    ///
    /// # Errors
    /// Rejects invalid headers, schemas, reserved names and malformed nominal IDs.
    pub fn local(
        name: impl Into<String>,
        version: u64,
        input: Value,
        members: BTreeMap<String, Member>,
    ) -> Result<Self> {
        Self::from_descriptor(Descriptor {
            descriptor_version: 1,
            name: name.into(),
            version,
            kind: ContractKind::Local,
            input,
            placement: None,
            variants: BTreeMap::from([(String::new(), members)]),
            fingerprint: String::new(),
        })
    }

    /// Construct a remote contract with explicit placement and named variants.
    ///
    /// # Errors
    /// Also rejects missing/invalid placement, unnamed variants and native/opaque members.
    pub fn remote(
        name: impl Into<String>,
        version: u64,
        input: Value,
        placement: Placement,
        variants: BTreeMap<String, BTreeMap<String, Member>>,
    ) -> Result<Self> {
        Self::from_descriptor(Descriptor {
            descriptor_version: 1,
            name: name.into(),
            version,
            kind: ContractKind::Remote,
            input,
            placement: Some(placement),
            variants,
            fingerprint: String::new(),
        })
    }

    /// Validate owned authoring data and compute its normalized fingerprint.
    ///
    /// An empty fingerprint requests computation for trusted authoring. Peer ingress
    /// must use [`Self::from_json`], which requires a supplied fingerprint.
    ///
    /// # Errors
    /// Rejects descriptor/profile violations or a nonempty incorrect fingerprint.
    pub fn from_descriptor(mut descriptor: Descriptor) -> Result<Self> {
        if descriptor.descriptor_version != 1
            || descriptor.name.is_empty()
            || descriptor.version == 0
            || descriptor.version > 9_007_199_254_740_991
        {
            return Err(ContractError::new(
                "descriptor",
                "invalid descriptor version, contract name, or contract version",
            ));
        }
        descriptor.input = normalize_schema(&descriptor.input)?;
        crate::schema::check_normalization(&descriptor.input)?;
        match descriptor.kind {
            ContractKind::Local
                if descriptor.placement.is_some()
                    || descriptor.variants.len() != 1
                    || !descriptor.variants.contains_key("") =>
            {
                return Err(ContractError::new(
                    "descriptor",
                    "local contract requires only the unnamed variant and no placement",
                ));
            }
            ContractKind::Remote
                if descriptor.placement.is_none()
                    || descriptor.variants.is_empty()
                    || descriptor.variants.contains_key("") =>
            {
                return Err(ContractError::new(
                    "descriptor",
                    "remote contract requires placement and named variants",
                ));
            }
            _ => {}
        }
        if let Some(placement) = &descriptor.placement {
            match placement {
                Placement::Named { name } if name.is_empty() => {
                    return Err(ContractError::new("placement", "host name is empty"));
                }
                Placement::Family { prefix, key } => {
                    let required = descriptor.input.get("required").and_then(Value::as_array);
                    if prefix.is_empty()
                        || key.is_empty()
                        || !required.is_some_and(|keys| {
                            keys.iter().any(|value| value.as_str() == Some(key))
                        })
                        || descriptor
                            .input
                            .pointer(&format!(
                                "/properties/{}/type",
                                key.replace('~', "~0").replace('/', "~1")
                            ))
                            .and_then(Value::as_str)
                            != Some("string")
                    {
                        return Err(ContractError::new(
                            "placement",
                            "family key must be a required string property and prefix must be nonempty",
                        ));
                    }
                }
                _ => {}
            }
        }
        for members in descriptor.variants.values_mut() {
            for (name, member) in members {
                if name.is_empty()
                    || [
                        "then", "status", "ready", "release", "variant", "shape", "contract",
                        "error", "onShape", "onStatus",
                    ]
                    .contains(&name.as_str())
                {
                    return Err(ContractError::new(
                        "member",
                        format!("reserved member name: {name}"),
                    ));
                }
                match member {
                    Member::Method { args, result } => {
                        for schema in args.iter_mut().chain(result.iter_mut()) {
                            match schema {
                                ValueSchema::Portable(value) => *value = normalize_schema(value)?,
                                ValueSchema::Native(id) => {
                                    if descriptor.kind == ContractKind::Remote {
                                        return Err(ContractError::new(
                                            "member",
                                            "remote methods cannot have native signatures",
                                        ));
                                    }
                                    check_nominal(id)?;
                                }
                            }
                        }
                    }
                    Member::State { schema } => *schema = normalize_schema(schema)?,
                    Member::Opaque { id } => {
                        if descriptor.kind == ContractKind::Remote {
                            return Err(ContractError::new(
                                "member",
                                "remote contracts cannot have opaque members",
                            ));
                        }
                        check_nominal(id)?;
                    }
                }
            }
        }
        let supplied = std::mem::take(&mut descriptor.fingerprint);
        let bytes = canonical_json(
            &serde_json::to_value(&descriptor)
                .map_err(|error| ContractError::new("descriptor", error.to_string()))?,
        )?;
        let computed = format!("{:x}", Sha256::digest(bytes.as_bytes()));
        if !supplied.is_empty() && supplied != computed {
            return Err(ContractError::new(
                "fingerprint",
                "descriptor fingerprint does not match canonical bytes",
            ));
        }
        descriptor.fingerprint = computed;
        let prepared = Arc::new(PreparedContract::new(&descriptor));
        Ok(Self {
            descriptor,
            prepared,
        })
    }

    pub fn name(&self) -> &str {
        &self.descriptor.name
    }
    pub fn version(&self) -> u64 {
        self.descriptor.version
    }
    pub fn kind(&self) -> ContractKind {
        self.descriptor.kind
    }
    pub fn input(&self) -> &Value {
        &self.descriptor.input
    }
    pub fn placement(&self) -> Option<&Placement> {
        self.descriptor.placement.as_ref()
    }
    pub fn variants(&self) -> &BTreeMap<String, BTreeMap<String, Member>> {
        &self.descriptor.variants
    }
    pub fn members(&self, variant: &str) -> Option<&BTreeMap<String, Member>> {
        self.descriptor.variants.get(variant)
    }
    pub fn fingerprint(&self) -> &str {
        &self.descriptor.fingerprint
    }
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// Compares this exact checked member's retained canonical profiles.
    ///
    /// Returns false for a missing variant/member, non-method or different position.
    /// Keys are comparison data only; this neither validates values nor admits a call.
    /// A deferred canonical preparation failure does not become a matching key.
    pub fn method_profiles_match(
        &self,
        variant: &str,
        member: &str,
        input: &crate::CanonicalProfileKey,
        output: &crate::CanonicalProfileKey,
    ) -> bool {
        self.prepared.member(variant, member)
            .and_then(|member| member.profiles.as_ref())
            .is_some_and(|(args, result)| matches!((args, result), (Ok(args), Ok(result)) if args == input && result == output))
    }

    /// Clean portable method arguments using this descriptor's retained preparation.
    /// No default insertion, value validation or runtime admission is performed.
    ///
    /// # Errors
    /// Refuses an absent/native/non-method slot, or the same value/schema errors as
    /// [`crate::clean_arguments`], checking value portability before preparation refusals.
    pub fn clean_method_args(&self, variant: &str, member: &str, value: &Value) -> Result<Value> {
        self.prepared
            .member(variant, member)
            .and_then(|m| m.args.as_ref())
            .ok_or_else(|| ContractError::new("member", "portable argument schema is absent"))?
            .clean_arguments(value)
    }

    /// Validate portable arguments with retained schema preparation and no value caching.
    ///
    /// # Errors
    /// Refuses an absent/native/non-method slot or the same ordered errors as [`crate::validate`].
    pub fn validate_method_args(&self, variant: &str, member: &str, value: &Value) -> Result<()> {
        self.prepared
            .member(variant, member)
            .and_then(|m| m.args.as_ref())
            .ok_or_else(|| ContractError::new("member", "portable argument schema is absent"))?
            .validate(value)
    }

    /// Validate a portable result without cleaning, coercion or runtime admission.
    ///
    /// # Errors
    /// Refuses an absent/native/non-method slot or the same ordered errors as [`crate::validate`].
    pub fn validate_method_result(&self, variant: &str, member: &str, value: &Value) -> Result<()> {
        self.prepared
            .member(variant, member)
            .and_then(|m| m.result.as_ref())
            .ok_or_else(|| ContractError::new("member", "portable result schema is absent"))?
            .validate(value)
    }

    /// Validate a state value against this exact descriptor variant/member.
    ///
    /// # Errors
    /// Refuses a missing/non-state slot or the same ordered errors as [`crate::validate`].
    pub fn validate_state(&self, variant: &str, member: &str, value: &Value) -> Result<()> {
        self.prepared
            .member(variant, member)
            .and_then(|m| m.state.as_ref())
            .ok_or_else(|| ContractError::new("member", "state schema is absent"))?
            .validate(value)
    }

    /// Normalize acquisition input without changing this contract or the caller's value.
    ///
    /// # Errors
    /// See [`crate::normalize_input`]; no input candidate is adopted by this operation.
    pub fn normalize_input(&self, value: &Value) -> Result<Value> {
        self.prepared.input.normalize_input(value)
    }

    /// Prepares one acquisition's owned input under this exact immutable contract.
    ///
    /// No caller-supplied validation proof is accepted. The result exposes no mutation,
    /// dispatch authority, Context or authorization result. Identity is evaluated lazily.
    /// # Errors
    /// Returns the ordinary normalization error before creating a prepared input.
    #[doc(hidden)]
    pub fn prepare_acquisition_input(&self, value: &Value) -> Result<AcquisitionInput<'_>> {
        let normalized = self.normalize_input(value)?;

        Ok(AcquisitionInput {
            contract: self,
            normalized,
            identity: OnceCell::new(),
        })
    }
    /// Normalize input, omit default-valued properties, and return its JCS identity.
    ///
    /// # Errors
    /// Fails if acquisition normalization or canonical serialization fails.
    pub fn identity(&self, value: &Value) -> Result<String> {
        canonical_json(&identity_value(
            self.input(),
            &self.normalize_input(value)?,
        )?)
    }
    /// Normalizes acquisition input and resolves the declared destination.
    ///
    /// # Errors
    /// Returns acquisition normalization errors for any placement, or `placement` when a
    /// family key resolves to an empty or non-string suffix. Valid local input returns `None`.
    pub fn resolve_host(&self, input: &Value) -> Result<Option<String>> {
        let normalized = self.normalize_input(input)?;
        match self.placement() {
            None => Ok(None),
            Some(Placement::Named { name }) => Ok(Some(name.clone())),
            Some(Placement::Family { prefix, key }) => {
                let suffix = normalized
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ContractError::new(
                            "placement",
                            "family key must resolve to a nonempty string",
                        )
                    })?;
                Ok(Some(format!("{prefix}{suffix}")))
            }
        }
    }
}

/// Owned per-acquisition input bound to its originating contract by an immutable borrow.
/// Its fields are private, so another contract cannot adopt it as prevalidated input.
#[doc(hidden)]
pub struct AcquisitionInput<'a> {
    contract: &'a Contract,
    normalized: Value,
    identity: OnceCell<Result<String>>,
}

impl AcquisitionInput<'_> {
    /// Borrows the normalized value; callers cannot change the prepared candidate.
    pub fn value(&self) -> &Value {
        &self.normalized
    }

    /// Consumes this preparation without granting authority over the returned value.
    pub fn into_value(self) -> Value {
        self.normalized
    }

    /// Returns exactly this contract's canonical identity for the prepared candidate.
    /// # Errors
    /// Preserves identity transformation and canonical JSON errors. A refusal is retained
    /// only by this individual preparation, never by the contract or across acquisitions.
    pub fn identity(&self) -> Result<&str> {
        self.identity
            .get_or_init(|| {
                canonical_json(&identity_value(self.contract.input(), &self.normalized)?)
            })
            .as_ref()
            .map(String::as_str)
            .map_err(Clone::clone)
    }

    /// Resolves placement from this preparation's immutable normalized input.
    /// # Errors
    /// Preserves the ordinary nonempty-string family-key refusal.
    pub fn resolve_host(&self) -> Result<Option<String>> {
        match self.contract.placement() {
            None => Ok(None),
            Some(Placement::Named { name }) => Ok(Some(name.clone())),
            Some(Placement::Family { prefix, key }) => {
                let suffix = self
                    .normalized
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ContractError::new(
                            "placement",
                            "family key must resolve to a nonempty string",
                        )
                    })?;
                Ok(Some(format!("{prefix}{suffix}")))
            }
        }
    }
}

fn present_schema<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<ValueSchema>, D::Error> {
    ValueSchema::deserialize(deserializer).map(Some)
}

fn check_nominal(id: &str) -> Result<()> {
    let valid = id.rsplit_once('@').is_some_and(|(name, version)| {
        !name.is_empty()
            && !name.contains('@')
            && !name.chars().any(char::is_whitespace)
            && !version.is_empty()
            && version.bytes().all(|byte| byte.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(ContractError::new(
            "native_id",
            "nominal id must be name@version",
        ))
    }
}

#[cfg(test)]
mod acquisition_preparation_tests {
    use super::*;
    use serde_json::json;

    fn local(schema: Value) -> Contract {
        Contract::local("prepared-acquisition", 1, schema, BTreeMap::new()).unwrap()
    }

    #[test]
    fn prepared_input_matches_repeated_normalization_identity_and_refusals() {
        let schemas = vec![
            json!(true),
            json!({"type":"object","properties":{"value":{"type":"integer","default":0}},"additionalProperties":false}),
            json!({"type":"object","properties":{"value":{"type":["integer","null"],"default":null}},"additionalProperties":true}),
            json!({"type":"object","properties":{"nested":{"type":"object","default":{},"properties":{"name":{"type":"string","default":"x"}}}},"additionalProperties":false}),
            json!({"type":"array","items":{"type":"object","properties":{"value":{"type":"integer","default":7}}}}),
            json!({"anyOf":[{"type":"null"},{"type":"object","properties":{"value":{"type":"integer","default":3}}}]}),
            json!({"anyOf":[{"type":"object","properties":{"tag":{"const":"a"},"value":{"type":"integer","default":1}},"required":["tag"]},{"type":"object","properties":{"tag":{"const":"b"},"value":{"type":"string","default":"s"}},"required":["tag"]}]}),
            json!({"anyOf":[{"type":"integer"},{"type":"string"}]}),
        ];
        let mut values = vec![
            json!(null),
            json!({}),
            json!({"value":0}),
            json!({"value":null}),
            json!({"value":"bad","extra":5}),
            json!({"nested":{"name":"z","unknown":1}}),
            json!([{}, {"value":4}]),
            json!({"tag":"a"}),
            json!({"tag":"b","value":"t"}),
            json!({"tag":"bad"}),
            json!(9007199254740992_u64),
            json!({"value":1.5}),
        ];
        let mut deep = json!(0);
        for _ in 0..300 {
            deep = json!([deep]);
        }
        values.push(deep);
        for schema in schemas {
            let contract = local(schema);
            for value in &values {
                let original = value.clone();
                let legacy = contract.normalize_input(value);
                let candidate = contract.prepare_acquisition_input(value);
                match (legacy, candidate) {
                    (Ok(normalized), Ok(prepared)) => {
                        assert_eq!(prepared.value(), &normalized);
                        let second = contract.normalize_input(&normalized).unwrap();
                        assert_eq!(second, normalized, "normalization must be idempotent");
                        assert_eq!(
                            prepared.identity().map(str::to_owned),
                            contract.identity(&second)
                        );
                        assert_eq!(
                            prepared.identity().map(str::to_owned),
                            contract.identity(value)
                        );
                        assert_eq!(prepared.resolve_host(), contract.resolve_host(&normalized));
                        assert_eq!(prepared.into_value(), normalized);
                    }
                    (Err(old), Err(new)) => assert_eq!(old, new),
                    _ => panic!("normalization result differs"),
                }
                assert_eq!(*value, original, "input changed");
            }
        }
    }

    #[test]
    fn consumer_provider_preparations_keep_their_own_defaults_and_placement() {
        let consumer = local(
            json!({"type":"object","properties":{"mode":{"type":"string","default":"consumer"}}}),
        );
        let provider = local(
            json!({"type":"object","properties":{"mode":{"type":"string","default":"provider"}}}),
        );
        let prepared = consumer.prepare_acquisition_input(&json!({})).unwrap();
        let actual = provider
            .prepare_acquisition_input(prepared.value())
            .unwrap();
        assert_eq!(prepared.value(), &json!({"mode":"consumer"}));
        assert_eq!(prepared.identity().unwrap(), "{}");
        assert_eq!(actual.identity().unwrap(), "{\"mode\":\"consumer\"}");
        assert_ne!(prepared.identity().unwrap(), actual.identity().unwrap());
        let variants = BTreeMap::from([("v".to_owned(), BTreeMap::new())]);
        for placement in [
            Placement::Named {
                name: "fixed".into(),
            },
            Placement::Family {
                prefix: "node-".into(),
                key: "region".into(),
            },
        ] {
            let contract=Contract::remote("remote-input",1,json!({"type":"object","properties":{"region":{"type":"string"}},"required":["region"]}),placement,variants.clone()).unwrap();
            for value in [
                json!({"region":"west"}),
                json!({"region":""}),
                json!({}),
                json!({"region":2}),
            ] {
                let old = contract.resolve_host(&value);
                let new = contract
                    .prepare_acquisition_input(&value)
                    .and_then(|input| input.resolve_host());
                assert_eq!(old, new);
            }
        }
    }
}
