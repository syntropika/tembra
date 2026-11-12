//! What a derived interface uses from the interfaces the Orb consumes. A derived signature names
//! a type of such an interface through the module the Orb module re-exports for it:
//! `greeting::Entry` for `import = "example:greeter/greeting@1.0.0"`, `counter_ref::Tally` for a
//! type the view of an interface imported by key uses, `book::Entry` for an implemented
//! `#[service("example:ledger/book@1.0.0")]`, `guard::Admission` for a hook the module contributes
//! to. So does a field or a case of a record or a variant of the module that enters the WIT, and a
//! parameter names the handle of an imported interface's resource the same way, `texts::Builder`.
//! The derived interface `use`s each name from the interface that defines it, so its bindings name
//! the type the author's code holds: a value crosses as it is, with no copy and no conversion.
//!
//! The bindings put the types of an interface the world only imports in its import module, which
//! the Orb module re-exports, or which a keyed import's view re-exports them from. An interface
//! the world exports, an implemented one or a hook, gets a copy of its types in its export module,
//! and a derived interface, exported too, uses that copy: the Orb module re-exports the export
//! module of an implemented interface and of a contributed hook, but the import module of a hook it
//! declares, which `run` calls, so a derived interface names none of a declared hook's types. A
//! WASI interface's names stay out of a derived interface, and a consumed type crosses one when it
//! carries data only. A state may hold a consumed type that `serde` can write, which the bindings
//! derive for it in an Orb with a state.
use crate::{
    Candidate, Contribution, Declared, Service, Shape, ShapeKind, State, WitResource, hook_path,
    keyed, resource_params, wasi, wit,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use syn::{Ident, Item, Type, spanned::Spanned};

/// How the world holds a consumed interface, which decides what a derived interface names through
/// the interface's module.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Another Orb's interface, imported: its types, and its resources' handles as parameters.
    Imported,
    /// Another Orb's interface, imported by key, whose view the Orb module re-exports: the
    /// interface's types the view uses, but not the view's objects.
    Keyed,
    /// A WASI interface, imported: none of its names.
    Wasi,
    /// An interface the module implements, exported: its types, from the export.
    Implemented,
    /// A hook the module contributes to, imported and exported: its types, from the export.
    Contributed,
    /// A hook the module declares from a consumed package, imported and exported: none of its
    /// names, since the Orb module re-exports the import.
    Declared,
}

/// A consumed interface the Orb module re-exports a module for: its identity and how the world
/// holds it.
pub(crate) struct Reexported {
    pub identity: String,
    pub role: Role,
}

impl Reexported {
    /// The interface, for the errors: "the imported interface ns:name/interface@version".
    fn described(&self) -> String {
        let identity = &self.identity;
        match self.role {
            Role::Imported => format!("the imported interface {identity}"),
            Role::Keyed => format!("the interface {identity}, which this module imports by key"),
            Role::Wasi => format!("the WASI interface {identity}"),
            Role::Implemented => format!("the implemented interface {identity}"),
            Role::Contributed => format!("the hook {identity}, which this module contributes to"),
            Role::Declared => format!("the hook {identity}, which this module declares"),
        }
    }
}

/// The modules the Orb module re-exports for its consumed interfaces, by the name it re-exports
/// each under, `greeting`: the imported interfaces, the views of those imported by key, the
/// implemented ones, the hooks it contributes to and the hooks it declares from a consumed package.
pub(crate) struct Modules(BTreeMap<String, Reexported>);

/// A name a type path reaches through a consumed interface's module, `greeting::Entry`: the
/// interface, the module's name, the name and the path itself, for the errors.
#[derive(Clone, Copy)]
pub(crate) struct Named<'m, 't> {
    pub interface: &'m Reexported,
    pub module: &'m str,
    pub ident: &'t Ident,
    pub ty: &'t Type,
}

