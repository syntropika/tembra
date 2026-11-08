//! Lossless WIT signature descriptions for preparation and inspection tools.
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use wit_parser::{InterfaceId, Resolve, Type, TypeDefKind, TypeId, WorldId, WorldItem};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorldDescription<'a> {
    world: &'a str,
    package: Option<String>,
    imports: Vec<WorldBinding<'a>>,
    exports: Vec<WorldBinding<'a>>,
    types: Vec<TypeDescription<'a>>,
    javascript_names: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct TypeDescription<'a> {
    id: usize,
    definition: &'a wit_parser::TypeDef,
}

#[derive(Serialize)]
#[serde(untagged)]
enum WorldBinding<'a> {
    Interface {
        binding: String,
        contract: Option<String>,
        type_only: bool,
        resources: bool,
        functions: &'a wit_parser::IndexMap<String, wit_parser::Function>,
        types: BTreeMap<&'a str, usize>,
    },
    Function {
        binding: String,
        function: &'a wit_parser::Function,
    },
    Type {
        binding: String,
        r#type: usize,
    },
}

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
        items
            .iter()
            .map(|(key, item)| {
                let name = resolve.name_world_key(key);
                match item {
                    WorldItem::Interface { id, .. } => {
                        let interface = &resolve.interfaces[*id];
                        WorldBinding::Interface {
                            binding: name,
                            contract: resolve.id_of(*id),
                            type_only: interface.functions.is_empty()
                                && !has_resource(resolve, *id),
                            resources: has_resource(resolve, *id),
                            functions: &interface.functions,
                            types: interface
                                .types
                                .iter()
                                .map(|(name, id)| (name.as_str(), id.index()))
                                .collect(),
                        }
                    }
                    WorldItem::Function(function) => WorldBinding::Function {
                        binding: name,
                        function,
                    },
                    WorldItem::Type { id, .. } => WorldBinding::Type {
                        binding: name,
                        r#type: id.index(),
                    },
                }
            })
            .collect::<Vec<_>>()
    };
    let mut names = BTreeMap::new();
    let mut insert = |name: &str| {
        names.insert(name.to_owned(), crate::javascript_name(name));
    };
    for (_, interface) in resolve.interfaces.iter() {
        if let Some(name) = &interface.name {
            insert(name);
        }
        for name in interface.functions.keys() {
            insert(name);
        }
    }
    for (_, definition) in resolve.types.iter() {
        match &definition.kind {
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    insert(&field.name);
                }
            }
            TypeDefKind::Flags(flags) => {
                for flag in &flags.flags {
                    insert(&flag.name);
                }
            }
            _ => {}
        }
    }
    serde_json::to_value(WorldDescription {
        world: &selected.name,
        package: selected
            .package
            .map(|id| resolve.packages[id].name.to_string()),
        imports: side(false),
        exports: side(true),
        javascript_names: names,
        types: resolve
            .types
            .iter()
            .map(|(id, definition)| TypeDescription {
                id: id.index(),
                definition,
            })
            .collect(),
    })
    .expect("serializable WIT graph")
}
