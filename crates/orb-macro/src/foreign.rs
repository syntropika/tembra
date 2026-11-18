//! What a derived interface uses from the interfaces the Orb consumes. A derived signature names
//! a type of such an interface through the module the Orb module re-exports for it:
//! `greeting::Entry` for `import = "example:greeter/greeting@1.0.0"`, `counter_ref::Tally` for a
//! type the view of an interface imported by key uses, `book::Entry` for an implemented
//! `#[service("example:ledger/book@1.0.0")]`, `guard::Admission` for a hook the module contributes
//! to. So does a field or a case of a record or a variant of the module that enters the WIT, and a
//! parameter names the handle of an imported interface's resource the same way, `texts::Builder`,
//! itself or in a record or a variant of the module. Where the runtime carries a resource, into an
//! argument but out of a result only from the interface that defines it, is tembra-contracts' rule,
//! which the expansion checks of the derived WIT afterwards.
//! The derived interface `use`s each name from the interface that defines it, so its bindings name
//! the type the author's code holds: a value crosses as it is, with no copy and no conversion.
//!
//! The bindings put the types of an interface the world only imports in its import module, which
//! the Orb module re-exports, or which a keyed import's view re-exports them from. An interface
//! the world exports, an implemented one or a hook, gets a copy of its types in its export module,
//! but of those it `use`s from an interface the world only imports, which its import and its
//! export alias alike, and a derived interface, exported too, uses that copy: the Orb module
//! re-exports the export module of an implemented interface and of a contributed hook, but the
//! import module of a hook it declares, which `run` calls, so where the two copies differ a
//! derived interface names none of a declared hook's types, and none of an exported interface's,
//! an implemented one's or a hook's, through the view of its keyed import, the import's copy. A
//! WASI interface's names stay out of a derived interface, named through the WASI interface's
//! module or through a consumed interface that `use`s them, and a consumed type crosses one when it
//! carries data only. A state may hold a consumed type that `serde` can write, which the bindings
//! derive for it in an Orb with a state.
use crate::{
    Candidate, Contribution, Declared, Newtype, Service, Shape, ShapeKind, State, WitResource,
    hook_path, keyed, resource_params, wasi, wit,
};
use std::collections::{BTreeMap, BTreeSet};
use syn::{Ident, Item, LitStr, Type, spanned::Spanned};
use tembra_contracts::guest;

/// How the world holds a consumed interface, which decides what a derived interface names through
/// the interface's module.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Another Orb's interface, imported: its types, and its resources' handles, alone or in the
    /// records and variants of the module.
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
    /// The kernel's context interface, imported for the `context` option: it only keeps its name
    /// from the consumed interfaces, and a path through its module reaches no consumed name.
    Context,
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
            Role::Context => {
                format!("the kernel's interface {identity}, which the `context` option imports")
            }
        }
    }
}

/// The modules the Orb module re-exports for its consumed interfaces, by the name it re-exports
/// each under, `greeting`: the imported interfaces, the views of those imported by key, the
/// implemented ones, the hooks it contributes to and the hooks it declares from a consumed package,
/// and the kernel's context interface for the `context` option. With them, the modules of the Orb
/// itself, in which a path resolves first, and the newtypes of the module, which a path naming one
/// reaches through to its field.
pub(crate) struct Modules {
    reexported: BTreeMap<String, Reexported>,
    /// The path of each module of the Orb from the Orb module, by scope; scope 0 is the Orb module.
    paths: Vec<Vec<Ident>>,
    /// The newtypes of the module.
    newtypes: Vec<Newtype>,
    /// What each module of the Orb imports, by scope, which a path written there is read through.
    uses: Vec<crate::Uses>,
}

/// A name a type path reaches through a consumed interface's module, `greeting::Entry`: the
/// interface, the module's name, the name and the path itself, for the errors.
#[derive(Clone)]
pub(crate) struct Named<'m, 't> {
    pub interface: &'m Reexported,
    pub module: &'m str,
    /// The type's name in the interface, which a `use` may have imported under another.
    pub ident: Ident,
    pub ty: &'t Type,
}

/// Where a type path leads once `Modules::resolve` stops following it: what is left of the path.
struct Leads {
    segments: Vec<Ident>,
}

/// Where a type path written in a module of the Orb leads, as rustc resolves it there
/// (`Modules::resolve`).
enum Resolution {
    /// The name `ident` in the module of the Orb `scope`, which declares a type of the name, or
    /// declares and imports nothing of it.
    Module(Ident, usize),
    /// A path the macro follows no further: into a module it does not read, a crate's, a
    /// consumed interface's or the bindings'.
    Stopped(Leads),
    /// A form the macro does not follow, a glob import or a path through `crate::`, with how to
    /// write the name.
    Refused(String),
    /// An alias that is more than a path, which a contract type cannot name, with how to write it,
    /// and the alias's name in the module declaring it, by scope.
    Composite(String, Ident, usize),
}

/// What a walk through a type reaches: a name through a consumed interface's module, the name of
/// a type of the module, with the module of the Orb it resolves in, by scope (`Modules::resolve`),
/// `Vec` and `Entry` in `Vec<Entry>`, or a name the macro refuses to follow, with why and whether
/// the macro cannot follow it anywhere, as it cannot a glob import or a `crate::` path, rather than
/// only to a contract type, an alias that is more than a path or a type outside the Orb module.
enum Reached<'m, 't> {
    Consumed(Named<'m, 't>),
    Module(Ident, usize),
    Refused(syn::Error, bool),
}