impl Modules {
    /// The consumed interfaces of the module, each under the name of its re-exported module: an
    /// interface imported by key under its view's.
    pub(crate) fn new(
        imports: &[String],
        keyed: &[String],
        services: &[Service],
        contributions: &[Contribution],
        declared: &[Declared],
    ) -> Self {
        let mut modules = BTreeMap::new();
        for identity in keyed {
            let view = crate::rust_ident(&keyed::view_name(identity));
            modules
                .entry(view.to_string())
                .or_insert_with(|| Reexported {
                    identity: identity.clone(),
                    role: Role::Keyed,
                });
        }
        let roles = imports
            .iter()
            .map(|identity| {
                let role = if wasi::is_wasi(identity) {
                    Role::Wasi
                } else {
                    Role::Imported
                };
                (identity, role)
            })
            .chain(
                services
                    .iter()
                    .filter_map(|service| service.implements.as_ref())
                    .map(|identity| (identity, Role::Implemented)),
            )
            .chain(
                contributions
                    .iter()
                    .map(|contribution| (&contribution.hook, Role::Contributed)),
            )
            .chain(
                declared
                    .iter()
                    .filter(|declaration| declaration.input.is_none())
                    .map(|declaration| (&declaration.hook, Role::Declared)),
            );
        for (identity, role) in roles {
            let [_, _, interface] = hook_path(identity);
            modules
                .entry(interface.to_string())
                .or_insert_with(|| Reexported {
                    identity: identity.clone(),
                    role,
                });
        }
        Self(modules)
    }

    /// The name a type reaches through a consumed interface's module, when it is such a path:
    /// `greeting::Entry`, or any path ending so.
    pub(crate) fn named<'m, 't>(&'m self, ty: &'t Type) -> Option<Named<'m, 't>> {
        let Type::Path(path) = ty else {
            return None;
        };
        let segments: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
        let [.., module, item] = segments.as_slice() else {
            return None;
        };
        if path.qself.is_some() || !matches!(item.arguments, syn::PathArguments::None) {
            return None;
        }
        let (module, interface) = self.0.get_key_value(&module.ident.to_string())?;
        Some(Named {
            interface,
            module,
            ident: &item.ident,
            ty,
        })
    }

    /// Each name a type reaches through a consumed interface's module: itself, or inside the type
    /// parameters of a path and the elements of a tuple.
    pub(crate) fn named_in<'m, 't>(&'m self, ty: &'t Type, out: &mut Vec<Named<'m, 't>>) {
        if let Some(named) = self.named(ty) {
            out.push(named);
            return;
        }
        match ty {
            Type::Path(path) => {
                if let Some(syn::PathArguments::AngleBracketed(arguments)) =
                    path.path.segments.last().map(|segment| &segment.arguments)
                {
                    for argument in &arguments.args {
                        if let syn::GenericArgument::Type(ty) = argument {
                            self.named_in(ty, out);
                        }
                    }
                }
            }
            Type::Tuple(tuple) => {
                for element in &tuple.elems {
                    self.named_in(element, out);
                }
            }
            _ => {}
        }
    }

    /// The module types a type mentions: the last name of each path, but the types with a WIT
    /// spelling of their own, looking through type parameters and tuples, and none of the names a
    /// consumed interface's module reaches, which are no types of the module.
    pub(crate) fn references(&self, ty: &Type, out: &mut Vec<Ident>) {
        if self.named(ty).is_some() {
            return;
        }
        match ty {
            Type::Path(path) => {
                let Some(segment) = path.path.segments.last() else {
                    return;
                };
                if !wit::BUILTIN.contains(&segment.ident.to_string().as_str()) {
                    out.push(segment.ident.clone());
                }
                if let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments {
                    for argument in &arguments.args {
                        if let syn::GenericArgument::Type(ty) = argument {
                            self.references(ty, out);
                        }
                    }
                }
            }
            Type::Tuple(tuple) => {
                for element in &tuple.elems {
                    self.references(element, out);
                }
            }
            _ => {}
        }
    }

