//! Lower a resolved WIT world into the compact runtime model (authoring only).
use crate::{
    Error,
    model::{
        Case, Entry, Field, Function, FunctionKind, Interface, Item, Kind, MAX_DEPTH, Model,
        PreparedContract, ResourceIdentity, Ty,
    },
};
use std::collections::HashMap;
use wit_parser::{
    Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, TypeOwner, WorldId, WorldItem,
};

/// Keep only definitions reachable from the selected world, dependencies first.
struct Lowering<'a> {
    resolve: &'a Resolve,
    types: Vec<Kind>,
    ids: HashMap<TypeId, u32>,
}

/// Prepare a resolved world once; the result is validated like any decoded artifact.
/// `explicit` holds the world's own import keys; elaborated type-only dependencies are absent.
/// `None` (unknown provenance) treats every import as explicit.
pub(crate) fn world(
    resolve: &Resolve,
    world: WorldId,
    explicit: Option<&std::collections::BTreeSet<String>>,
) -> Result<PreparedContract, Error> {
    let selected = &resolve.worlds[world];
    let mut lowering = Lowering {
        resolve,
        types: Vec::new(),
        ids: HashMap::new(),
    };
    let mut side = |items: &wit_parser::IndexMap<wit_parser::WorldKey, WorldItem>,
                    imports: bool| {
        items
            .iter()
            .map(|(key, item)| {
                let key = resolve.name_world_key(key);
                let own = !imports || explicit.is_none_or(|explicit| explicit.contains(&key));
                Ok(Entry {
                    item: match item {
                        WorldItem::Interface { id, .. } => {
                            Item::Interface(lowering.interface(*id, own)?)
                        }
                        WorldItem::Function(function) => {
                            Item::Function(lowering.function(function)?)
                        }
                        WorldItem::Type { id, .. } => Item::Type(lowering.ty(Type::Id(*id), 0)?),
                    },
                    key,
                })
            })
            .collect::<Result<Vec<_>, Error>>()
    };
    let imports = side(&selected.imports, true)?;
    let exports = side(&selected.exports, false)?;
    PreparedContract::new(Model {
        world: selected.name.clone(),
        package: selected
            .package
            .map(|id| resolve.packages[id].name.to_string()),
        types: lowering.types,
        imports,
        exports,
    })
}

