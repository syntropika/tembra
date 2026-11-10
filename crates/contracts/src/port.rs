//! Typed data requests share one contract implementation across platform bindings.
use crate::{Binding, Grants, OrbProvider, PreparedContract};
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
    /// Select providers, check profiles and grants, plan, and map Orb imports in one exchange.
    Install {
        name: String,
        api_version: u32,
        #[serde(default)]
        requires: BTreeMap<String, OrbProvider>,
        #[serde(default)]
        grants: Grants,
        #[serde(default)]
        filesystem: BTreeSet<String>,
        #[serde(default)]
        network: BTreeSet<String>,
        /// HTTP provider keys; separate from `network` so a host may back them differently.
        #[serde(default)]
        http: BTreeSet<String>,
    },
    Implementation {
        name: String,
        api_version: u32,
        value: crate::Implementation,
        #[serde(default)]
        bindings: BTreeMap<String, Binding>,
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

/// Encode structured contract diagnostics through the common platform boundary.
pub fn envelope<T: Serialize>(result: Result<T>) -> String {
    let response = match result {
        Ok(value) => Response::Ok(value),
        Err(error) => Response::Error(error.into()),
    };
    serde_json::to_string(&response).expect("serializable contract response")
}

impl PreparedContract {
    /// Decode a complete typed request before performing any planning or validation.
    pub fn request(&self, packet: &str) -> String {
        envelope(
            serde_json::from_str::<Request>(packet)
                .map_err(invalid)
                .and_then(|request| self.query(request)),
        )
    }

    /// Validate a JSON-encoded executor-packet value through a bound slot and return the result
    /// envelope. WIT `u64`/`s64` positions must be canonical decimal strings, the packet wire
    /// form that [`Self::from_wire`] accepts.
    pub fn validate_packet(&self, slot: u32, arguments: bool, value: &str) -> String {
        envelope(
            serde_json::from_str::<Value>(value)
                .map_err(invalid)
                .and_then(|value| self.validate_wire(slot, arguments, &value).map_err(invalid))
                .map(|()| Value::Null),
        )
    }

    /// Reject export and selected Orb-import signatures outside the interpreter value profile.
    pub fn check_profiles(&self, bindings: &BTreeMap<String, Binding>) -> Result<()> {
        self.check_value_profile().map_err(|error| {
            Error::new(Code::Unsupported, "wit_value_profile", error.to_string())
        })?;
        for (name, binding) in bindings {
            if matches!(binding, Binding::Orb { .. }) {
                self.check_import_value_profile(name).map_err(invalid)?;
            }
        }
        Ok(())
    }

    /// Answer the install-time metadata queries; value checks use bound slots instead.
    fn query(&self, request: Request) -> Result<Value> {
        match request {
            Request::Describe {} => Ok(self.describe()),
            Request::Install {
                name,
                api_version,
                requires,
                grants,
                filesystem,
                network,
                http,
            } => {
                let bindings = self.providers(&requires, &grants)?;
                self.check_profiles(&bindings)?;
                let plan = self.plan(&name, api_version, &bindings).map_err(invalid)?;
                grants.check(&plan.host_imports, &filesystem, &network, &http)?;
                Ok(serde_json::json!({
                    "bindings": bindings,
                    "plan": plan.describe(),
                    "importServices": self.import_services(&bindings).map_err(invalid)?,
                }))
            }
            Request::Implementation {
                name,
                api_version,
                value,
                bindings,
            } => {
                let services = self
                    .check_implementation(&name, api_version, value.clone())
                    .map_err(invalid)?;
                let ingress = self.check_ingress(&name, &value).map_err(invalid)?;
                let hooks = self.check_hooks(&value, &bindings).map_err(invalid)?;
                let mut plan = self.plan(&name, api_version, &bindings).map_err(invalid)?;
                hooks.apply(&mut plan);
                let mut checked = serde_json::json!({ "services": services, "hooks": hooks, "plan": plan.describe() });
                // The host-local incoming HTTP handler, never an ordinary portable service.
                if let Some(ingress) = ingress {
                    checked["ingress"] = serde_json::to_value(ingress).map_err(invalid)?;
                }
                Ok(checked)
            }
        }
    }
}