    /// The module types the fields of a struct or the payloads of an enum's cases mention.
    pub(crate) fn item_references(&self, item: &Item) -> Vec<Ident> {
        let mut references = Vec::new();
        for ty in item_types(item) {
            self.references(ty, &mut references);
        }
        references
    }
}

/// The types of a struct's fields, or of the payloads of an enum's cases.
fn item_types(item: &Item) -> Vec<&Type> {
    match item {
        Item::Struct(item) => item.fields.iter().map(|field| &field.ty).collect(),
        Item::Enum(item) => item
            .variants
            .iter()
            .flat_map(|variant| &variant.fields)
            .map(|field| &field.ty)
            .collect(),
        _ => Vec::new(),
    }
}

/// The types of a shape's fields, or of the payloads of its cases.
fn shape_types(shape: &Shape) -> Vec<&Type> {
    match &shape.kind {
        ShapeKind::Record(fields) => fields.iter().map(|(_, ty)| ty).collect(),
        ShapeKind::Variant(cases) => cases.iter().filter_map(|(_, ty)| ty.as_ref()).collect(),
        ShapeKind::Enum(_) | ShapeKind::Flags(_) => Vec::new(),
    }
}

/// What a derived interface `use`s from a consumed interface: its identity, and each name it takes
/// from there, a type or a resource, by its WIT name and by the Rust name the author writes, whose
/// kebab-case form names it in the derived interface.
pub(crate) struct Foreign {
    pub identity: String,
    pub names: Vec<(String, Ident)>,
}

impl Foreign {
    /// The `use` line of the interface's text: `use ns:name/interface@version.{entry};`, each name
    /// renamed with `as` where its WIT name is not the kebab-case form of its Rust name.
    pub(crate) fn text(&self) -> String {
        let names: Vec<String> = self
            .names
            .iter()
            .map(|(name, ident)| {
                let local = wit::kebab(ident);
                if *name == local {
                    wit::escaped(name)
                } else {
                    format!("{} as {}", wit::escaped(name), wit::escaped(&local))
                }
            })
            .collect();
        format!("  use {}.{{{}}};\n", self.identity, names.join(", "))
    }
}

/// Where a derived function, a type of the module, a derived hook's input or a state names an item
/// of a consumed interface.
#[derive(Clone)]
enum Place<'a> {
    /// A parameter of a derived function or constructor of the interface of the service
    /// `interface`.
    Param {
        function: &'a str,
        interface: &'a Ident,
    },
    /// The result of a derived function of the interface of the service `interface`.
    Result {
        function: &'a str,
        interface: &'a Ident,
    },
    /// A field or a case of `shape`, a type of the module that the interface of the service
    /// `interface` defines.
    Field {
        shape: &'a Ident,
        interface: &'a Ident,
    },
    /// The input of the derived hook `hook`, or a field or a case of the type `via` it reaches.
    Input {
        hook: &'a Ident,
        via: Option<&'a Ident>,
    },
    /// A field of the state `state`, or of the module type `via` its fields reach.
    State {
        state: &'a Ident,
        via: Option<Ident>,
    },
}

impl Place<'_> {
    /// What names the item, for the errors: "`Notes::read` returns".
    fn subject(&self) -> String {
        match self {
            Place::Param { function, .. } => format!("`{function}` takes"),
            Place::Result { function, .. } => format!("`{function}` returns"),
            Place::Field { shape, .. } => format!("`{shape}` holds"),
            Place::Input { hook, via: None } => format!("the input of the hook `{hook}` holds"),
            Place::Input {
                hook,
                via: Some(via),
            } => format!("`{via}`, which the input of the hook `{hook}` reaches, holds"),
            Place::State { state, via: None } => format!("the state `{state}` holds"),
            Place::State {
                state,
                via: Some(via),
            } => format!("the state `{state}` holds `{via}`, which holds"),
        }
    }
}

