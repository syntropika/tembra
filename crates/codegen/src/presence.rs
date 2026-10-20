use std::collections::BTreeMap;

use serde_json::Value;
use typify::TypeSpace;

use crate::{Result, error};

/// DTO-only metadata links Typify's generated structs to normalized source schemas.
/// The public descriptor and fingerprint never receive these private titles.
#[derive(Default)]
pub(crate) struct Adapter {
    records: BTreeMap<String, Value>,
    optional: BTreeMap<(String, String), String>,
    next: usize,
}

impl Adapter {
    pub fn prepare(&mut self, schema: &mut Value) {
        let Some(object) = schema.as_object_mut() else {
            return;
        };
        let title = if object.get("type").and_then(Value::as_str) == Some("object") {
            let title = format!("TembraRecord{}", self.next);
            self.next += 1;
            object.insert("title".into(), Value::String(title.clone()));
            Some(title)
        } else {
            None
        };
        if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
            for property in properties.values_mut() {
                self.prepare(property);
            }
        }
        if let Some(items) = object.get_mut("items") {
            self.prepare(items);
        }
        if let Some(branches) = object.get_mut("anyOf").and_then(Value::as_array_mut) {
            for branch in branches {
                self.prepare(branch);
            }
        }
        if let Some(title) = title {
            self.records.insert(title, schema.clone());
        }
    }

    pub fn add_property_types(&mut self, types: &mut TypeSpace, path: &str) -> Result<()> {
        for (record, schema) in &self.records {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (name, property) in properties {
                let required = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|fields| fields.iter().any(|field| field.as_str() == Some(name)));
                if required || property == &Value::Bool(false) {
                    continue;
                }
                let property: schemars08::schema::Schema = serde_json::from_value(property.clone())
                    .map_err(|cause| error("schema_adapter", path, cause.to_string()))?;
                let id = types
                    .add_type(&property)
                    .map_err(|cause| error("typify", path, cause.to_string()))?;
                let kind = types
                    .get_type(&id)
                    .map_err(|cause| error("typify", path, cause.to_string()))?
                    .ident()
                    .to_string();
                self.optional.insert((record.clone(), name.clone()), kind);
            }
        }
        Ok(())
    }

    pub fn rewrite(&self, file: &mut syn::File, path: &str) -> Result<()> {
        for item in &mut file.items {
            match item {
                syn::Item::Struct(item) => {
                    let record = item.ident.to_string();
                    for field in &mut item.fields {
                        let (wire, flatten, optional) = serde_field(field)?;
                        if flatten {
                            continue;
                        }
                        let Some(wire) = wire else {
                            continue;
                        };
                        if let Some(kind) = self.optional.get(&(record.clone(), wire.clone())) {
                            field.ty = syn::parse_str(&format!("super::Presence<{kind}>"))
                                .map_err(|cause| {
                                    error("generated_syntax", path, cause.to_string())
                                })?;
                            field
                                .attrs
                                .retain(|attribute| !attribute.path().is_ident("serde"));
                            let rename = syn::LitStr::new(&wire, item.ident.span());
                            field.attrs.push(syn::parse_quote!(
                                #[serde(
                                    default,
                                    skip_serializing_if = "super::Presence::is_missing",
                                    rename = #rename
                                )]
                            ));
                        } else if optional {
                            return Err(error(
                                "unrepresentable_presence",
                                path,
                                concat!(
                                    "Typify generated an optional field ",
                                    "without an exact source-property mapping",
                                ),
                            ));
                        }
                    }
                }
                syn::Item::Enum(item) => {
                    for variant in &item.variants {
                        for field in &variant.fields {
                            if serde_field(field)?.2 {
                                return Err(error(
                                    "unrepresentable_presence",
                                    path,
                                    concat!(
                                        "optional fields in a synthesized union variant ",
                                        "require a proved source mapping",
                                    ),
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn serde_field(field: &syn::Field) -> Result<(Option<String>, bool, bool)> {
    let mut wire = field.ident.as_ref().map(ToString::to_string);
    let mut flatten = false;
    let mut optional = false;
    for attribute in &field.attrs {
        if !attribute.path().is_ident("serde") {
            continue;
        }
        attribute
            .parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    wire = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                } else if meta.path.is_ident("flatten") {
                    flatten = true;
                } else if meta.path.is_ident("default") {
                    optional = true;
                    if meta.input.peek(syn::Token![=]) {
                        let _: syn::LitStr = meta.value()?.parse()?;
                    }
                } else if meta.input.peek(syn::Token![=]) {
                    let _: syn::Expr = meta.value()?.parse()?;
                }
                Ok(())
            })
            .map_err(|cause| error("generated_syntax", "serde field", cause.to_string()))?;
    }
    Ok((wire, flatten, optional))
}

pub(crate) const HELPER: &str = r#"
/// Presence of an object property; explicit null and empty containers remain present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence<T> {
    Missing,
    Present(T),
}

impl<T> Default for Presence<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<T> Presence<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl<T: ::serde::Serialize> ::serde::Serialize for Presence<T> {
    fn serialize<S: ::serde::Serializer>(
        &self,
        serializer: S,
    ) -> ::std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Present(value) => value.serialize(serializer),
            Self::Missing => Err(::serde::ser::Error::custom(
                "missing properties must be skipped by their containing DTO",
            )),
        }
    }
}

impl<'de, T: ::serde::Deserialize<'de>> ::serde::Deserialize<'de> for Presence<T> {
    fn deserialize<D: ::serde::Deserializer<'de>>(
        deserializer: D,
    ) -> ::std::result::Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}
"#;
