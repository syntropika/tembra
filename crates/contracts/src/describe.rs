//! Emit the guest value-adapter graph and inspection output from the typed model.
//!
//! The JSON layout matches the official `wit-parser` serialization the guest adapter consumes,
//! so interpreter code restores typed arrays, options, results and field spellings unchanged.
use crate::{
    javascript_name,
    model::{Entry, Function, Item, Kind, PreparedContract, Ty},
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

impl PreparedContract {
    /// Describe both sides of the selected world and every referenced type definition.
    pub fn describe(&self) -> Value {
        let model = self.model();
        let mut names = BTreeMap::new();
        let mut insert = |name: &str| {
            names
                .entry(name.to_owned())
                .or_insert_with(|| javascript_name(name));
        };
        for entry in model.imports.iter().chain(&model.exports) {
            match &entry.item {
                Item::Interface(interface) => {
                    if let Some(name) = &interface.name {
                        insert(name);
                    }
                    for function in &interface.functions {
                        insert(&function.name);
                    }
                }
                Item::Function(function) => insert(&function.name),
                Item::Type(_) => {}
            }
        }
        // Member spellings were derived once with the contract; reuse them instead of renaming.
        for (id, kind) in model.types.iter().enumerate() {
            let spellings = self.spellings(id as u32);
            match kind {
                Kind::Record(fields) => {
                    for (field, spelling) in fields.iter().zip(spellings) {
                        names
                            .entry(field.name.clone())
                            .or_insert_with(|| spelling.to_string());
                    }
                }
                Kind::Flags(flags) => {
                    for (flag, spelling) in flags.iter().zip(spellings) {
                        names
                            .entry(flag.clone())
                            .or_insert_with(|| spelling.to_string());
                    }
                }
                _ => {}
            }
        }
        json!({
            "world": model.world,
            "package": model.package,
            "imports": side(&model.imports),
            "exports": side(&model.exports),
            "types": model.types.iter().enumerate().map(|(id, kind)| {
                let mut definition = json!({"kind": describe_kind(kind)});
                if let Kind::Resource(resource) = kind {
                    definition["name"] = json!(resource.name);
                    definition["owner"] = json!(resource.interface);
                }
                json!({"id": id, "definition": definition})
            }).collect::<Vec<_>>(),
            "javascriptNames": names,
            "references": self.references(),
        })
    }
}

fn side(entries: &[Entry]) -> Value {
    entries
        .iter()
        .map(|Entry { key, item }| match item {
            Item::Interface(interface) => json!({
                "binding": key,
                "contract": interface.contract,
                "type_only": !interface.callable(),
                "explicit": interface.explicit,
                "resources": interface.resources,
                "functions": interface.functions.iter().map(|function| {
                    (function.name.clone(), describe_function(function))
                }).collect::<Map<_, _>>(),
            }),
            Item::Function(function) => {
                json!({"binding": key, "function": describe_function(function)})
            }
            Item::Type(ty) => json!({"binding": key, "type": describe_ty(*ty)}),
        })
        .collect()
}

fn describe_function(function: &Function) -> Value {
    let mut value = json!({
        "name": function.name,
        "kind": match function.kind.resource() {
            Some(resource) => json!({function.kind.as_str(): resource}),
            None => json!(function.kind.as_str()),
        },
        "params": function.params.iter().map(|param| {
            json!({"name": param.name, "type": describe_ty(param.ty)})
        }).collect::<Vec<_>>(),
    });
    if let Some(result) = function.result {
        value["result"] = describe_ty(result);
    }
    value
}

fn describe_ty(ty: Ty) -> Value {
    let name = match ty {
        Ty::Bool => "bool",
        Ty::U8 => "u8",
        Ty::U16 => "u16",
        Ty::U32 => "u32",
        Ty::U64 => "u64",
        Ty::S8 => "s8",
        Ty::S16 => "s16",
        Ty::S32 => "s32",
        Ty::S64 => "s64",
        Ty::F32 => "f32",
        Ty::F64 => "f64",
        Ty::Char => "char",
        Ty::String => "string",
        Ty::ErrorContext => "error-context",
        Ty::Ref(id) => return json!(id),
    };
    json!(name)
}

fn describe_kind(kind: &Kind) -> Value {
    let optional = |ty: Option<Ty>| ty.map_or(Value::Null, describe_ty);
    let named = |names: &[String]| {
        names
            .iter()
            .map(|name| json!({"name": name}))
            .collect::<Vec<_>>()
    };
    match kind {
        Kind::Alias(ty) => json!({"type": describe_ty(*ty)}),
        Kind::Record(fields) => json!({"record": {"fields": fields.iter().map(|field| {
            json!({"name": field.name, "type": describe_ty(field.ty)})
        }).collect::<Vec<_>>()}}),
        Kind::Tuple(types) => {
            json!({"tuple": {"types": types.iter().map(|ty| describe_ty(*ty)).collect::<Vec<_>>()}})
        }
        Kind::Variant(cases) => json!({"variant": {"cases": cases.iter().map(|case| {
            json!({"name": case.name, "type": optional(case.ty)})
        }).collect::<Vec<_>>()}}),
        Kind::Enum(cases) => json!({"enum": {"cases": named(cases)}}),
        Kind::Flags(flags) => json!({"flags": {"flags": named(flags)}}),
        Kind::Option(ty) => json!({"option": describe_ty(*ty)}),
        Kind::Result { ok, err } => json!({"result": {"ok": optional(*ok), "err": optional(*err)}}),
        Kind::List(ty) => json!({"list": describe_ty(*ty)}),
        Kind::FixedList(ty, length) => json!({"fixed-length-list": [describe_ty(*ty), length]}),
        Kind::Map(key, value) => json!({"map": [describe_ty(*key), describe_ty(*value)]}),
        Kind::Resource(_) => json!("resource"),
        Kind::Own(id) => json!({"handle": {"own": id}}),
        Kind::Borrow(id) => json!({"handle": {"borrow": id}}),
        Kind::Future(ty) => json!({"future": optional(*ty)}),
        Kind::Stream(ty) => json!({"stream": optional(*ty)}),
    }
}