/// What the uses of the consumed interfaces are read from: the consumed modules and the directory
/// of the consumed packages, the module's services, resources and declared hooks, the types that
/// enter the WIT with the interface defining each, and the states with the module types that stay
/// out of the WIT, which a state's fields may reach.
pub(crate) struct Reach<'a> {
    pub modules: &'a Modules,
    pub dir: Option<&'a Path>,
    pub services: &'a [Service],
    pub resources: &'a [WitResource],
    pub declared: &'a [Declared],
    pub shapes: &'a [Shape],
    pub placement: &'a BTreeMap<String, Ident>,
    pub states: &'a [State],
    pub unreached: &'a [Candidate],
}

/// What each derived interface `use`s from the consumed interfaces, by the interface's WIT name,
/// once every item a consumed interface's module names is checked where it is named. A derived
/// function, a constructor and a type of the module that enters the WIT name the data types of an
/// interface imported, plainly or by key, or implemented, or of a hook the module contributes to,
/// and a parameter the handles of an imported interface's resources; a derived hook's input names
/// none; a state names the data types that `serde` can write. Nothing is read from the consumed
/// packages when nothing names them.
pub(crate) fn foreign(reach: Reach<'_>) -> syn::Result<BTreeMap<String, Vec<Foreign>>> {
    let Reach {
        modules,
        dir,
        services,
        resources,
        declared,
        shapes,
        placement,
        states,
        unreached,
    } = reach;
    let functions = resource_params::derived(services, resources);
    let mut found = Vec::new();
    for function in &functions {
        let (name, interface) = (function.name.as_str(), function.interface);
        for (_, ty, _) in function.params {
            let place = Place::Param {
                function: name,
                interface,
            };
            found_in(modules, ty, place, &mut found);
        }
        if let Some(ty) = function.result {
            let place = Place::Result {
                function: name,
                interface,
            };
            found_in(modules, ty, place, &mut found);
        }
    }
    let hooks: Vec<&Declared> = declared
        .iter()
        .filter(|declaration| declaration.input.is_some())
        .collect();
    for shape in shapes {
        let Some(interface) = placement.get(&shape.ident.to_string()) else {
            continue;
        };
        // A type a derived hook's input reaches is defined in the hook's interface.
        let place = match hooks.iter().find(|hook| hook.ident == *interface) {
            Some(hook) => Place::Input {
                hook: &hook.ident,
                via: Some(&shape.ident),
            },
            None => Place::Field {
                shape: &shape.ident,
                interface,
            },
        };
        for ty in shape_types(shape) {
            found_in(modules, ty, place.clone(), &mut found);
        }
    }
    for hook in &hooks {
        let (input, _) = hook.input.as_ref().expect("a derived hook");
        let place = Place::Input {
            hook: &hook.ident,
            via: None,
        };
        found_in(modules, input, place, &mut found);
    }
    for state in states {
        for (ty, via) in state_types(state, shapes, unreached, modules) {
            let place = Place::State {
                state: &state.ident,
                via: via.cloned(),
            };
            found_in(modules, ty, place, &mut found);
        }
    }
    if found.is_empty() {
        return Ok(BTreeMap::new());
    }
    let defined = wit::defined(
        dir,
        found
            .iter()
            .map(|(named, _)| named.interface.identity.as_str()),
    )?;
    let mut foreign: BTreeMap<String, Vec<Foreign>> = BTreeMap::new();
    for (named, place) in &found {
        let items = &defined[&named.interface.identity];
        let Some(definition) = items.get(named.ident) else {
            if named.interface.role == Role::Keyed
                && *named.ident == view_object(&named.interface.identity)
            {
                return Err(syn::Error::new(
                    named.ty.span(),
                    format!(
                        "{} `{}`, an object of {}: such an object belongs to the call that acquired it, so a derived interface takes and returns none",
                        place.subject(),
                        named.ident,
                        named.interface.described()
                    ),
                ));
            }
            return Err(syn::Error::new(
                named.ty.span(),
                format!(
                    "{} `{}`, which names no type or resource of {}: {}",
                    place.subject(),
                    named.ident,
                    named.interface.described(),
                    items.listed()
                ),
            ));
        };
        let Some(interface) = checked(named, definition, place, declared)? else {
            continue;
        };
        let uses = foreign.entry(wit::kebab(interface)).or_default();
        let index = match uses
            .iter()
            .position(|used| used.identity == named.interface.identity)
        {
            Some(index) => index,
            None => {
                uses.push(Foreign {
                    identity: named.interface.identity.clone(),
                    names: Vec::new(),
                });
                uses.len() - 1
            }
        };
        let names = &mut uses[index].names;
        if !names.iter().any(|(_, known)| known == named.ident) {
            names.push((definition.name.clone(), named.ident.clone()));
        }
    }
    Ok(foreign)
}

