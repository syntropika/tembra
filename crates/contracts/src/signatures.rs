//! Compare executable interfaces without trusting package names or arena-local type identities.
use crate::Error;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

/// Describe complete exported signatures, including ordered record fields and variant cases.
pub(crate) fn exports(resolve: &Resolve, world: WorldId) -> Result<Value, Error> {
    let mut exports = BTreeMap::new();
    for (key, item) in &resolve.worlds[world].exports {
        let WorldItem::Interface { id, .. } = item else {
            return Err(Error(
                "compiled value exports require named interfaces".into(),
            ));
        };
        let mut functions = BTreeMap::new();
        for (name, function) in &resolve.interfaces[*id].functions {
            let params = function
                .params
                .iter()
                .map(|param| Ok(json!({"name":param.name,"type":shape(resolve,param.ty,0)?})))
                .collect::<Result<Vec<_>, Error>>()?;
            functions.insert(name.clone(),json!({"kind":function.kind,"params":params,"result":function.result.map(|ty|shape(resolve,ty,0)).transpose()?}));
        }
        exports.insert(
            resolve.name_world_key(key),
            json!({"contract":resolve.id_of(*id),"functions":functions}),
        );
    }
    Ok(json!(exports))
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
