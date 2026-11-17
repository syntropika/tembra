//! What a derived interface uses from the interfaces the Orb consumes. A derived signature names
//! a type of such an interface through the module the Orb module re-exports for it:
//! `greeting::Entry` for `import = "example:greeter/greeting@1.0.0"`, `counter_ref::Tally` for a
//! type the view of an interface imported by key uses, `book::Entry` for an implemented
//! `#[service("example:ledger/book@1.0.0")]`, `guard::Admission` for a hook the module contributes
//! to. So does a field or a case of a record or a variant of the module that enters the WIT, and a
//! parameter names the handle of an imported interface's resource the same way, `texts::Builder`,
//! itself or in a record or a variant of the module that no derived function returns, as the
//! runtime moves such a handle anywhere into an argument but returns a resource only from the
//! interface that defines it.
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
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use syn::{Ident, Item, LitStr, Type, ext::IdentExt, spanned::Spanned};
use tembra_contracts::guest;

/// How the world holds a consumed interface, which decides what a derived interface names through
/// the interface's module.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Another Orb's interface, imported: its types, and its resources' handles as parameters,
    /// alone or in the records and variants of the module that no derived function returns.
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
    /// The newtypes of the module, by name.
    newtypes: BTreeMap<String, Newtype>,
    /// What each module of the Orb imports, by scope, which a path written there is read through.
    uses: Vec<crate::Uses>,
}

/// A name a type path reaches through a consumed interface's module, `greeting::Entry`: the
/// interface, the module's name, the name and the path itself, for the errors, and whether the
/// path is the value of an `Option`, `Option<greeting::Entry>`.
#[derive(Clone)]
pub(crate) struct Named<'m, 't> {
    pub interface: &'m Reexported,
    pub module: &'m str,
    pub ident: &'t Ident,
    pub ty: &'t Type,
    pub in_option: bool,
    /// Whether the path leads into the bindings' own module of the interface's import,
    /// `self::example::counters::counter::Tally`, rather than through the module the Orb module
    /// re-exports: the import's copy of the type, whatever that module holds.
    pub through_import: bool,
    /// Where the `use` declarations of the modules the path passes lead it, when they do, for the
    /// errors: "`example::counters::counter::Tally` of the Orb module" for `counter::Tally` after
    /// `use super::example::counters::counter;`.
    pub led: Option<String>,
}

/// Where a type path leads, read as far as `Modules::leads` follows it: what is left of the path,
/// whether a `use` declaration led it there and the walk stopped in the Orb module, and the module
/// of the Orb it stopped in, by scope, none once it left the Orb module.
struct Leads {
    segments: Vec<Ident>,
    used: bool,
    rooted: bool,
    scope: Option<usize>,
}

