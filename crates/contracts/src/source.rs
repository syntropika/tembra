use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::{ContractError, Result};

#[derive(Clone, Copy, Debug)]
pub struct CompileLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
}

impl Default for CompileLimits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_nodes: 16_384,
        }
    }
}

pub(crate) fn resolve(
    source: &Value,
    limits: CompileLimits,
) -> Result<(Value, Vec<(String, Value)>)> {
    let mut resolver = Resolver {
        root: source,
        limits,
        nodes: 0,
        active: BTreeSet::new(),
        definitions: Vec::new(),
    };
    let normalized = resolver.expand(source, "#", 0)?;
    Ok((normalized, resolver.definitions))
}

struct Resolver<'a> {
    root: &'a Value,
    limits: CompileLimits,
    nodes: usize,
    active: BTreeSet<String>,
    definitions: Vec<(String, Value)>,
}

impl Resolver<'_> {
    fn expand(&mut self, source: &Value, path: &str, depth: usize) -> Result<Value> {
        self.nodes += 1;
        if depth > self.limits.max_depth || self.nodes > self.limits.max_nodes {
            return Err(error(
                "schema_budget",
                path,
                format!("{path}: schema expansion budget exceeded"),
            ));
        }
        let Some(object) = source.as_object() else {
            return Ok(source.clone());
        };
        if object.get("$schema").is_some_and(|dialect| {
            dialect.as_str() != Some("https://json-schema.org/draft/2020-12/schema")
        }) {
            return Err(error(
                "dialect",
                path,
                format!("{path}: unsupported schema dialect"),
            ));
        }
        for keyword in ["$id", "$anchor", "$dynamicAnchor", "$dynamicRef"] {
            if object.contains_key(keyword) {
                return Err(error(
                    "unsupported_schema",
                    path,
                    format!(
                        "{path}/{keyword}: resource rebasing/dynamic references are unsupported"
                    ),
                ));
            }
        }
        if let Some(reference) = object.get("$ref") {
            let reference = reference
                .as_str()
                .filter(|reference| reference.starts_with("#/$defs/"))
                .ok_or_else(|| {
                    error(
                        "reference",
                        path,
                        format!(
                            "{path}: only local JSON Pointer references into $defs are supported"
                        ),
                    )
                })?;
            let annotations = [
                "title",
                "description",
                "default",
                "examples",
                "$comment",
                "readOnly",
                "writeOnly",
                "deprecated",
            ];
            if object.keys().any(|key| {
                !["$ref", "$defs", "$schema"].contains(&key.as_str())
                    && !annotations.contains(&key.as_str())
            }) {
                return Err(error(
                    "reference_sibling",
                    path,
                    format!("{path}: $ref validation siblings are unsupported"),
                ));
            }
            if let Some(definitions) = object.get("$defs") {
                let definitions = definitions.as_object().ok_or_else(|| {
                    error(
                        "schema",
                        path,
                        format!("{path}/$defs: definitions must be an object"),
                    )
                })?;
                for (name, definition) in definitions {
                    let expanded = self.expand(
                        definition,
                        &format!("{path}/$defs/{}", escape(name)),
                        depth + 1,
                    )?;
                    self.definitions.push((
                        location(&format!("{path}/$defs/{}", escape(name))),
                        expanded,
                    ));
                }
            }
            if !valid_pointer(reference) {
                return Err(error(
                    "reference",
                    path,
                    format!("{path}: invalid JSON Pointer escape"),
                ));
            }
            if !self.active.insert(reference.into()) {
                return Err(error(
                    "recursive_schema",
                    path,
                    format!("{path}: recursive reference {reference}"),
                ));
            }
            let target = self.root.pointer(&reference[1..]).ok_or_else(|| {
                error(
                    "reference",
                    path,
                    format!("{path}: unresolved reference {reference}"),
                )
            })?;
            let target = target.clone();
            let result = self.expand(&target, reference, depth + 1);
            self.active.remove(reference);
            let mut result = result?;
            if object.keys().any(|key| annotations.contains(&key.as_str())) {
                let target = result.as_object_mut().ok_or_else(|| {
                    error(
                        "reference_sibling",
                        path,
                        "annotations on boolean references require an object target",
                    )
                })?;
                for (key, value) in object {
                    if annotations.contains(&key.as_str()) {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            return Ok(result);
        }
        let mut result = Map::new();
        for (key, value) in object {
            let next = format!("{path}/{key}");
            match key.as_str() {
                "$defs" => {
                    let definitions = value.as_object().ok_or_else(|| {
                        error(
                            "schema",
                            path,
                            format!("{next}: definitions must be an object"),
                        )
                    })?;
                    for (name, definition) in definitions {
                        let expanded = self.expand(
                            definition,
                            &format!("{next}/{}", escape(name)),
                            depth + 1,
                        )?;
                        self.definitions
                            .push((location(&format!("{next}/{}", escape(name))), expanded));
                    }
                }
                "properties" => {
                    let properties = value.as_object().ok_or_else(|| {
                        error(
                            "schema",
                            path,
                            format!("{next}: properties must be an object"),
                        )
                    })?;
                    let mut values = Map::new();
                    for (name, property) in properties {
                        values.insert(
                            name.clone(),
                            self.expand(property, &format!("{next}/{}", escape(name)), depth + 1)?,
                        );
                    }
                    result.insert(key.clone(), Value::Object(values));
                }
                "items" => {
                    result.insert(key.clone(), self.expand(value, &next, depth + 1)?);
                }
                "anyOf" => {
                    let branches = value.as_array().ok_or_else(|| {
                        error("schema", path, format!("{next}: union must be an array"))
                    })?;
                    let mut values = Vec::new();
                    for (index, branch) in branches.iter().enumerate() {
                        values.push(self.expand(branch, &format!("{next}/{index}"), depth + 1)?);
                    }
                    result.insert(key.clone(), Value::Array(values));
                }
                _ => {
                    result.insert(key.clone(), value.clone());
                }
            }
        }
        Ok(Value::Object(result))
    }
}

fn valid_pointer(reference: &str) -> bool {
    let mut chars = reference.chars();
    while let Some(character) = chars.next() {
        if character == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn location(path: &str) -> String {
    let path = path.strip_prefix('#').unwrap_or(path).trim_end_matches('/');
    if path.is_empty() {
        "/".into()
    } else {
        path.into()
    }
}

fn error(reason: &str, path: &str, message: impl Into<String>) -> ContractError {
    ContractError::new(reason, message).with_path(location(path))
}
