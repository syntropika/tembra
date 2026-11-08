//! Compare executable interfaces without trusting package names or arena-local type identities.
use crate::Error;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

/// Describe complete exported signatures, including ordered record fields and variant cases.
pub(crate) fn exports(resolve: &Resolve, world: WorldId) -> Result<Value, Error> {
    side(resolve, &resolve.worlds[world].exports)
}

/// Compare actual component imports before a linker can provide undeclared host capabilities.
pub(crate) fn imports(resolve: &Resolve, world: WorldId) -> Result<Value, Error> {
    side(resolve, &resolve.worlds[world].imports)
}

/// Canonicalize a world side while keeping interface identity and complete function shapes.
fn side(
    resolve: &Resolve,
    items: &wit_parser::IndexMap<wit_parser::WorldKey, WorldItem>,
) -> Result<Value, Error> {
    let mut exports = BTreeMap::new();
    for (key, item) in items {
        if matches!(item, WorldItem::Type { .. }) {
            continue;
        }
        let WorldItem::Interface { id, .. } = item else {
            return Err(Error(
                "compiled value exports require named interfaces".into(),
            ));
        };
        exports.insert(
            resolve.name_world_key(key),
            json!({"contract":resolve.id_of(*id),"functions":interface(resolve, *id)?}),
        );
    }
    Ok(json!(exports))
}

/// Describe one interface independently of its exporting world and resolver-local indexes.
fn interface(resolve: &Resolve, id: wit_parser::InterfaceId) -> Result<Value, Error> {
    let mut functions = BTreeMap::new();
    for (name, function) in &resolve.interfaces[id].functions {
        let params = function
            .params
            .iter()
            .map(|param| Ok(json!({"name":param.name,"type":shape(resolve,param.ty,0)?})))
            .collect::<Result<Vec<_>, Error>>()?;
        functions.insert(name.clone(),json!({"kind":function.kind,"params":params,"result":function.result.map(|ty|shape(resolve,ty,0)).transpose()?}));
    }
    Ok(json!(functions))
}

/// Hash structural signatures for admission against the provider actually installed in the kernel.
pub(crate) fn digest(resolve: &Resolve, id: wit_parser::InterfaceId) -> Result<String, Error> {
    use sha2::{Digest, Sha256};
    let bytes =
        serde_json::to_vec(&interface(resolve, id)?).map_err(|error| Error(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Expand aliases so independently decoded graphs compare by their ABI structure.
fn shape(resolve: &Resolve, ty: Type, depth: usize) -> Result<Value, Error> {
    if depth > 64 {
        return Err(Error(
            "component signature nesting exceeds the value profile".into(),
        ));
    }
    let Type::Id(id) = ty else {
        return Ok(json!(ty));
    };
    let next = |ty| shape(resolve, ty, depth + 1);
    Ok(match &resolve.types[id].kind {
        TypeDefKind::Type(ty) => next(*ty)?,
        TypeDefKind::List(ty) => json!({"list":next(*ty)?}),
        TypeDefKind::Option(ty) => json!({"option":next(*ty)?}),
        TypeDefKind::Record(record) => {
            json!({"record":record.fields.iter().map(|field| Ok(json!([field.name,next(field.ty)?]))).collect::<Result<Vec<_>,Error>>()?})
        }
        TypeDefKind::Tuple(tuple) => {
            json!({"tuple":tuple.types.iter().map(|ty|next(*ty)).collect::<Result<Vec<_>,Error>>()?})
        }
        TypeDefKind::Variant(variant) => {
            json!({"variant":variant.cases.iter().map(|case|Ok(json!([case.name,case.ty.map(next).transpose()?]))).collect::<Result<Vec<_>,Error>>()?})
        }
        TypeDefKind::Result(result) => {
            json!({"result":{"ok":result.ok.map(next).transpose()?,"err":result.err.map(next).transpose()?}})
        }
        TypeDefKind::Flags(flags) => {
            json!({"flags":flags.flags.iter().map(|flag| &flag.name).collect::<Vec<_>>()})
        }
        TypeDefKind::Enum(enumeration) => {
            json!({"enum":enumeration.cases.iter().map(|case| &case.name).collect::<Vec<_>>()})
        }
        other => {
            return Err(Error(format!(
                "{} requires a component value adapter",
                other.as_str()
            )));
        }
    })
}