/// The type of the objects the view of a keyed import gives the interface `identity`, which takes
/// the interface's name: `Counter` for `example:counters/counter@1.0.0`.
fn view_object(identity: &str) -> Ident {
    let interface = identity.split_once('/').map_or(identity, |(_, rest)| rest);
    crate::type_ident(interface.split('@').next().unwrap_or(interface))
}

/// Each item `ty` names through a consumed interface's module, named at `place`.
fn found_in<'a, 'p>(
    modules: &'a Modules,
    ty: &'a Type,
    place: Place<'p>,
    found: &mut Vec<(Named<'a, 'a>, Place<'p>)>,
) {
    let mut named = Vec::new();
    modules.named_in(ty, &mut named);
    found.extend(named.into_iter().map(|named| (named, place.clone())));
}

/// Check one item a consumed interface's module names where it is named, and return the service
/// whose derived interface `use`s it, if one does.
fn checked<'a>(
    named: &Named<'_, '_>,
    definition: &wit::Definition,
    place: &Place<'a>,
    declared: &[Declared],
) -> syn::Result<Option<&'a Ident>> {
    let subject = place.subject();
    let ident = named.ident;
    let interface = named.interface;
    let of = interface.described();
    let refuse = |reason: String| -> syn::Result<Option<&'a Ident>> {
        Err(syn::Error::new(
            named.ty.span(),
            format!("{subject} `{ident}`, {reason}"),
        ))
    };
    let what = match &definition.kind {
        wit::Kind::Resource => "a resource",
        wit::Kind::Type { what, .. } => what,
    };
    // A state holds what the kernel keeps as JSON: data, which `serde` writes.
    if let Place::State { .. } = place {
        return match &definition.kind {
            wit::Kind::Resource => refuse(format!(
                "{what} of {of}: a state holds data, which the kernel keeps as JSON"
            )),
            wit::Kind::Type {
                carries: Some(carried),
                ..
            } => refuse(format!(
                "{what} of {of} that holds {carried}: a state holds data, which the kernel keeps as JSON"
            )),
            wit::Kind::Type {
                unwritable: Some(problem),
                ..
            } => {
                let holds = if what == *problem {
                    String::new()
                } else {
                    format!(" that holds {problem}")
                };
                refuse(format!(
                    "{what} of {of}{holds}: the bindings give it no `serde` implementation, and the kernel keeps a state as JSON"
                ))
            }
            wit::Kind::Type { .. } => Ok(None),
        };
    }
    if let Place::Input { .. } = place {
        return refuse(format!(
            "{what} of {of}: a derived hook's input holds the types of the module, not those of a consumed interface"
        ));
    }
    let (interface_of, returned) = match place {
        Place::Param { interface, .. } => (*interface, false),
        Place::Result { interface, .. } => (*interface, true),
        Place::Field { interface, .. } => (*interface, false),
        Place::Input { .. } | Place::State { .. } => unreachable!("checked above"),
    };
    match (interface.role, &definition.kind) {
        (Role::Wasi, wit::Kind::Resource) => refuse(format!(
            "{what} of {of}: the runtime moves no WASI handle between Orbs"
        )),
        (Role::Wasi, wit::Kind::Type { .. }) => refuse(format!(
            "{what} of {of}: the types of a WASI interface stay out of a derived interface; use a type of the module"
        )),
        (Role::Declared, _) => {
            let run = declared
                .iter()
                .find(|declaration| declaration.hook == interface.identity)
                .map(|declaration| format!("`{}::run`", declaration.ident))
                .unwrap_or_else(|| "`run`".to_owned());
            let module = named.module;
            refuse(format!(
                "{what} of {of}: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `{module}::{ident}` and {run} are the import's; use a type of the module"
            ))
        }
        (Role::Imported, wit::Kind::Resource) => match place {
            Place::Param { .. } => Ok(Some(interface_of)),
            Place::Result { .. } => refuse(format!(
                "{what} of {of}: the runtime returns a resource only from the interface that defines it, so a derived function takes such a handle but returns none"
            )),
            _ => refuse(format!(
                "{what} of {of}: a derived function takes such a handle as a parameter, alone or in an option, a result, a list or a tuple, but a record or a variant of the module holds none"
            )),
        },
        (Role::Implemented | Role::Contributed, wit::Kind::Resource) if returned => {
            refuse(format!(
                "{what} of {of}: the runtime returns a resource only from the interface that defines it"
            ))
        }
        (Role::Implemented | Role::Contributed, wit::Kind::Resource) => refuse(format!(
            "{what} of {of}: a derived function takes the resources of the interfaces this module derives and the handles of the interfaces it imports"
        )),
        (Role::Keyed, wit::Kind::Resource) => refuse(format!(
            "{what} of {of}: the objects of a view carry data, so an interface with a resource is not imported by key"
        )),
        (
            _,
            wit::Kind::Type {
                carries: Some(carried),
                ..
            },
        ) => refuse(format!(
            "{what} of {of} that holds {carried}: a derived interface names the consumed types that carry data only"
        )),
        (_, wit::Kind::Type { .. }) => Ok(Some(interface_of)),
    }
}

