//! Lossless WIT signature descriptions for preparation and inspection tools.
use serde_json::{Value, json};
use wit_parser::{InterfaceId, Resolve, Type, TypeDefKind, TypeId, WorldId, WorldItem};

/// Detect a resource through aliases instead of confusing it with plain type data.
fn resource_type(resolve: &Resolve, mut id: TypeId) -> bool {
    loop {
        match resolve.types[id].kind {
            TypeDefKind::Resource => return true,
            TypeDefKind::Type(Type::Id(next)) => id = next,
            _ => return false,
        }
    }
}

/// Report whether an interface owns or aliases a resource type.
pub(crate) fn has_resource(resolve: &Resolve, id: InterfaceId) -> bool {
    resolve.interfaces[id]
        .types
        .values()
        .any(|id| resource_type(resolve, *id))
}

/// Describe both sides of the selected world and preserve referenced type identities.
pub(crate) fn world(resolve: &Resolve, world: WorldId) -> Value {
    let selected = &resolve.worlds[world];
    let side = |exports| {
        let items = if exports {
            &selected.exports
        } else {
            &selected.imports
        };
        items.iter().map(|(key, item)| {
            let name = resolve.name_world_key(key);
            match item {
                WorldItem::Interface { id, .. } => {
                    let interface = &resolve.interfaces[*id];
                    json!({"binding":name, "contract":resolve.id_of(*id),
                        "type_only":interface.functions.is_empty() && !has_resource(resolve, *id),
                        "resources":has_resource(resolve, *id),
                        "functions":interface.functions,
                        "types":interface.types.iter().map(|(name,id)| (name,id.index())).collect::<std::collections::BTreeMap<_,_>>()})
                }
                WorldItem::Function(function) => json!({"binding":name,"function":function}),
                WorldItem::Type { id, .. } => json!({"binding":name,"type":id.index()}),
            }
        }).collect::<Vec<_>>()
    };
    json!({"world":selected.name,"package":selected.package.map(|id| resolve.packages[id].name.to_string()),
        "imports":side(false),"exports":side(true),
        "types":resolve.types.iter().map(|(id,definition)| json!({"id":id.index(),"definition":definition})).collect::<Vec<_>>()})
}
