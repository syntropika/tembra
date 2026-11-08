//! Thin platform bindings share one contract implementation and its diagnostics.
use crate::{Binding, Contract, Error};
use serde_json::{Value, json};
use std::collections::BTreeMap;

impl Contract {
    /// Execute a data-only contract query for a platform facade, without granting runtime authority.
    pub fn request(&self, packet: &str) -> String {
        let result = serde_json::from_str(packet)
            .map_err(|error| Error(format!("{error}")))
            .and_then(|packet| self.query(packet));
        match result {
            Ok(value) => json!({"ok":value}),
            Err(error) => json!({"error":error.to_string()}),
        }
        .to_string()
    }

    /// Keep parsing, planning and value validation identical across native and browser bindings.
    fn query(&self, request: Value) -> Result<Value, Error> {
        let text = |name: &str| {
            request[name]
                .as_str()
                .ok_or_else(|| Error(format!("expected string {name}")))
        };
        match text("kind")? {
            "describe" => return Ok(self.describe()),
            "component" => {
                let bytes: Vec<u8> = serde_json::from_value(request["bytes"].clone())
                    .map_err(|error| Error(error.to_string()))?;
                self.check_component_exports(&bytes)?;
            }
            "plan" => {
                let bindings: BTreeMap<String, Binding> =
                    serde_json::from_value(request["bindings"].clone())
                        .map_err(|error| Error(error.to_string()))?;
                let version = request["apiVersion"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(|| Error("expected positive API version".into()))?;
                return Ok(self.plan(text("name")?, version, &bindings)?.describe());
            }
            "profile" => {
                self.check_value_profile()?;
                let bindings: BTreeMap<String, Binding> =
                    serde_json::from_value(request["bindings"].clone())
                        .map_err(|error| Error(error.to_string()))?;
                for (name, binding) in bindings {
                    if matches!(binding, Binding::Orb { .. }) {
                        self.check_import_value_profile(&name)?;
                    }
                }
            }
            "arguments" => {
                self.validate_arguments(text("binding")?, text("method")?, &request["value"])?
            }
            "result" => {
                self.validate_result(text("binding")?, text("method")?, &request["value"])?
            }
            "import-arguments" => self.validate_import_arguments(
                text("binding")?,
                text("method")?,
                &request["value"],
            )?,
            "import-result" => {
                self.validate_import_result(text("binding")?, text("method")?, &request["value"])?
            }
            _ => return Err(Error("unknown contract query".into())),
        }
        Ok(Value::Null)
    }
}