/// The types of a state's fields and of the module types they reach, each once, with the module
/// type that holds it, `None` for the state's own fields. A name resolves as `serialized` resolves
/// it: to the type of the module naming it, else to the one type of that name in the Orb.
fn state_types<'a>(
    state: &State,
    shapes: &'a [Shape],
    unreached: &'a [Candidate],
    modules: &Modules,
) -> Vec<(&'a Type, Option<&'a Ident>)> {
    // Each type of the module, by its name and its module, with the types it holds.
    let declared: Vec<(&'a Ident, usize, Vec<&'a Type>)> = shapes
        .iter()
        .map(|shape| (&shape.ident, shape.scope, shape_types(shape)))
        .chain(unreached.iter().map(|candidate| {
            (
                &candidate.ident,
                candidate.scope,
                item_types(&candidate.item),
            )
        }))
        .collect();
    let resolve = |name: &Ident, scope: usize| {
        let named: Vec<&(&'a Ident, usize, Vec<&'a Type>)> = declared
            .iter()
            .filter(|(ident, _, _)| *ident == name)
            .collect();
        match named.iter().find(|(_, module, _)| *module == scope) {
            Some(found) => Some(*found),
            None if named.len() == 1 => Some(named[0]),
            None => None,
        }
    };
    let mut types = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending = vec![(state.ident.clone(), state.scope, true)];
    while let Some((name, scope, own)) = pending.pop() {
        let Some((ident, scope, held)) = resolve(&name, scope) else {
            continue;
        };
        if !seen.insert((ident.to_string(), *scope)) {
            continue;
        }
        let via = (!own).then_some(*ident);
        for ty in held {
            types.push((*ty, via));
            let mut references = Vec::new();
            modules.references(ty, &mut references);
            pending.extend(
                references
                    .into_iter()
                    .map(|reference| (reference, *scope, false)),
            );
        }
    }
    types
}