impl Modules {
    /// The consumed interfaces of the module, each under the name of its re-exported module: an
    /// interface imported by key under its view's, and with `context`, the attribute's option, the
    /// kernel's context interface under `context`. `imports`
    /// are the attribute's literals, read as identities already; `paths` are the Orb's own modules
    /// and `uses` what each imports, by scope. Another interface under a name already taken is an
    /// error on it, naming both, and so is a second role of an interface, or one role twice, which
    /// the world cannot hold, an import of the kernel's context interface beside the option, and a
    /// plain import of an interface the module implements, which would make the world a hook's. An
    /// item of the Orb module under the name of a re-exported module, or of a module the bindings
    /// declare there, is left to rustc, which reports the name defined twice on the item.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        context: bool,
        imports: &[LitStr],
        keyed: &[String],
        services: &[Service],
        contributions: &[Contribution],
        declared: &[Declared],
        paths: &[Vec<Ident>],
        uses: &[crate::Uses],
    ) -> syn::Result<Self> {
        let mut modules = BTreeMap::new();
        // The option keeps no span: its name is taken first, so the error falls on the interface
        // re-exported under it, or on an import of the kernel's interface itself.
        if context {
            if let Some(import) = imports
                .iter()
                .find(|import| import.value() == guest::CONTEXT)
            {
                return Err(syn::Error::new(
                    import.span(),
                    format!(
                        "the `context` option imports the kernel's interface {} already, which the Orb module re-exports as `context`; remove this import",
                        guest::CONTEXT
                    ),
                ));
            }
            let kernel = Reexported {
                identity: guest::CONTEXT.to_owned(),
                role: Role::Context,
            };
            modules.insert("context".to_owned(), kernel);
        }
        // Two views of one name are the error of `keyed::views`, which names their interfaces.
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
            .map(|literal| {
                let identity = literal.value();
                let role = if wasi::is_wasi(&identity) {
                    Role::Wasi
                } else {
                    Role::Imported
                };
                (identity, role, literal.span())
            })
            .chain(services.iter().filter_map(|service| {
                let identity = service.implements.clone()?;
                Some((identity, Role::Implemented, service.ident.span()))
            }))
            .chain(contributions.iter().map(|contribution| {
                let identity = contribution.hook.clone();
                (identity, Role::Contributed, contribution.ident.span())
            }))
            .chain(
                declared
                    .iter()
                    .filter(|declaration| declaration.input.is_none())
                    .map(|declaration| {
                        let identity = declaration.hook.clone();
                        (identity, Role::Declared, declaration.ident.span())
                    }),
            );
        for (identity, role, span) in roles {
            // A world that imports an interface itself and exports it is a hook to `cargo tembra
            // build` and the kernel, and the Orb module would re-export both the import and the
            // export under the interface's name.
            if role == Role::Imported
                && services
                    .iter()
                    .any(|service| service.implements.as_ref() == Some(&identity))
            {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "{identity} is implemented by this module and imported plainly: a world that imports and exports one interface is a hook; reach another provider's objects with import(\"{identity}\", keyed), or contribute to or declare the hook with #[hook(\"{identity}\", ..)]"
                    ),
                ));
            }
            let [_, _, interface] = hook_path(&identity);
            let name = interface.to_string();
            let second = Reexported { identity, role };
            let Some(first) = modules.get(&name) else {
                modules.insert(name, second);
                continue;
            };
            // The world holds an interface in one role, which the Orb module re-exports once: a
            // second role, or one role twice, would import or export the interface twice. Any
            // other pair of one name is two modules the Orb module cannot both re-export.
            let identity = &second.identity;
            let verb = |role| match role {
                Role::Contributed => "contributes to",
                _ => "declares",
            };
            let message = match (first.identity == *identity, first.role, second.role) {
                (true, Role::Imported, Role::Imported) | (true, Role::Wasi, Role::Wasi) => format!(
                    "{identity} is imported twice: the world imports an interface once; remove this import"
                ),
                (true, Role::Imported, Role::Contributed | Role::Declared) => format!(
                    "{identity} is imported plainly and a hook this module {}: the world imports and exports the hook already, and the Orb module re-exports the hook's module as `{name}`; remove the import",
                    verb(second.role)
                ),
                (true, Role::Implemented, Role::Contributed | Role::Declared) => {
                    let verb = verb(second.role);
                    format!(
                        "{identity} is implemented by this module and a hook it {verb}: the world exports the interface once, for the service or for the hook; an Orb implements an interface or {verb} it as a hook, not both"
                    )
                }
                (true, Role::Implemented, Role::Implemented) => format!(
                    "{identity} is implemented twice by this module: the world exports the interface once; implement it with one service"
                ),
                _ => format!(
                    "the Orb module would re-export two modules as `{name}`: that of {}, and that of {}; keep one of them",
                    second.described(),
                    first.described()
                ),
            };
            return Err(syn::Error::new(span, message));
        }
        Ok(Self {
            reexported: modules,
            paths: paths.to_vec(),
            newtypes: Vec::new(),
            uses: uses.to_vec(),
        })
    }

    /// The same modules, with the newtypes of the module.
    pub(crate) fn with_newtypes(mut self, newtypes: &[Newtype]) -> Self {
        self.newtypes = newtypes.to_vec();
        self
    }

    /// The newtype a type written in the module `scope` names, when it names one: a path without
    /// type parameters that resolves to the newtype (`resolve`).
    pub(crate) fn newtype(&self, ty: &Type, scope: usize) -> Option<&Newtype> {
        let Type::Path(path) = ty else {
            return None;
        };
        let last = path.path.segments.last()?;
        if path.qself.is_some() || !matches!(last.arguments, syn::PathArguments::None) {
            return None;
        }
        let Resolution::Module(ident, module) = self.resolve(&path.path, scope) else {
            return None;
        };
        self.newtypes
            .iter()
            .find(|newtype| newtype.ident == ident && newtype.scope == module)
    }

    /// The newtypes a type written in the module `scope` names: itself, or inside the type
    /// parameters of a path and the elements of a tuple, but not inside a newtype's field.
    pub(crate) fn newtypes_in(&self, ty: &Type, scope: usize) -> Vec<&Newtype> {
        if let Some(newtype) = self.newtype(ty, scope) {
            return vec![newtype];
        }
        match ty {
            Type::Path(path) => match path.path.segments.last().map(|last| &last.arguments) {
                Some(syn::PathArguments::AngleBracketed(arguments)) => arguments
                    .args
                    .iter()
                    .flat_map(|argument| match argument {
                        syn::GenericArgument::Type(ty) => self.newtypes_in(ty, scope),
                        _ => Vec::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            },
            Type::Tuple(tuple) => tuple
                .elems
                .iter()
                .flat_map(|element| self.newtypes_in(element, scope))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Whether a type written in the module `scope` names a newtype, itself or inside its type
    /// parameters and tuples.
    pub(crate) fn holds_newtype(&self, ty: &Type, scope: usize) -> bool {
        !self.newtypes_in(ty, scope).is_empty()
    }

    /// The path of the module `scope` from the Orb module.
    pub(crate) fn path(&self, scope: usize) -> &[Ident] {
        &self.paths[scope]
    }

    /// How many modules the module `scope` is below the Orb module.
    pub(crate) fn depth(&self, scope: usize) -> usize {
        self.paths[scope].len()
    }

    /// The trait of the Orb module through which a newtype crosses the bindings, as the module
    /// `scope` names it.
    pub(crate) fn newtype_trait(&self, scope: usize) -> proc_macro2::TokenStream {
        let root = crate::supers(self.depth(scope));
        let name = quote::format_ident!("{}", crate::NEWTYPE);
        quote::quote! { #root #name }
    }

    /// The path of the trait `IMPORT_NEWTYPE`, which the Orb module declares, from the module
    /// `scope`.
    pub(crate) fn import_newtype_trait(&self, scope: usize) -> proc_macro2::TokenStream {
        let root = crate::supers(self.depth(scope));
        let name = quote::format_ident!("{}", crate::IMPORT_NEWTYPE);
        quote::quote! { #root #name }
    }

    /// The WIT spelling of a type written in the module `scope`: a newtype as its field's type. A
    /// newtype whose field holds it, through other newtypes too, has none.
    pub(crate) fn spelled(&self, ty: &Type, scope: usize) -> syn::Result<String> {
        self.spelled_within(ty, scope, &[])
    }

    /// The WIT spelling of a type written in the module `scope`, inside the fields of the newtypes
    /// `within`, each by its name and its module, which the type may not name again.
    fn spelled_within(
        &self,
        ty: &Type,
        scope: usize,
        within: &[(&Ident, usize)],
    ) -> syn::Result<String> {
        wit::wit_type_with(ty, &|named| {
            let Some(newtype) = self.newtype(named, scope) else {
                return Ok(self.wit_name(named, scope));
            };
            if within.contains(&(&newtype.ident, newtype.scope)) {
                return Err(syn::Error::new(
                    named.span(),
                    format!(
                        "`{}` holds itself through its field, which WIT cannot spell: a type that enters the WIT holds no type that holds it",
                        newtype.ident
                    ),
                ));
            }
            let mut within = within.to_vec();
            within.push((&newtype.ident, newtype.scope));
            self.spelled_within(&newtype.inner, newtype.scope, &within)
                .map(Some)
        })
    }

    /// The WIT name of a type written in the module `scope`, by the name of the type it resolves
    /// to, `entry` for `Item` after `use model::Entry as Item;`: a type of the module or of a
    /// consumed interface. `None` for a name the macro does not resolve.
    fn wit_name(&self, ty: &Type, scope: usize) -> Option<String> {
        let ident = match self.named(ty, scope) {
            Some(named) => named.ident,
            None => {
                let Type::Path(path) = ty else {
                    return None;
                };
                let Resolution::Module(ident, _) = self.resolve(&path.path, scope) else {
                    return None;
                };
                ident
            }
        };
        Some(wit::escaped(&wit::kebab(&ident)))
    }

    /// The name a type reaches through a consumed interface's module, when it is such a path:
    /// `greeting::Entry`, any path ending so, or a name a `use` or an alias leads there, written
    /// in the module `scope` (`resolve`). A path that leads into a module of the Orb names a type
    /// of the module, whatever that module is called: the author's `greeting::Banner` next to a
    /// `mod greeting` of their own resolves there first. The kernel's context module reaches none.
    pub(crate) fn named<'m, 't>(&'m self, ty: &'t Type, scope: usize) -> Option<Named<'m, 't>> {
        let Type::Path(path) = ty else {
            return None;
        };
        let written = path.path.segments.last()?;
        if path.qself.is_some() || !matches!(written.arguments, syn::PathArguments::None) {
            return None;
        }
        let Resolution::Stopped(leads) = self.resolve(&path.path, scope) else {
            return None;
        };
        let [.., module, item] = leads.segments.as_slice() else {
            return None;
        };
        let (module, interface) = self.reexported.get_key_value(&module.to_string())?;
        if interface.role == Role::Context {
            return None;
        }
        let mut ident = item.clone();
        ident.set_span(written.ident.span());
        Some(Named {
            interface,
            module,
            ident,
            ty,
        })
    }

    /// The type of the Orb's modules a type written in the module `scope` names, by its own name
    /// and the module declaring it, when its path resolves to one (`resolve`): `Entry` of `model`
    /// for `Item` after `use super::model::Entry as Item;`.
    pub(crate) fn module_type(&self, ty: &Type, scope: usize) -> Option<(Ident, usize)> {
        let Type::Path(path) = ty else {
            return None;
        };
        if path.qself.is_some() {
            return None;
        }
        match self.resolve(&path.path, scope) {
            Resolution::Module(ident, module) => Some((ident, module)),
            Resolution::Stopped(_) | Resolution::Refused(_) | Resolution::Composite(..) => None,
        }
    }

    /// Where a type path written in the module `scope` leads, as rustc resolves it inside the Orb
    /// module: through `self::`, `super::` and the sub-modules of the Orb, the names the `use`
    /// declarations of each module it passes bind, with `as` and in groups, a sub-module's
    /// `pub use` included, and the aliases that are only a path, `type Item = model::Entry;`, to
    /// the type's own name in the module that declares it, or to a module that declares nothing
    /// of the name. It stops at a crate or a module above the Orb module, and at a name nothing it
    /// reads binds, such as the modules the bindings declare in the Orb module and the consumed
    /// interfaces' modules the Orb module re-exports. A glob import, a path through `crate::` and
    /// an alias that is more than a path are refused, each with how to write the name.
    fn resolve(&self, path: &syn::Path, scope: usize) -> Resolution {
        self.resolve_within(path, scope, 0)
    }

    /// `resolve`, `depth` aliases deep: aliases naming one another in a circle would never end.
    fn resolve_within(&self, path: &syn::Path, scope: usize, depth: usize) -> Resolution {
        let mut segments: Vec<Ident> = path
            .segments
            .iter()
            .map(|segment| segment.ident.clone())
            .collect();
        let outside = |segments: Vec<Ident>| Resolution::Stopped(Leads { segments });
        if path.leading_colon.is_some() {
            return outside(segments);
        }
        let written = path.segments.last().map(|segment| &segment.ident);
        let mut at = scope;
        // Each step consumes a name or replaces one by what a declaration binds it to;
        // declarations binding one another in a circle would never end.
        for _ in 0..64 {
            let Some((first, rest)) = segments.split_first() else {
                break;
            };
            let (first, rest) = (first.clone(), rest.to_vec());
            let name = first.to_string();
            let uses = &self.uses[at];
            if rest.is_empty() {
                if let Some(alias) = uses.aliases.get(&name) {
                    return self.aliased(alias, at, depth);
                }
                if uses.items.contains(&name) {
                    return Resolution::Module(first, at);
                }
            }
            if first == "self" && !rest.is_empty() {
                segments = rest;
                continue;
            }
            if first == "super" && !rest.is_empty() {
                let Some(parent) = crate::parent(at, &self.paths) else {
                    return outside(segments);
                };
                at = parent;
                segments = rest;
                continue;
            }
            if first == "crate" {
                let written = written.map_or_else(String::new, Ident::to_string);
                return Resolution::Refused(format!(
                    "`{written}` is named through `crate::`, which the macro cannot place in the Orb module; name it from this module, through `self::`, `super::` or a `use` of the Orb's modules, `use super::model::{written};`"
                ));
            }
            if first == "Self" {
                return outside(segments);
            }
            if let Some(bound) = uses.names.get(&name) {
                // `use name;` brings the crate `name` into scope.
                if bound.global || (bound.segments.len() == 1 && bound.segments[0] == first) {
                    return outside(bound.segments.iter().cloned().chain(rest).collect());
                }
                segments = bound.segments.iter().cloned().chain(rest).collect();
                continue;
            }
            if rest.is_empty() {
                if !wit::BUILTIN.contains(&name.as_str())
                    && let Some(module) = crate::through_globs(&first, at, &self.uses, &self.paths)
                {
                    return Resolution::Refused(self.globbed(&first, at, module));
                }
                return Resolution::Module(first, at);
            }
            let mut child = self.paths[at].clone();
            child.push(first.clone());
            if let Some(index) = self.paths.iter().position(|module| *module == child) {
                at = index;
                segments = rest;
                continue;
            }
            if let Some(module) = crate::through_globs(&first, at, &self.uses, &self.paths) {
                return Resolution::Refused(self.globbed(&first, at, module));
            }
            break;
        }
        Resolution::Stopped(Leads { segments })
    }

    /// Where the alias `alias` of the module `scope` leads (`resolve`): an alias that is only a
    /// path, without type parameters, to the type it names, a type of the Orb's modules or of a
    /// consumed interface; to itself, a type of the module, when its path names neither, as
    /// `type Token = u32;` does. One that is more than a path is refused.
    fn aliased(&self, alias: &syn::ItemType, scope: usize, depth: usize) -> Resolution {
        let ident = &alias.ident;
        let Type::Path(target) = &*alias.ty else {
            return more_than_a_path(alias, scope);
        };
        if !alias.generics.params.is_empty()
            || target.qself.is_some()
            || target
                .path
                .segments
                .iter()
                .any(|segment| !segment.arguments.is_none())
        {
            return more_than_a_path(alias, scope);
        }
        if depth >= 64 {
            return Resolution::Module(ident.clone(), scope);
        }
        match self.resolve_within(&target.path, scope, depth + 1) {
            Resolution::Module(name, module)
                if self.uses[module].items.contains(&name.to_string()) =>
            {
                Resolution::Module(name, module)
            }
            Resolution::Stopped(leads)
                if matches!(
                    leads.segments.as_slice(),
                    [.., module, _] if self.reexported.contains_key(&module.to_string())
                ) =>
            {
                Resolution::Stopped(leads)
            }
            Resolution::Refused(why) => Resolution::Refused(why),
            Resolution::Composite(why, alias, module) => Resolution::Composite(why, alias, module),
            _ => Resolution::Module(ident.clone(), scope),
        }
    }

    /// Why `name`, written in the module `scope`, is not followed through the glob import that
    /// brings it from the module `module`, with the `use` that imports it by name.
    fn globbed(&self, name: &Ident, scope: usize, module: usize) -> String {
        format!(
            "`{name}` comes into this module through a glob import, which the macro does not follow for a contract type; import it by name, `use {}::{name};`",
            self.path_to(scope, module)
        )
    }

    /// The path from the module `from` to the module `to`, both by scope, as a `use` written in
    /// `from` spells it: `super::model`, `self::model`, `super`.
    pub(crate) fn path_to(&self, from: usize, to: usize) -> String {
        let (from, to) = (&self.paths[from], &self.paths[to]);
        let shared = from
            .iter()
            .zip(to)
            .take_while(|(left, right)| left == right)
            .count();
        let up = from.len() - shared;
        let mut segments: Vec<String> = if up == 0 {
            vec!["self".to_owned()]
        } else {
            vec!["super".to_owned(); up]
        };
        segments.extend(to[shared..].iter().map(Ident::to_string));
        segments.join("::")
    }

    /// Walk a type written in the module `scope`: itself, then the type parameters of a path and
    /// the elements of a tuple. Each name a consumed interface's module reaches is reported, and
    /// not looked into; so is the name of every other path, before its type parameters, with the
    /// module of the Orb it resolves in (`resolve`): the type's own name in the module declaring
    /// it, or the writing module for a name of a crate's, such as `Vec`. A name declared outside
    /// the Orb module, or one the macro refuses to follow, is reported with why. A type with a WIT
    /// spelling of its own is not a name of the module: free JSON is not looked into, and a
    /// newtype, which the WIT spells as its field, is walked as its field, in the newtype's
    /// module, once.
    fn walk<'m, 't>(&'m self, ty: &'t Type, scope: usize, visit: &mut impl FnMut(Reached<'m, 't>))
    where
        'm: 't,
    {
        self.walk_within(ty, scope, visit, &mut Vec::new());
    }

    /// `walk`, inside the fields of the newtypes `within`, each by its name and its module, which
    /// are not read again.
    fn walk_within<'m, 't>(
        &'m self,
        ty: &'t Type,
        scope: usize,
        visit: &mut impl FnMut(Reached<'m, 't>),
        within: &mut Vec<(&'m Ident, usize)>,
    ) where
        'm: 't,
    {
        if let Some(named) = self.named(ty, scope) {
            visit(Reached::Consumed(named));
            return;
        }
        if wit::free_json(ty) {
            return;
        }
        if let Some(newtype) = self.newtype(ty, scope) {
            let key = (&newtype.ident, newtype.scope);
            if !within.contains(&key) {
                within.push(key);
                self.walk_within(&newtype.inner, newtype.scope, visit, within);
                within.pop();
            }
            return;
        }
        match ty {
            Type::Path(path) => {
                let Some(segment) = path.path.segments.last() else {
                    return;
                };
                let builtin = wit::BUILTIN.contains(&segment.ident.to_string().as_str());
                match self.resolve(&path.path, scope) {
                    Resolution::Module(mut ident, module) => {
                        ident.set_span(segment.ident.span());
                        visit(Reached::Module(ident, module));
                    }
                    Resolution::Stopped(_) if builtin => {
                        visit(Reached::Module(segment.ident.clone(), scope));
                    }
                    Resolution::Stopped(_) => {
                        let ident = &segment.ident;
                        // Free JSON imported by `use` reads as such a name: it is free JSON only
                        // by its path.
                        let hint = if ident == "Value" {
                            "; for free JSON, write it by its path, `serde_json::Value`"
                        } else {
                            ""
                        };
                        visit(Reached::Refused(
                            syn::Error::new_spanned(
                                path,
                                format!(
                                    "`{ident}` is declared outside the Orb module, or in a module the macro does not read: a contract type is declared inside the Orb module, or is a consumed interface's type, named through its module, `greeting::Entry`, or a `use` of it{hint}"
                                ),
                            ),
                            false,
                        ));
                    }
                    Resolution::Refused(why) => {
                        visit(Reached::Refused(syn::Error::new_spanned(path, why), true));
                    }
                    Resolution::Composite(why, ..) => {
                        visit(Reached::Refused(syn::Error::new_spanned(path, why), false));
                    }
                }
                if let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments {
                    for argument in &arguments.args {
                        if let syn::GenericArgument::Type(ty) = argument {
                            self.walk_within(ty, scope, visit, within);
                        }
                    }
                }
            }
            Type::Tuple(tuple) => {
                for element in &tuple.elems {
                    self.walk_within(element, scope, visit, within);
                }
            }
            Type::Array(array) => self.walk_within(&array.elem, scope, visit, within),
            _ => {}
        }
    }

    /// The types of the aliases that are more than a path, `type Items = Vec<catalog::Item>;`,
    /// which a type written in the module `scope` names, itself or inside the type parameters of a
    /// path, the elements of a tuple and an array, however the name reaches the alias, through a
    /// module path or a `use` included, each with the module declaring the alias, where its type
    /// is written: `resolve` follows none of them, and a state's types are followed into them
    /// (`state_types`).
    fn composite_aliases(&self, ty: &Type, scope: usize) -> Vec<(&Type, usize)> {
        match ty {
            Type::Path(path) => {
                let mut found = Vec::new();
                if path.qself.is_none()
                    && let Resolution::Composite(_, alias, module) = self.resolve(&path.path, scope)
                    && let Some(alias) = self.uses[module].aliases.get(&alias.to_string())
                {
                    found.push((&*alias.ty, module));
                }
                if let Some(syn::PathArguments::AngleBracketed(arguments)) =
                    path.path.segments.last().map(|segment| &segment.arguments)
                {
                    for argument in &arguments.args {
                        if let syn::GenericArgument::Type(ty) = argument {
                            found.extend(self.composite_aliases(ty, scope));
                        }
                    }
                }
                found
            }
            Type::Tuple(tuple) => tuple
                .elems
                .iter()
                .flat_map(|ty| self.composite_aliases(ty, scope))
                .collect(),
            Type::Array(array) => self.composite_aliases(&array.elem, scope),
            _ => Vec::new(),
        }
    }

    /// Refuse a type written in the module `scope` that names, itself or inside its type
    /// parameters and tuples, a type the macro does not follow (`resolve`): through a glob
    /// import, `crate::` or an alias that is more than a path, or declared outside the Orb module.
    pub(crate) fn resolved(&self, ty: &Type, scope: usize) -> syn::Result<()> {
        self.refused(ty, scope, false)
    }

    /// Refuse a type written in the module `scope` that names a type through a form the macro
    /// cannot follow anywhere, a glob import or `crate::`, as in a state's fields, which may hold
    /// any type, but whose module types the macro follows to what they hold.
    fn followed(&self, ty: &Type, scope: usize) -> syn::Result<()> {
        self.refused(ty, scope, true)
    }

    /// The first refusal of a type written in the module `scope` (`resolved`), only of the forms
    /// the macro cannot follow anywhere when `unfollowed`.
    fn refused(&self, ty: &Type, scope: usize, unfollowed: bool) -> syn::Result<()> {
        let mut refused = None;
        self.walk(ty, scope, &mut |reached| {
            if let Reached::Refused(error, anywhere) = reached
                && (anywhere || !unfollowed)
            {
                refused.get_or_insert(error);
            }
        });
        refused.map_or(Ok(()), Err)
    }

    /// `resolved`, for each type the fields of a struct or the payloads of an enum's cases hold,
    /// the item declared in the module `scope`.
    pub(crate) fn item_resolved(&self, item: &Item, scope: usize) -> syn::Result<()> {
        item_types(item)
            .into_iter()
            .try_for_each(|ty| self.resolved(ty, scope))
    }

    /// Each name a type written in the module `scope` reaches through a consumed interface's
    /// module: itself, or inside the type parameters of a path and the elements of a tuple.
    pub(crate) fn named_in<'m, 't>(
        &'m self,
        ty: &'t Type,
        scope: usize,
        out: &mut Vec<Named<'m, 't>>,
    ) where
        'm: 't,
    {
        self.walk(ty, scope, &mut |reached| {
            if let Reached::Consumed(named) = reached {
                out.push(named);
            }
        });
    }

    /// The module types a type written in the module `scope` mentions: the last name of each path,
    /// but the types with a WIT spelling of their own, looking through type parameters and tuples,
    /// and none of the names a consumed interface's module reaches, which are no types of the
    /// module.
    pub(crate) fn references(&self, ty: &Type, scope: usize, out: &mut Vec<Ident>) {
        let mut placed = Vec::new();
        self.references_in(ty, scope, &mut placed);
        out.extend(placed.into_iter().map(|(ident, _)| ident));
    }

    /// `references`, each name with the module of the Orb the path names its type in, by scope:
    /// the module a path leads into, `sessions` for `super::sessions::Token` written in `tickets`,
    /// or the writing module for a bare name, where the name resolves as in Rust.
    pub(crate) fn references_in(&self, ty: &Type, scope: usize, out: &mut Vec<(Ident, usize)>) {
        self.walk(ty, scope, &mut |reached| {
            if let Reached::Module(ident, module) = reached
                && !wit::BUILTIN.contains(&ident.to_string().as_str())
            {
                out.push((ident, module));
            }
        });
    }

    /// The module the Orb module re-exports for the interface `identity` when the world exports
    /// it, with the role the module holds it in: `counter` for an implemented
    /// `example:counters/counter@1.0.0`, the export module, whose types a derived interface takes,
    /// as for a hook the module contributes to; the import module for a hook it declares.
    fn exporting(&self, identity: &str) -> Option<(&str, Role)> {
        self.reexported
            .iter()
            .find(|(_, reexported)| {
                matches!(
                    reexported.role,
                    Role::Implemented | Role::Contributed | Role::Declared
                ) && reexported.identity == identity
            })
            .map(|(module, reexported)| (module.as_str(), reexported.role))
    }

    /// The module types the fields of a struct or the payloads of an enum's cases mention, the
    /// item declared in the module `scope`, each with the module of the Orb the field's path names
    /// it in (`references_in`).
    pub(crate) fn item_references(&self, item: &Item, scope: usize) -> Vec<(Ident, usize)> {
        let mut references = Vec::new();
        for ty in item_types(item) {
            self.references_in(ty, scope, &mut references);
        }
        references
    }
}

/// Why the alias `alias` of the module `scope`, more than a path, does not name a contract type.
fn more_than_a_path(alias: &syn::ItemType, scope: usize) -> Resolution {
    let ident = &alias.ident;
    let ty = quote::ToTokens::to_token_stream(&alias.ty).to_string();
    Resolution::Composite(
        format!(
            "`{ident}` is an alias of `{ty}`, which is more than a path: the macro follows an alias of a contract type only when it names the type by its path, `type {ident} = model::Entry;`; write `{ty}` where `{ident}` stands"
        ),
        ident.clone(),
        scope,
    )
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
pub(crate) fn shape_types(shape: &Shape) -> Vec<&Type> {
    match &shape.kind {
        ShapeKind::Record(fields) => fields.iter().map(|(_, ty)| ty).collect(),
        ShapeKind::Variant(cases) => cases
            .iter()
            .flat_map(|(_, payload)| payload.types())
            .collect(),
        ShapeKind::Enum(_) => Vec::new(),
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

/// What the uses of the consumed interfaces are read from: the consumed modules and the packages
/// the expansion parses, the module's services, resources, contributions and declared hooks, the
/// types that enter the WIT with the interface defining each, and the states with the module types
/// that stay out of the WIT, which a state's fields may reach.
pub(crate) struct Reach<'a> {
    pub modules: &'a Modules,
    pub parsed: &'a wit::Parsed,
    pub services: &'a [Service],
    pub resources: &'a [WitResource],
    pub contributions: &'a [Contribution],
    pub declared: &'a [Declared],
    pub shapes: &'a [Shape],
    pub placement: &'a BTreeMap<String, Ident>,
    pub states: &'a [State],
    pub unreached: &'a [Candidate],
}

/// What the module reaches of the consumed interfaces (`foreign`).
#[derive(Default)]
pub(crate) struct Consumption {
    /// What each derived interface `use`s from the consumed interfaces, by the interface's WIT
    /// name.
    pub uses: BTreeMap<String, Vec<Foreign>>,
    /// The consumed records, variants and enums a state holds, directly or through the types of
    /// the module, and those they hold in turn, by full name, `ns:name/interface@version/type`:
    /// the bindings derive serde's traits on these alone.
    pub serialized: BTreeSet<String>,
}

/// What each derived interface `use`s from the consumed interfaces, by the interface's WIT name,
/// and the consumed types a state holds (`Consumption`), once every item a consumed interface's module
/// names is checked where it is named. A derived
/// function, a constructor and a type of the module that enters the WIT name the data types of an
/// interface imported, plainly or by key, or implemented, or of a hook the module contributes to,
/// and the handles of their resources, which tembra-contracts' check of the derived WIT refuses
/// where the runtime does not carry them; a derived hook's input names none; a state names the
/// data types that `serde` can write. Nothing is read from the consumed packages when nothing
/// names them.
pub(crate) fn foreign(reach: Reach<'_>) -> syn::Result<Consumption> {
    let Reach {
        modules,
        parsed,
        services,
        resources,
        contributions,
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
            found_in(modules, ty, function.scope, place, &mut found);
        }
        if let Some(ty) = function.result {
            let place = Place::Result {
                function: name,
                interface,
            };
            found_in(modules, ty, function.scope, place, &mut found);
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
            found_in(modules, ty, shape.scope, place.clone(), &mut found);
        }
    }
    for hook in &hooks {
        let (input, _) = hook.input.as_ref().expect("a derived hook");
        let place = Place::Input {
            hook: &hook.ident,
            via: None,
        };
        found_in(modules, input, hook.scope, place, &mut found);
    }
    for state in states {
        for (ty, via, scope) in state_types(state, shapes, unreached, modules)? {
            let place = Place::State {
                state: &state.ident,
                via: via.cloned(),
            };
            found_in(modules, ty, scope, place, &mut found);
        }
    }
    if found.is_empty() {
        return Ok(Consumption::default());
    }
    // The consumed interfaces the world exports: the implemented ones, the hooks the module
    // contributes to and those it declares from a consumed package.
    let exported: Vec<Reexported> = services
        .iter()
        .filter_map(|service| service.implements.clone())
        .map(|identity| (identity, Role::Implemented))
        .chain(
            contributions
                .iter()
                .map(|contribution| (contribution.hook.clone(), Role::Contributed)),
        )
        .chain(
            declared
                .iter()
                .filter(|declaration| declaration.input.is_none())
                .map(|declaration| (declaration.hook.clone(), Role::Declared)),
        )
        .map(|(identity, role)| Reexported { identity, role })
        .collect();
    let defined = wit::defined(
        parsed,
        found
            .iter()
            .map(|(named, _)| named.interface.identity.as_str()),
        &exported
            .iter()
            .map(|interface| interface.identity.as_str())
            .collect(),
    )?;
    let mut foreign: BTreeMap<String, Vec<Foreign>> = BTreeMap::new();
    let mut serialized = BTreeSet::new();
    for (named, place) in &found {
        let items = &defined[&named.interface.identity];
        // A view's module holds its resource, named after the interface, and re-exports the types
        // the view uses, whatever else the interface defines: rustc reports another name on the
        // path.
        if named.interface.role == Role::Keyed
            && named.ident == view_object(&named.interface.identity)
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
        let Some(definition) = items.get(&named.ident) else {
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
        let dependency = exported
            .iter()
            .find(|interface| items.depends.contains(&interface.identity));
        let checked = checked(named, definition, place, declared, dependency, modules)?;
        if let (
            Place::State { .. },
            wit::Kind::Type {
                serialized: held, ..
            },
        ) = (place, &definition.kind)
        {
            serialized.extend(held.iter().cloned());
        }
        let Some(interface) = checked else {
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
        if !names.iter().any(|(_, known)| *known == named.ident) {
            names.push((definition.name.clone(), named.ident.clone()));
        }
    }
    Ok(Consumption {
        uses: foreign,
        serialized,
    })
}

/// The type of the objects the view of a keyed import gives the interface `identity`, which takes
/// the interface's name: `Counter` for `example:counters/counter@1.0.0`.
fn view_object(identity: &str) -> Ident {
    crate::type_ident(crate::Identity::of(identity).interface.unwrap_or_default())
}

/// Each item `ty`, written in the module `scope`, names through a consumed interface's module,
/// named at `place`.
fn found_in<'a, 'p>(
    modules: &'a Modules,
    ty: &'a Type,
    scope: usize,
    place: Place<'p>,
    found: &mut Vec<(Named<'a, 'a>, Place<'p>)>,
) {
    let mut named = Vec::new();
    modules.named_in(ty, scope, &mut named);
    found.extend(named.into_iter().map(|named| (named, place.clone())));
}

/// Check one item a consumed interface's module names where it is named, and return the service
/// whose derived interface `use`s it, if one does. `dependency` is an interface the world exports
/// that the consumed interface depends on, if there is one.
fn checked<'a>(
    named: &Named<'_, '_>,
    definition: &wit::Definition,
    place: &Place<'a>,
    declared: &[Declared],
    dependency: Option<&Reexported>,
    modules: &Modules,
) -> syn::Result<Option<&'a Ident>> {
    let subject = place.subject();
    let ident = &named.ident;
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
    let interface_of = match place {
        Place::Param { interface, .. }
        | Place::Result { interface, .. }
        | Place::Field { interface, .. } => *interface,
        Place::Input { .. } | Place::State { .. } => unreachable!("checked above"),
    };
    // A path into the bindings' module of the import of an interface the world exports names the
    // import's copy of its types, as the view of its keyed import does, while a derived interface,
    // exported, takes the copy of the export, which the module the Orb module re-exports for an
    // implemented interface and a contributed hook names: rustc reports the mismatch on the
    // author's signature. A declared hook's module is the import's too, which its rule refuses
    // below.
    // The `run` of a hook the module declares, for the errors.
    let run = || {
        declared
            .iter()
            .find(|declaration| declaration.hook == interface.identity)
            .map(|declaration| format!("`{}::run`", declaration.ident))
            .unwrap_or_else(|| "`run`".to_owned())
    };
    // The view of an interface the world exports, one the module implements or a hook it
    // contributes to or declares, names the import's copy of the interface's types, while a
    // derived interface, exported, takes the copy of its export, which the implemented interface's
    // or the contributed hook's module names, so rustc reports the mismatch on the author's
    // signature; a declared hook's module names the import's too, so no path names the export's.
    // The two copies are one type where the export holds none of its own.
    if interface.role == Role::Keyed
        && definition.copied
        && let Some((module, Role::Declared)) = modules.exporting(&interface.identity)
    {
        let view = named.module;
        return refuse(format!(
            "{what} of {of} and declares: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `{view}::{ident}`, `{module}::{ident}` and {} are the import's; use a type of the module",
            run()
        ));
    }
    // A WASI interface's type or resource, named through the WASI interface's module or through
    // a consumed interface that `use`s it: the runtime carries neither between Orbs.
    if interface.role == Role::Wasi || definition.wasi.is_some() {
        let from = match &definition.wasi {
            Some(wasi) if interface.role != Role::Wasi => {
                format!(", which it uses from the WASI interface {wasi}")
            }
            _ => String::new(),
        };
        return match &definition.kind {
            wit::Kind::Resource => refuse(format!(
                "{what} of {of}{from}: the runtime moves no WASI handle between Orbs"
            )),
            wit::Kind::Type { .. } => refuse(format!(
                "{what} of {of}{from}: the types of a WASI interface stay out of a derived interface; use a type of the module"
            )),
        };
    }
    // A derived interface `use`s the item from the interface defining it, which the world then
    // imports with what it depends on, while an import of a world depends on none of its exports.
    if let (Role::Imported | Role::Keyed, Some(exported)) = (interface.role, dependency) {
        return refuse(format!(
            "{what} of {of}: that interface depends on {}, so a derived interface naming `{ident}` would make the world import an interface depending on one it exports, which WIT refuses; use a type of the module",
            exported.described()
        ));
    }
    match (interface.role, &definition.kind) {
        (Role::Wasi, _) => unreachable!("checked above"),
        (Role::Context, _) => unreachable!("`Modules::named` reaches no name of the context"),
        // The Orb module re-exports the import of a hook it declares, a copy of the hook's types
        // other than the export's, but for a type the hook `use`s from an interface the world only
        // imports, which both alias; a resource stays the hook's, as a contributed hook's does.
        (Role::Declared, _) if definition.copied => {
            let module = named.module;
            refuse(format!(
                "{what} of {of}: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `{module}::{ident}` and {} are the import's; use a type of the module",
                run()
            ))
        }
        // Where the runtime carries the handle is tembra-contracts' check of the derived WIT,
        // `check_profile`'s.
        (
            Role::Imported | Role::Implemented | Role::Contributed | Role::Declared,
            wit::Kind::Resource,
        ) => Ok(Some(interface_of)),
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
        (
            _,
            wit::Kind::Type {
                unsupported: Some(problem),
                ..
            },
        ) => refuse(format!(
            "{what} of {of}, which the runtime refuses in a signature: {problem}"
        )),
        (_, wit::Kind::Type { .. }) => Ok(Some(interface_of)),
    }
}

/// The types of a state's fields and of the module types they reach, each once, with the module
/// type that holds it, `None` for the state's own fields, and the module the holder is declared
/// in: a struct with named fields, an enum or a tuple struct of the module, newtype or not, and
/// the type of an alias that is more than a path, which a type names. A name resolves to the type
/// of the module that declares it; one the macro cannot follow, through a glob import or
/// `crate::`, is an error (`Modules::followed`).
fn state_types<'a>(
    state: &State,
    shapes: &'a [Shape],
    unreached: &'a [Candidate],
    modules: &'a Modules,
) -> syn::Result<Vec<(&'a Type, Option<&'a Ident>, usize)>> {
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
    // A struct with named fields or an enum of the module, else a tuple struct, by the name it
    // resolves to in its module, with the types it holds, each with the module it is written in.
    type Held<'h> = Vec<(&'h Type, usize)>;
    let resolve = |name: &Ident, scope: usize| -> Option<(&'a Ident, usize, Held<'a>)> {
        let named = declared
            .iter()
            .filter(|(ident, _, _)| *ident == name)
            .map(|entry| (entry.1, entry));
        if let Some((ident, scope, held)) = crate::declared_in(named, scope) {
            let held = held.iter().map(|ty| (*ty, *scope)).collect();
            return Some((ident, *scope, held));
        }
        let (ident, fields) = modules.uses[scope].tuples.get(&name.to_string())?;
        Some((ident, scope, fields.iter().map(|ty| (ty, scope)).collect()))
    };
    let mut types = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending = vec![(state.ident.clone(), state.scope, true)];
    while let Some((name, scope, own)) = pending.pop() {
        let Some((ident, scope, mut held)) = resolve(&name, scope) else {
            continue;
        };
        if !seen.insert((ident.to_string(), scope)) {
            continue;
        }
        let via = (!own).then_some(ident);
        // The types of the aliases the held types name are held too, each once, in the module
        // declaring the alias.
        let mut aliased: BTreeSet<*const Type> = BTreeSet::new();
        let mut next = 0;
        while let Some(&(ty, scope)) = held.get(next) {
            next += 1;
            modules.followed(ty, scope)?;
            types.push((ty, via, scope));
            let mut references = Vec::new();
            modules.references_in(ty, scope, &mut references);
            pending.extend(
                references
                    .into_iter()
                    .map(|(reference, scope)| (reference, scope, false)),
            );
            for alias in modules.composite_aliases(ty, scope) {
                if aliased.insert(alias.0) {
                    held.push(alias);
                }
            }
        }
    }
    Ok(types)
}