impl Lowering<'_> {
    /// Lower an interface's functions and record whether it declares resources.
    fn interface(&mut self, id: InterfaceId, explicit: bool) -> Result<Interface, Error> {
        let interface = &self.resolve.interfaces[id];
        // A resource-only interface can have no constructor or methods. Keep its declarations
        // even when no function signature reaches them, so linkers can identify its types.
        let mut resources = std::collections::BTreeMap::new();
        for (name, id) in &interface.types {
            if self.resource_type(*id) {
                resources.insert(name.clone(), self.reference(*id)?);
            }
        }
        Ok(Interface {
            explicit,
            contract: self.resolve.id_of(id),
            name: interface.name.clone(),
            resources,
            functions: interface
                .functions
                .values()
                .map(|function| self.function(function))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Detect a resource through aliases instead of confusing it with plain type data.
    fn resource_type(&self, mut id: TypeId) -> bool {
        loop {
            match self.resolve.types[id].kind {
                TypeDefKind::Resource => return true,
                TypeDefKind::Type(Type::Id(next)) => id = next,
                _ => return false,
            }
        }
    }

    fn function(&mut self, function: &wit_parser::Function) -> Result<Function, Error> {
        use wit_parser::FunctionKind as Wit;
        let mut resource = |id| self.reference(id);
        let kind = match function.kind {
            Wit::Freestanding => FunctionKind::Freestanding,
            Wit::AsyncFreestanding => FunctionKind::AsyncFreestanding,
            Wit::Getter => FunctionKind::Getter,
            Wit::Setter => FunctionKind::Setter,
            Wit::Method(id) => FunctionKind::Method(resource(id)?),
            Wit::AsyncMethod(id) => FunctionKind::AsyncMethod(resource(id)?),
            Wit::MethodGetter(id) => FunctionKind::MethodGetter(resource(id)?),
            Wit::MethodSetter(id) => FunctionKind::MethodSetter(resource(id)?),
            Wit::Static(id) => FunctionKind::Static(resource(id)?),
            Wit::AsyncStatic(id) => FunctionKind::AsyncStatic(resource(id)?),
            Wit::StaticGetter(id) => FunctionKind::StaticGetter(resource(id)?),
            Wit::StaticSetter(id) => FunctionKind::StaticSetter(resource(id)?),
            Wit::Constructor(id) => FunctionKind::Constructor(resource(id)?),
        };
        Ok(Function {
            name: function.name.clone(),
            kind,
            params: function
                .params
                .iter()
                .map(|param| {
                    Ok(Field {
                        name: param.name.clone(),
                        ty: self.ty(param.ty, 0)?,
                    })
                })
                .collect::<Result<_, Error>>()?,
            result: function.result.map(|ty| self.ty(ty, 0)).transpose()?,
        })
    }

    /// Lower a definition reference and return its compact index.
    fn reference(&mut self, id: TypeId) -> Result<u32, Error> {
        match self.ty(Type::Id(id), 0)? {
            Ty::Ref(index) => Ok(index),
            _ => unreachable!("type identifiers lower to references"),
        }
    }

    fn ty(&mut self, ty: Type, depth: usize) -> Result<Ty, Error> {
        Ok(match ty {
            Type::Bool => Ty::Bool,
            Type::U8 => Ty::U8,
            Type::U16 => Ty::U16,
            Type::U32 => Ty::U32,
            Type::U64 => Ty::U64,
            Type::S8 => Ty::S8,
            Type::S16 => Ty::S16,
            Type::S32 => Ty::S32,
            Type::S64 => Ty::S64,
            Type::F32 => Ty::F32,
            Type::F64 => Ty::F64,
            Type::Char => Ty::Char,
            Type::String => Ty::String,
            Type::ErrorContext => Ty::ErrorContext,
            Type::Id(id) => {
                if let Some(index) = self.ids.get(&id) {
                    return Ok(Ty::Ref(*index));
                }
                if depth > MAX_DEPTH {
                    return Err(Error("WIT type nesting exceeds the contract limit".into()));
                }
                let kind = self.kind(id, depth + 1)?;
                let index = u32::try_from(self.types.len())
                    .map_err(|_| Error("too many WIT types".into()))?;
                self.types.push(kind);
                self.ids.insert(id, index);
                Ty::Ref(index)
            }
        })
    }

    fn kind(&mut self, id: TypeId, depth: usize) -> Result<Kind, Error> {
        let definition = &self.resolve.types[id];
        if matches!(definition.kind, TypeDefKind::Resource) {
            return Ok(Kind::Resource(ResourceIdentity {
                interface: match definition.owner {
                    TypeOwner::Interface(owner) => self.resolve.id_of(owner),
                    TypeOwner::World(_) | TypeOwner::None => None,
                },
                name: definition.name.clone(),
            }));
        }
        let resolve = self.resolve;
        let kind = &resolve.types[id].kind;
        let mut ty = |ty: Type| self.ty(ty, depth);
        Ok(match kind {
            TypeDefKind::Type(inner) => Kind::Alias(ty(*inner)?),
            TypeDefKind::Record(record) => Kind::Record(
                record
                    .fields
                    .iter()
                    .map(|field| {
                        Ok(Field {
                            name: field.name.clone(),
                            ty: ty(field.ty)?,
                        })
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            TypeDefKind::Tuple(tuple) => Kind::Tuple(
                tuple
                    .types
                    .iter()
                    .map(|inner| ty(*inner))
                    .collect::<Result<_, _>>()?,
            ),
            TypeDefKind::Variant(variant) => Kind::Variant(
                variant
                    .cases
                    .iter()
                    .map(|case| {
                        Ok(Case {
                            name: case.name.clone(),
                            ty: case.ty.map(&mut ty).transpose()?,
                        })
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            TypeDefKind::Enum(enumeration) => Kind::Enum(
                enumeration
                    .cases
                    .iter()
                    .map(|case| case.name.clone())
                    .collect(),
            ),
            TypeDefKind::Flags(flags) => {
                Kind::Flags(flags.flags.iter().map(|flag| flag.name.clone()).collect())
            }
            TypeDefKind::Option(inner) => Kind::Option(ty(*inner)?),
            TypeDefKind::Result(result) => Kind::Result {
                ok: result.ok.map(&mut ty).transpose()?,
                err: result.err.map(&mut ty).transpose()?,
            },
            TypeDefKind::List(inner) => Kind::List(ty(*inner)?),
            TypeDefKind::FixedLengthList(inner, length) => Kind::FixedList(ty(*inner)?, *length),
            TypeDefKind::Map(key, value) => Kind::Map(ty(*key)?, ty(*value)?),
            TypeDefKind::Resource => unreachable!("handled before lowering value types"),
            TypeDefKind::Handle(Handle::Own(id)) => match ty(Type::Id(*id))? {
                Ty::Ref(index) => Kind::Own(index),
                _ => unreachable!("type identifiers lower to references"),
            },
            TypeDefKind::Handle(Handle::Borrow(id)) => match ty(Type::Id(*id))? {
                Ty::Ref(index) => Kind::Borrow(index),
                _ => unreachable!("type identifiers lower to references"),
            },
            TypeDefKind::Future(inner) => Kind::Future(inner.map(&mut ty).transpose()?),
            TypeDefKind::Stream(inner) => Kind::Stream(inner.map(&mut ty).transpose()?),
            TypeDefKind::Unknown => {
                return Err(Error("unresolved WIT type in a resolved world".into()));
            }
        })
    }
}
