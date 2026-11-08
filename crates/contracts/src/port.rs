//! Typed data requests share one contract implementation across platform bindings.
use crate::{Binding, Contract, Grants, HostImport, OrbProvider};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::{Code, Error, Result};

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Request {
    Describe {},
    Bindings {
        requires: BTreeMap<String, OrbProvider>,
        grants: Grants,
    },
    Grants {
        imports: Vec<HostImport>,
        grants: Grants,
        filesystem: BTreeSet<String>,
        network: BTreeSet<String>,
    },
    Plan {
        name: String,
        api_version: u32,
        bindings: BTreeMap<String, Binding>,
    },
    Profile {
        bindings: BTreeMap<String, Binding>,
    },
    Implementation {
        name: String,
        api_version: u32,
        value: crate::Implementation,
    },
    ImportServices {
        bindings: BTreeMap<String, Binding>,
    },
    ImportMethod {
        binding: String,
        method: String,
    },
    Arguments {
        binding: String,
        method: String,
        value: Value,
    },
    Result {
        binding: String,
        method: String,
        value: Value,
    },
    ImportArguments {
        binding: String,
        method: String,
        value: Value,
    },
    ImportResult {
        binding: String,
        method: String,
        value: Value,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Response<T> {
    Ok(T),
    Error(Failure),
}

#[derive(Serialize)]
struct Failure {
    code: &'static str,
    reason: String,
    message: String,
}

/// Preserve structured diagnostics without adding serde to the lifecycle kernel.
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            code: error.code.as_str(),
            reason: error.reason,
            message: error.message,
        }
    }
}

/// Convert parser and value diagnostics at the contract boundary.
fn invalid(error: impl ToString) -> Error {
    Error::new(Code::Invalid, "wit_contract", error.to_string())
}

impl Contract {
    /// Decode a complete typed request before performing any planning or validation.
    pub fn request(&self, packet: &str) -> String {
        let result = serde_json::from_str::<Request>(packet)
            .map_err(invalid)
            .and_then(|request| self.query(request));
        let response = match result {
            Ok(value) => Response::Ok(value),
            Err(error) => Response::Error(error.into()),
        };
        serde_json::to_string(&response).expect("serializable contract response")
    }

    /// Execute typed metadata operations while keeping dynamic argument data subject to WIT.
    fn query(&self, request: Request) -> Result<Value> {
        match request {
            Request::Describe {} => return Ok(self.describe()),
            Request::Bindings { requires, grants } => {
                return serde_json::to_value(self.providers(&requires, &grants)?).map_err(invalid);
            }
            Request::Grants {
                imports,
                grants,
                filesystem,
                network,
            } => grants.check(&imports, &filesystem, &network)?,
            Request::Plan {
                name,
                api_version,
                bindings,
            } => {
                return Ok(self
                    .plan(&name, api_version, &bindings)
                    .map_err(invalid)?
                    .describe());
            }
            Request::Profile { bindings } => {
                self.check_value_profile().map_err(|error| {
                    Error::new(Code::Unsupported, "wit_value_profile", error.to_string())
                })?;
                for (name, binding) in bindings {
                    if matches!(binding, Binding::Orb { .. }) {
                        self.check_import_value_profile(&name).map_err(invalid)?;
                    }
                }
            }
            Request::Implementation {
                name,
                api_version,
                value,
            } => {
                return serde_json::to_value(
                    self.check_implementation(&name, api_version, value)
                        .map_err(invalid)?,
                )
                .map_err(invalid);
            }
            Request::ImportServices { bindings } => {
                return serde_json::to_value(self.import_services(&bindings).map_err(invalid)?)
                    .map_err(invalid);
            }
            Request::ImportMethod { binding, method } => {
                return serde_json::to_value(
                    self.import_method(&binding, &method).map_err(invalid)?,
                )
                .map_err(invalid);
            }
            Request::Arguments {
                binding,
                method,
                value,
            } => self
                .validate_arguments(&binding, &method, &value)
                .map_err(invalid)?,
            Request::Result {
                binding,
                method,
                value,
            } => self
                .validate_result(&binding, &method, &value)
                .map_err(invalid)?,
            Request::ImportArguments {
                binding,
                method,
                value,
            } => self
                .validate_import_arguments(&binding, &method, &value)
                .map_err(invalid)?,
            Request::ImportResult {
                binding,
                method,
                value,
            } => self
                .validate_import_result(&binding, &method, &value)
                .map_err(invalid)?,
        }
        Ok(Value::Null)
    }
}
