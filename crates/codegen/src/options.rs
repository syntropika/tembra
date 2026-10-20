use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tembra_contracts::{Contract, ContractKind, Member};

use crate::{Result, error};

/// Explicit Rust bindings layered over an unchanged canonical descriptor.
///
/// Type expressions are parsed as Rust types before generation. They must resolve
/// from a generated schema module; application types should use absolute paths.
/// Options and CLI bindings files are trusted Rust build inputs. Type expressions
/// are emitted as source code and may contain macros or constant expressions;
/// parsing checks Rust syntax and does not sandbox their compilation.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GenerationOptions {
    /// Root token name for a contract with exactly one variant.
    pub name: Option<String>,
    /// Native nominal IDs mapped to Rust types. Opaque getters return `Rc<T>`.
    #[serde(default)]
    pub native_types: BTreeMap<String, String>,
    /// Local method names mapped to explicit native signatures.
    ///
    /// These select the checked native route even for portable member schemas.
    /// Remote contracts reject overrides, including same-host remote contracts.
    #[serde(default)]
    pub native_methods: BTreeMap<String, NativeMethodBinding>,
}

/// A local method's actual native Rust argument and result types.
///
/// Presence must match the descriptor. Absent positions use Rust unit internally.
/// These bindings do not assert that a native type implements a portable schema.
/// Type expressions are trusted source code, with the same compilation authority
/// as handwritten application types.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMethodBinding {
    pub args: Option<String>,
    pub result: Option<String>,
}

pub(crate) fn validate(contract: &Contract, options: &GenerationOptions) -> Result<()> {
    if contract.kind() == ContractKind::Remote
        && (!options.native_types.is_empty() || !options.native_methods.is_empty())
    {
        return Err(error(
            "invalid_binding",
            "options",
            "remote contracts always use the validated portable channel",
        ));
    }
    if let Some(name) = &options.name {
        let parsed = syn::parse_str::<syn::Ident>(name)
            .map_err(|cause| error("unsupported_name", "options/name", cause.to_string()))?;
        if parsed != name {
            return Err(error(
                "unsupported_name",
                "options/name",
                "invalid token name",
            ));
        }
        if contract.variants().len() != 1 {
            return Err(error(
                "invalid_binding",
                "options/name",
                "a root token name requires exactly one variant",
            ));
        }
        if matches!(name.as_str(), "Input" | "Presence" | "Root") {
            return Err(error(
                "name_collision",
                "options/name",
                "token name collides with a generated type",
            ));
        }
    }
    for (id, kind) in &options.native_types {
        validate_type(kind, &format!("options/nativeTypes/{id}"))?;
        let used = contract.variants().values().any(|members| {
            members.values().any(|member| match member {
                Member::Opaque { id: member_id } => member_id == id,
                Member::Method { args, result } => [args, result].into_iter().any(|schema| {
                    matches!(schema, Some(tembra_contracts::ValueSchema::Native(value)) if value == id)
                }),
                Member::State { .. } => false,
            })
        });
        if !used {
            return Err(error(
                "invalid_binding",
                id,
                "native ID is absent from the descriptor",
            ));
        }
    }
    for (name, binding) in &options.native_methods {
        let path = format!("options/nativeMethods/{name}");
        let member = contract.members("").and_then(|members| members.get(name));
        let Some(Member::Method { args, result }) = member else {
            return Err(error(
                "invalid_binding",
                &path,
                "local method does not exist",
            ));
        };
        if args.is_some() != binding.args.is_some() || result.is_some() != binding.result.is_some()
        {
            return Err(error(
                "invalid_binding",
                &path,
                "binding presence must match the descriptor",
            ));
        }
        for (position, kind) in [("args", &binding.args), ("result", &binding.result)] {
            if let Some(kind) = kind {
                validate_type(kind, &format!("{path}/{position}"))?;
            }
        }
        for (schema, kind) in [(args, &binding.args), (result, &binding.result)] {
            if let Some(tembra_contracts::ValueSchema::Native(id)) = schema
                && options.native_types.get(id) != kind.as_ref()
            {
                return Err(error(
                    "invalid_binding",
                    &path,
                    "nominal method binding must match its native ID mapping",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_type(kind: &str, path: &str) -> Result<()> {
    syn::parse_str::<syn::Type>(kind)
        .map(|_| ())
        .map_err(|cause| error("invalid_binding", path, cause.to_string()))
}