/// What a walk through a type reaches: a name through a consumed interface's module, or the last
/// name of another path, `Vec` and `Entry` in `Vec<Entry>`, which may name a type of the module,
/// with the module of the Orb the path names it in, by scope: the one the path leads into,
/// `super::sessions::Token`, or the one writing it.
enum Reached<'m, 't> {
    Consumed(Named<'m, 't>),
    Module(&'t Ident, usize),
}

impl Modules {
    /// The consumed interfaces of the module, each under the name of its re-exported module: an
    /// interface imported by key under its view's, and with `context`, the attribute's option, the
    /// kernel's context interface under `context`. `kernel` says whether a state or a resource
    /// makes the world import the kernel's interfaces, `package` is the Orb's package, `imports`
    /// are the attribute's literals, read as identities already; `paths` are the Orb's own modules
    /// and `uses` what each imports, by scope; `own` are the names the Orb module's items take
    /// where a module takes its name (`own_names`). Another interface under a name already taken is
    /// an error on it, naming both, and so is a second role of an interface, or one role twice,
    /// which the world cannot hold, an import of the kernel's context interface beside the option,
    /// and a plain import of an interface the module implements, which would make the world a
    /// hook's. An item of the Orb module under the name of a re-exported module, or of a module the
    /// bindings declare there, is an error on the item, naming both.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        context: bool,
        kernel: bool,
        package: &str,
        imports: &[LitStr],
        keyed: &[String],
        services: &[Service],
        contributions: &[Contribution],
        declared: &[Declared],
        paths: &[Vec<Ident>],
        uses: &[crate::Uses],
        own: &[(Ident, &str)],
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
        // The Orb module holds its items beside the modules it re-exports: one of its own names
        // taken by a re-exported module would be defined twice, `r#ledger` as `ledger`. The option
        // keeps no span, so the error falls on the item for every re-export.
        for (ident, kind) in own {
            if let Some(reexported) = modules.get(&ident.unraw().to_string()) {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "`{ident}` is the name of the Orb module's own {kind} and of the module it re-exports for {}; rename its own {kind}",
                        reexported.described()
                    ),
                ));
            }
        }
        // The bindings declare in the Orb module `exports`, the module of the interfaces the world
        // exports, and the module of each namespace of the interfaces it imports itself: those the
        // attribute imports, the consumed hooks, the views and the derived hooks, interfaces of the
        // Orb's package, and the kernel's for the `context` option, a state or a resource. An
        // interface the world imports because one of those `use`s it, which the macro does not
        // read here, may bring another namespace, which the compiler judges.
        let namespace = |identity: &str| {
            crate::rust_ident(crate::Identity::of(identity).namespace)
                .unraw()
                .to_string()
        };
        let mut namespaces: BTreeSet<String> = imports
            .iter()
            .map(|literal| namespace(&literal.value()))
            .chain(
                contributions
                    .iter()
                    .map(|contribution| namespace(&contribution.hook)),
            )
            .chain(
                declared
                    .iter()
                    .filter(|declaration| declaration.input.is_none())
                    .map(|declaration| namespace(&declaration.hook)),
            )
            .collect();
        if !keyed.is_empty()
            || declared
                .iter()
                .any(|declaration| declaration.input.is_some())
        {
            namespaces.insert(namespace(package));
        }
        if context || kernel {
            namespaces.insert(namespace(guest::CONTEXT));
        }
        let exporting = !services.is_empty() || !contributions.is_empty() || !declared.is_empty();
        for (ident, kind) in own {
            let name = ident.unraw().to_string();
            let holding = if name == "exports" && exporting {
                "the interfaces the world exports".to_owned()
            } else if namespaces.contains(&name) {
                format!(
                    "the namespace `{name}`, which holds the interfaces the world imports from it"
                )
            } else {
                continue;
            };
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{ident}` is the name of the Orb module's own {kind} and of the bindings' module of {holding}; rename its own {kind}"
                ),
            ));
        }
        Ok(Self {
            reexported: modules,
            paths: paths.to_vec(),
            newtypes: BTreeMap::new(),
            uses: uses.to_vec(),
        })
    }

    /// The same modules, with the newtypes of the module, each read by its name.
    pub(crate) fn with_newtypes(mut self, newtypes: &[Newtype]) -> Self {
        self.newtypes = newtypes
            .iter()
            .map(|newtype| (newtype.ident.to_string(), newtype.clone()))
            .collect();
        self
    }

    /// The newtype a type written in the module `scope` names, when it names one: a path ending in
    /// the newtype's name, without type parameters, that no consumed interface's module reaches.
    pub(crate) fn newtype(&self, ty: &Type, scope: usize) -> Option<&Newtype> {
        let Type::Path(path) = ty else {
            return None;
        };
        let last = path.path.segments.last()?;
        if path.qself.is_some()
            || !matches!(last.arguments, syn::PathArguments::None)
            || self.named(ty, scope).is_some()
        {
            return None;
        }
        self.newtypes.get(&last.ident.to_string())
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

    /// The WIT spelling of a type written in the module `scope`: a newtype as its field's type. A
    /// newtype whose field holds it, through other newtypes too, has none.
    pub(crate) fn spelled(&self, ty: &Type, scope: usize) -> syn::Result<String> {
        self.spelled_within(ty, scope, &[])
    }

    /// The WIT spelling of a type written in the module `scope`, inside the fields of the newtypes
    /// `within`, which the type may not name again.
    fn spelled_within(&self, ty: &Type, scope: usize, within: &[&Ident]) -> syn::Result<String> {
        wit::wit_type_with(ty, &|named| {
            let Some(newtype) = self.newtype(named, scope) else {
                return Ok(None);
            };
            if within.contains(&&newtype.ident) {
                return Err(syn::Error::new(
                    named.span(),
                    format!(
                        "`{}` holds itself through its field, which WIT cannot spell: a type that enters the WIT holds no type that holds it",
                        newtype.ident
                    ),
                ));
            }
            let mut within = within.to_vec();
            within.push(&newtype.ident);
            self.spelled_within(&newtype.inner, newtype.scope, &within)
                .map(Some)
        })
    }

    /// The name a type reaches through a consumed interface's module, when it is such a path:
    /// `greeting::Entry`, or any path ending so, written in the module `scope`. A path that leads
    /// into a module of the Orb names a type of the module, whatever that module is called: the
    /// author's `greeting::Banner` next to a `mod greeting` of their own resolves there first. The
    /// kernel's context module reaches none.
    pub(crate) fn named<'m, 't>(&'m self, ty: &'t Type, scope: usize) -> Option<Named<'m, 't>> {
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
        if let Some(own) = crate::module_of(&path.path, &self.paths[scope])
            && self.paths.contains(&own)
        {
            return None;
        }
        let (module, interface) = self.reexported.get_key_value(&module.ident.to_string())?;
        if interface.role == Role::Context {
            return None;
        }
        // The bindings put an interface's import in `namespace::package::interface` and its export
        // under `exports`: a path that leads through the namespace and the package, not under
        // `exports`, is the import module's, `self::example::counters::counter::Tally`, or
        // `counter::Tally` after `use super::example::counters::counter;`.
        let identity = crate::Identity::of(&interface.identity);
        let leads = self.leads(&path.path, scope);
        let through_import = matches!(
            leads.segments.as_slice(),
            [before @ .., namespace, package, _, _]
                if *namespace == crate::rust_ident(identity.namespace)
                    && *package == crate::rust_ident(identity.package)
                    && before.last().is_none_or(|exports| exports != "exports")
        );
        let led = leads.used.then(|| {
            let reached = leads
                .segments
                .iter()
                .map(Ident::to_string)
                .collect::<Vec<_>>()
                .join("::");
            if leads.rooted {
                format!("`{reached}` of the Orb module")
            } else {
                format!("`{reached}`")
            }
        });
        Some(Named {
            interface,
            module,
            ident: &item.ident,
            ty,
            in_option: false,
            through_import,
            led,
        })
    }

    /// Where a type path written in the module `scope` leads, read as far as the macro follows
    /// it: through `self::`, `super::` and the sub-modules of the Orb, the names the `use`
    /// declarations of each module it passes bind, and the glob imports from the Orb's modules
    /// that bind them. It stops at the type's own name, at a crate, `crate::` or a module above
    /// the Orb module, and at a name nothing it reads binds, such as the modules the bindings
    /// declare in the Orb module, where what is left is the path from the module it stopped in.
    fn leads(&self, path: &syn::Path, scope: usize) -> Leads {
        let mut leads = Leads {
            segments: path
                .segments
                .iter()
                .map(|segment| segment.ident.clone())
                .collect(),
            used: false,
            rooted: false,
            scope: None,
        };
        if path.leading_colon.is_some() {
            return leads;
        }
        let mut scope = Some(scope);
        // Each step consumes a name or replaces one by what a declaration binds it to;
        // declarations binding one another in a circle would never end.
        for _ in 0..64 {
            let (Some(at), [first, rest @ ..]) = (scope, leads.segments.as_slice()) else {
                break;
            };
            if rest.is_empty() {
                break;
            }
            let (first, rest) = (first.clone(), rest.to_vec());
            if first == "self" {
                leads.segments = rest;
                continue;
            }
            if first == "super" {
                scope = crate::parent(at, &self.paths);
                if scope.is_some() {
                    leads.segments = rest;
                }
                continue;
            }
            if first == "crate" || first == "Self" {
                scope = None;
                continue;
            }
            if let Some(bound) = self.uses[at].names.get(&first.to_string()) {
                // `use name;` brings the crate `name` into scope.
                if bound.global || (bound.segments.len() == 1 && bound.segments[0] == first) {
                    scope = None;
                    continue;
                }
                leads.segments = bound.segments.iter().cloned().chain(rest).collect();
                leads.used = true;
                continue;
            }
            let mut child = self.paths[at].clone();
            child.push(first.clone());
            if let Some(index) = self.paths.iter().position(|module| *module == child) {
                scope = Some(index);
                leads.segments = rest;
                continue;
            }
            match crate::through_globs(&first, at, &self.uses, &self.paths) {
                crate::Globbed::In(module) => {
                    scope = Some(module);
                    leads.used = true;
                }
                crate::Globbed::Outside | crate::Globbed::Nowhere => break,
            }
        }
        leads.rooted = scope == Some(0);
        leads.scope = scope;
        leads
    }

    /// Walk a type written in the module `scope`: itself, then the type parameters of a path and
    /// the elements of a tuple. Each name a consumed interface's module reaches is reported, with
    /// whether it is the value of an `Option`, which `in_option` says of the type itself, and not
    /// looked into; so is the last name of every other path, before its type parameters, with the
    /// module of the Orb the path leads into down to the name (`leads`), through `self::`,
    /// `super::`, the sub-modules and the modules the `use` declarations bind, or the writing
    /// module for a bare name or a path the macro cannot follow so. A type with a WIT spelling of
    /// its own is not a name of the module: free JSON is not looked into, and a newtype, which the
    /// WIT spells as its field, is walked as its field, in the newtype's module, once.
    fn walk<'m, 't>(
        &'m self,
        ty: &'t Type,
        scope: usize,
        in_option: bool,
        visit: &mut impl FnMut(Reached<'m, 't>),
    ) where
        'm: 't,
    {
        self.walk_within(ty, scope, in_option, visit, &mut Vec::new());
    }

    /// `walk`, inside the fields of the newtypes `within`, which are not read again.
    fn walk_within<'m, 't>(
        &'m self,
        ty: &'t Type,
        scope: usize,
        in_option: bool,
        visit: &mut impl FnMut(Reached<'m, 't>),
        within: &mut Vec<&'m Ident>,
    ) where
        'm: 't,
    {
        if let Some(named) = self.named(ty, scope) {
            visit(Reached::Consumed(Named { in_option, ..named }));
            return;
        }
        if wit::free_json(ty) {
            return;
        }
        if let Some(newtype) = self.newtype(ty, scope) {
            if !within.contains(&&newtype.ident) {
                within.push(&newtype.ident);
                self.walk_within(&newtype.inner, newtype.scope, in_option, visit, within);
                within.pop();
            }
            return;
        }
        match ty {
            Type::Path(path) => {
                let Some(segment) = path.path.segments.last() else {
                    return;
                };
                let leads = self.leads(&path.path, scope);
                let module = match (leads.scope, leads.segments.as_slice()) {
                    (Some(module), [_]) => module,
                    _ => scope,
                };
                visit(Reached::Module(&segment.ident, module));
                if let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments {
                    for argument in &arguments.args {
                        if let syn::GenericArgument::Type(ty) = argument {
                            self.walk_within(ty, scope, segment.ident == "Option", visit, within);
                        }
                    }
                }
            }
            Type::Tuple(tuple) => {
                for element in &tuple.elems {
                    self.walk_within(element, scope, false, visit, within);
                }
            }
            _ => {}
        }
    }

    /// Each name a type written in the module `scope` reaches through a consumed interface's
    /// module: itself, or inside the type parameters of a path and the elements of a tuple, each
    /// with whether it is the value of an `Option`.
    pub(crate) fn named_in<'m, 't>(
        &'m self,
        ty: &'t Type,
        scope: usize,
        out: &mut Vec<Named<'m, 't>>,
    ) where
        'm: 't,
    {
        self.walk(ty, scope, false, &mut |reached| {
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
        self.walk(ty, scope, false, &mut |reached| {
            if let Reached::Module(ident, module) = reached
                && !wit::BUILTIN.contains(&ident.to_string().as_str())
            {
                out.push((ident.clone(), module));
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

/// The names the items of the Orb module take in the type namespace, where a module the Orb module
/// re-exports takes its own, each with what the item is, for the errors: its sub-modules, types and
/// traits, its `extern crate`s, and what a `use` imports with `self` (`self_imports`). A plain
/// `use` may import a function, which takes its name in the value namespace and compiles beside
/// such a module, and an item under `#[cfg(..)]` is gone where its configuration is off: the
/// compiler judges those.
pub(crate) fn own_names(items: &[Item]) -> Vec<(Ident, &'static str)> {
    let mut names = Vec::new();
    for item in items {
        let (attrs, bound) = match item {
            Item::Mod(item) => (&item.attrs, vec![(item.ident.clone(), "module")]),
            Item::Struct(item) => (&item.attrs, vec![(item.ident.clone(), "struct")]),
            Item::Enum(item) => (&item.attrs, vec![(item.ident.clone(), "enum")]),
            Item::Union(item) => (&item.attrs, vec![(item.ident.clone(), "union")]),
            Item::Type(item) => (&item.attrs, vec![(item.ident.clone(), "type alias")]),
            Item::Trait(item) => (&item.attrs, vec![(item.ident.clone(), "trait")]),
            Item::TraitAlias(item) => (&item.attrs, vec![(item.ident.clone(), "trait alias")]),
            Item::ExternCrate(item) => {
                let ident = item
                    .rename
                    .as_ref()
                    .map_or(&item.ident, |(_, rename)| rename);
                (&item.attrs, vec![(ident.clone(), "`extern crate`")])
            }
            Item::Use(item) => {
                let mut imported = Vec::new();
                self_imports(&item.tree, None, &mut imported);
                let bound = imported.into_iter().map(|ident| (ident, "import"));
                (&item.attrs, bound.collect())
            }
            _ => continue,
        };
        if !attrs
            .iter()
            .any(|attribute| crate::conditional(&attribute.meta))
        {
            names.extend(bound.into_iter().filter(|(ident, _)| ident != "_"));
        }
    }
    names
}

/// Each name a `use` tree binds with `self`, `crate::util::ledger::{self}` or `{self as ledger}`,
/// which imports from the type namespace alone, whatever the path leads to. `parent` is the name
/// before the tree in the path.
fn self_imports(tree: &syn::UseTree, parent: Option<&Ident>, out: &mut Vec<Ident>) {
    match tree {
        syn::UseTree::Path(path) => self_imports(&path.tree, Some(&path.ident), out),
        syn::UseTree::Name(name) if name.ident == "self" => out.extend(parent.cloned()),
        syn::UseTree::Rename(rename) if rename.ident == "self" => out.push(rename.rename.clone()),
        syn::UseTree::Group(group) => {
            for tree in &group.items {
                self_imports(tree, parent, out);
            }
        }
        _ => {}
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
pub(crate) fn shape_types(shape: &Shape) -> Vec<&Type> {
    match &shape.kind {
        ShapeKind::Record(fields) => fields.iter().map(|(_, ty)| ty).collect(),
        ShapeKind::Variant(cases) => cases
            .iter()
            .flat_map(|(_, payload)| payload.types())
            .collect(),
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
    /// `interface` defines, and the derived function whose result reaches `shape`, if one does.
    Field {
        shape: &'a Ident,
        interface: &'a Ident,
        returned: Option<Returning<'a>>,
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

/// A derived function whose result reaches a type of the module: its name, its result type, where
/// the error points, and the type of the module the result names, which holds the reached type.
#[derive(Clone, Copy)]
struct Returning<'a> {
    function: &'a str,
    result: &'a Type,
    via: &'a Ident,
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
/// the expansion parses, the views of the interfaces imported by key, the module's services,
/// resources, contributions and declared hooks, the types that enter the WIT with the interface
/// defining each, and the states with the module types that stay out of the WIT, which a state's
/// fields may reach.
pub(crate) struct Reach<'a> {
    pub modules: &'a Modules,
    pub parsed: &'a wit::Parsed,
    pub views: &'a [keyed::View],
    pub services: &'a [Service],
    pub resources: &'a [WitResource],
    pub contributions: &'a [Contribution],
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
/// and a parameter the handles of an imported interface's resources, itself or in a type of the
/// module that no derived function returns; a derived hook's input names none; a state names the
/// data types that `serde` can write. Nothing is read from the consumed packages when nothing
/// names them.
pub(crate) fn foreign(reach: Reach<'_>) -> syn::Result<BTreeMap<String, Vec<Foreign>>> {
    let Reach {
        modules,
        parsed,
        views,
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
    let returned = returned(&functions, shapes, modules);
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
                returned: returned
                    .get(&(shape.ident.to_string(), shape.scope))
                    .copied(),
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
        for (ty, via, scope) in state_types(state, shapes, unreached, modules) {
            let place = Place::State {
                state: &state.ident,
                via: via.cloned(),
            };
            found_in(modules, ty, scope, place, &mut found);
        }
    }
    if found.is_empty() {
        return Ok(BTreeMap::new());
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
    for (named, place) in &found {
        let items = &defined[&named.interface.identity];
        // A view's module holds its resource, named after the interface, and re-exports the types
        // the view uses, whatever else the interface defines.
        if named.interface.role == Role::Keyed {
            if *named.ident == view_object(&named.interface.identity) {
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
            let used = &views
                .iter()
                .find(|view| view.target == named.interface.identity)
                .expect("a view of each interface imported by key")
                .used;
            if !used
                .iter()
                .any(|name| crate::type_ident(name) == *named.ident)
            {
                let listed = if used.is_empty() {
                    "and they name none".to_owned()
                } else {
                    used.iter()
                        .map(|name| format!("`{}`", crate::type_ident(name)))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return Err(syn::Error::new(
                    named.ty.span(),
                    format!(
                        "{} `{}`, which names no type of the view `{}`: the view of {}, uses only the types the interface's functions name, {listed}",
                        place.subject(),
                        named.ident,
                        named.module,
                        named.interface.described()
                    ),
                ));
            }
        }
        let Some(definition) = items.get(named.ident) else {
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
        let Some(interface) = checked(named, definition, place, declared, dependency, modules)?
        else {
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

/// The types of the module the results of the derived functions reach, by name and by the module
/// declaring each, with the first function returning it: the types a result names, itself or
/// inside its containers, and those their fields and cases reach in turn, as the runtime looks
/// into a result. A name resolves by `own_or_only`: to the type of the module naming it, else to
/// the only type of that name in the Orb.
fn returned<'f>(
    functions: &'f [resource_params::Derived<'f>],
    shapes: &'f [Shape],
    modules: &Modules,
) -> BTreeMap<(String, usize), Returning<'f>> {
    let mut returned: BTreeMap<(String, usize), Returning<'f>> = BTreeMap::new();
    for function in functions {
        let Some(result) = function.result else {
            continue;
        };
        let mut named = Vec::new();
        modules.references(result, function.scope, &mut named);
        let mut pending: Vec<(Ident, usize, Option<&'f Ident>)> = named
            .into_iter()
            .map(|ident| (ident, function.scope, None))
            .collect();
        while let Some((ident, scope, via)) = pending.pop() {
            let named = shapes
                .iter()
                .filter(|shape| shape.ident == ident)
                .map(|shape| (shape.scope, shape));
            let Some(shape) = crate::own_or_only(named, scope) else {
                continue;
            };
            let key = (ident.to_string(), shape.scope);
            if returned.contains_key(&key) {
                continue;
            }
            let via = via.unwrap_or(&shape.ident);
            returned.insert(
                key,
                Returning {
                    function: &function.name,
                    result,
                    via,
                },
            );
            pending.extend(
                shape
                    .references
                    .iter()
                    .map(|(reference, scope)| (reference.clone(), *scope, Some(via))),
            );
        }
    }
    returned
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
    // A path into the bindings' module of the import of an interface the world exports names the
    // import's copy of its types, as the view of its keyed import does, while a derived interface,
    // exported, takes the copy of the export, which the module the Orb module re-exports for an
    // implemented interface and a contributed hook names; a declared hook's module is the import's
    // too, which its rule refuses below. The two copies are one type where the export holds none
    // of its own, for a type the interface `use`s from an interface the world only imports.
    if named.through_import
        && definition.copied
        && let Some(takes) = match interface.role {
            Role::Implemented => Some("an implemented interface"),
            Role::Contributed => Some("a hook"),
            _ => None,
        }
    {
        let module = named.module;
        let written = named.ty.to_token_stream().to_string().replace(' ', "");
        let led = named
            .led
            .as_ref()
            .map(|led| format!(", a `use` leading it to {led}"))
            .unwrap_or_default();
        return refuse(format!(
            "{what} of {of}, through the bindings' module of its import: a derived interface takes the types of {takes} from its export, as `{module}::{ident}` names them; `{written}` is the import's{led}"
        ));
    }
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
    // or the contributed hook's module names; a declared hook's module names the import's too. The
    // two copies are one type where the export holds none of its own.
    if interface.role == Role::Keyed
        && definition.copied
        && let Some((module, role)) = modules.exporting(&interface.identity)
    {
        let view = named.module;
        return match role {
            Role::Implemented => refuse(format!(
                "{what} of {of} and implements: a derived interface takes the types of an implemented interface from its export, as `{module}::{ident}` names them; `{view}::{ident}` is the import's"
            )),
            Role::Contributed => refuse(format!(
                "{what} of {of} and contributes to: a derived interface takes the types of a hook from its export, as `{module}::{ident}` names them; `{view}::{ident}` is the import's"
            )),
            _ => refuse(format!(
                "{what} of {of} and declares: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `{view}::{ident}`, `{module}::{ident}` and {} are the import's; use a type of the module",
                run()
            )),
        };
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
        // The runtime moves an owned handle of an imported interface anywhere into an argument,
        // in a record or a variant of the module too, and returns a resource only from the
        // interface that defines it, wherever the result holds it.
        (Role::Imported, wit::Kind::Resource) => match place {
            Place::Param { .. } | Place::Field { returned: None, .. } => Ok(Some(interface_of)),
            Place::Result { .. } => refuse(format!(
                "{what} of {of}: the runtime returns a resource only from the interface that defines it, so a derived function takes such a handle but returns none"
            )),
            Place::Field {
                returned: Some(returning),
                ..
            } => {
                let Returning {
                    function,
                    result,
                    via,
                } = returning;
                Err(syn::Error::new(
                    result.span(),
                    format!(
                        "`{function}` returns `{via}`, which holds `{ident}`, {what} of {of}: the runtime returns a resource only from the interface that defines it, so a derived function takes such a handle, alone or in a record or a variant of the module, but returns none"
                    ),
                ))
            }
            Place::Input { .. } | Place::State { .. } => unreachable!("checked above"),
        },
        (Role::Implemented | Role::Contributed | Role::Declared, wit::Kind::Resource)
            if returned =>
        {
            refuse(format!(
                "{what} of {of}: the runtime returns a resource only from the interface that defines it"
            ))
        }
        (Role::Implemented | Role::Contributed | Role::Declared, wit::Kind::Resource) => {
            refuse(format!(
                "{what} of {of}: a derived function takes the resources of the interfaces this module derives and the handles of the interfaces it imports"
            ))
        }
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
            "{what} of {of} that holds {problem}, which the runtime refuses in a signature"
        )),
        (_, wit::Kind::Type { option: true, .. }) if named.in_option => refuse(format!(
            "{what} of {of}, in an `Option`, and `{ident}` is an option itself: an option of an option, which the runtime refuses in a signature; use an enum of the module with a case for each"
        )),
        (_, wit::Kind::Type { .. }) => Ok(Some(interface_of)),
    }
}

/// The types of a state's fields and of the module types they reach, each once, with the module
/// type that holds it, `None` for the state's own fields, and the module the holder is declared
/// in. A name resolves as `serialized` resolves it, by `own_or_only`: to the type of the module
/// naming it, else to the one type of that name in the Orb.
fn state_types<'a>(
    state: &State,
    shapes: &'a [Shape],
    unreached: &'a [Candidate],
    modules: &Modules,
) -> Vec<(&'a Type, Option<&'a Ident>, usize)> {
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
        let named = declared
            .iter()
            .filter(|(ident, _, _)| *ident == name)
            .map(|entry| (entry.1, entry));
        crate::own_or_only(named, scope)
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
            types.push((*ty, via, *scope));
            let mut references = Vec::new();
            modules.references(ty, *scope, &mut references);
            pending.extend(
                references
                    .into_iter()
                    .map(|reference| (reference, *scope, false)),
            );
        }
    }
    types
}
