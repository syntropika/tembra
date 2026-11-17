//! The WIT text derived from the module items, and the consumed packages it refers to.
use crate::foreign::{Foreign, Modules};
use crate::resource_params::{self, Derived};
use crate::{Composition, Declared, Function, Handles, Service, Shape, ShapeKind, WitResource};
use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro2::Span;
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, Fields, Ident, ItemEnum, ItemStruct, Lit, LitStr, Meta, Type, ext::IdentExt,
    spanned::Spanned,
};
use wit_parser::{
    PackageId, PackageName, PackageSourceMap, Resolve, TypeDefKind, UnresolvedPackageGroup,
};

/// WIT names are kebab-case; Rust identifiers map to them one by one, a raw identifier without
/// its `r#`.
pub(crate) fn kebab(ident: &Ident) -> String {
    ident.unraw().to_string().to_kebab_case()
}

/// A WIT name as the WIT text writes it: with WIT's `%` escape when it is a keyword of WIT, so
/// `list` or `record` names an item instead of starting a type.
pub(crate) fn escaped(name: &str) -> String {
    if crate::WIT_KEYWORDS.contains(&name) {
        format!("%{name}")
    } else {
        name.to_owned()
    }
}

/// What makes the kebab-case form of a Rust name no WIT identifier, if anything: WIT names are
/// words of ASCII letters and digits joined by `-`, the first starting with a letter.
pub(crate) fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("is empty");
    }
    if !name.starts_with(|first: char| first.is_ascii_alphabetic()) {
        return Some("does not start with a letter");
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    }) {
        return Some("holds a character other than ASCII letters, digits and `-`");
    }
    None
}

/// The names of one scope of the WIT, an interface, a resource or a parameter list, with the item
/// that claimed each: WIT refuses one name twice in a scope.
struct Namespace {
    /// The scope, for the errors: "the interface `texts`".
    scope: String,
    /// Why two items meet there, for the errors.
    rule: &'static str,
    claimed: BTreeMap<String, String>,
}

impl Namespace {
    fn new(scope: String, rule: &'static str) -> Self {
        Self {
            scope,
            rule,
            claimed: BTreeMap::new(),
        }
    }

    /// Claim the WIT name of `ident` for `item`, described for the errors.
    fn claim(&mut self, ident: &Ident, item: String) -> syn::Result<()> {
        let name = kebab(ident);
        if let Some(problem) = name_problem(&name) {
            return Err(syn::Error::new(
                ident.span(),
                format!("{item} has no WIT name: its kebab-case form `{name}` {problem}"),
            ));
        }
        if let Some(previous) = self.claimed.insert(name.clone(), item.clone()) {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "{previous} and {item} are both `{name}` in {}{}; rename one of them",
                    self.scope, self.rule
                ),
            ));
        }
        Ok(())
    }
}

/// Claim the names of a resource's methods and static functions, in the resource's own
/// namespace, and of the parameters of each of its functions.
fn claim_resource(resource: &WitResource) -> syn::Result<()> {
    let mut namespace = Namespace::new(
        format!("the resource `{}`", kebab(&resource.ident)),
        ", where its methods and static functions share one namespace",
    );
    for function in resource.methods.iter().chain(&resource.statics) {
        let item = format!("`{}::{}`", resource.ident, function.ident);
        namespace.claim(&function.ident, item.clone())?;
        claim_params(&item, &function.params)?;
    }
    if let Some(constructor) = &resource.constructor {
        let item = format!("`{}::{}`", resource.ident, constructor.ident);
        claim_params(&item, &constructor.params)?;
    }
    Ok(())
}

/// Claim the parameters of one function in a namespace of their own.
fn claim_params(function: &str, params: &[(Ident, Type, String)]) -> syn::Result<()> {
    let mut namespace = Namespace::new(format!("the parameters of {function}"), "");
    for (ident, _, _) in params {
        namespace.claim(ident, format!("the parameter `{ident}`"))?;
    }
    Ok(())
}

/// The items of the module that enter the WIT: its exported types, where each is defined, its
/// services, its resources and its declared hooks, of which the derived ones are interfaces of
/// the package, and what each derived interface `use`s from the consumed interfaces, by its name,
/// whose modules name no type of the module.
pub(crate) struct Items<'a> {
    pub shapes: &'a [Shape],
    pub placement: &'a BTreeMap<String, Ident>,
    pub services: &'a [Service],
    pub resources: &'a [WitResource],
    pub declared: &'a [Declared],
    pub foreign: &'a BTreeMap<String, Vec<Foreign>>,
    pub modules: &'a Modules,
}

/// The names of the shapes a derived hook reaches through its input, directly or through the
/// fields of another shape: they are defined in the first derived hook's interface.
pub(crate) fn hook_shapes(
    shapes: &[Shape],
    declared: &[Declared],
    modules: &Modules,
) -> BTreeSet<String> {
    let by_name: BTreeMap<String, &Shape> = shapes
        .iter()
        .map(|shape| (shape.ident.to_string(), shape))
        .collect();
    let mut pending: Vec<Ident> = Vec::new();
    for declaration in declared {
        if let Some((input, _)) = &declaration.input {
            modules.references(input, declaration.scope, &mut pending);
        }
    }
    let mut reached = BTreeSet::new();
    while let Some(ident) = pending.pop() {
        let name = ident.to_string();
        if let Some(shape) = by_name.get(&name)
            && reached.insert(name)
        {
            pending.extend(shape.references.iter().cloned());
        }
    }
    reached
}

/// The module types the interface of the derived service `service` mentions in its own text: those
/// of its `functions` among the derived ones, its own functions and the constructors, methods and
/// static functions of the resources it owns.
fn service_mentions(service: &Ident, functions: &[Derived<'_>], modules: &Modules) -> Vec<Ident> {
    let mut mentioned = Vec::new();
    for function in functions
        .iter()
        .filter(|function| function.interface == service)
    {
        for (_, ty, _) in function.params {
            modules.references(ty, function.scope, &mut mentioned);
        }
        if let Some(ty) = function.result {
            modules.references(ty, function.scope, &mut mentioned);
        }
    }
    mentioned
}

/// Where each shape that enters the WIT is defined, by name: the Rust name of the hook or
/// service whose interface defines it. A shape a derived hook reaches is defined in the first
/// derived hook's interface, which the hook's import brings into the world without any other.
/// Any other shape is defined in the interface of the earliest derived service, in declaration
/// order, that mentions it: in its functions, in its own resources' functions, or through the
/// fields of other shapes. So a later service that names a shape no earlier one does defines it
/// itself. Every shape that enters the WIT is reached from a derived hook, a derived service or
/// one of its resources, so each has a place.
pub(crate) fn placement(
    shapes: &[Shape],
    services: &[Service],
    resources: &[WitResource],
    declared: &[Declared],
    modules: &Modules,
) -> BTreeMap<String, Ident> {
    let by_name: BTreeMap<String, &Shape> = shapes
        .iter()
        .map(|shape| (shape.ident.to_string(), shape))
        .collect();
    let mut placed: BTreeMap<String, Ident> = BTreeMap::new();
    let first_hook = declared
        .iter()
        .find(|declaration| declaration.input.is_some())
        .map(|declaration| &declaration.ident);
    for name in hook_shapes(shapes, declared, modules) {
        let hook = first_hook.expect("a shape a derived hook reaches");
        placed.insert(name, hook.clone());
    }
    let derived: Vec<&Service> = services
        .iter()
        .filter(|service| service.implements.is_none())
        .collect();
    let functions = resource_params::derived(services, resources);
    for service in &derived {
        let mut pending = service_mentions(&service.ident, &functions, modules);
        let mut seen = BTreeSet::new();
        while let Some(ident) = pending.pop() {
            let name = ident.to_string();
            let Some(shape) = by_name.get(&name) else {
                continue;
            };
            if seen.insert(name.clone()) {
                placed.entry(name).or_insert_with(|| service.ident.clone());
                pending.extend(shape.references.iter().cloned());
            }
        }
    }
    placed
}

/// Which kernel interfaces the world uses.
pub(crate) struct Kernel {
    pub state: bool,
    pub resources: bool,
    pub context: bool,
}

/// One interface of the package: its name, its docs, the shapes it defines, what it `use`s from
/// which other interface, and the struct it is derived from.
struct Interface<'a> {
    name: String,
    ident: &'a Ident,
    docs: &'a [String],
    defined: Vec<&'a Shape>,
    uses: Vec<(String, Vec<String>)>,
}

/// Write the package: one interface per derived hook, with its callback `apply`, the first of them
/// defining the types the hooks reach; one interface per derived service, with the resources it
/// owns and the types `placement` gives it; each interface `use`s what it mentions and another
/// interface defines; then the view of each interface imported by key. Then a world using the
/// kernel's interfaces for its states, resources and context, importing every consumed interface
/// and every view, importing and exporting every hook, consumed or derived, and exporting every
/// service interface, derived or implemented. Returns the text and, by interface name, the WIT
/// names of the types each interface `use`s from the others, which its bindings' module declares
/// as aliases beside the types it defines.
pub(crate) fn render<'a>(
    package: &str,
    world: &str,
    items: Items<'_>,
    hooks: impl Iterator<Item = &'a String>,
    imports: &[String],
    views: &[crate::keyed::View],
    kernel: Kernel,
) -> syn::Result<(String, BTreeMap<String, BTreeSet<String>>)> {
    let Items {
        shapes,
        placement,
        services,
        resources,
        declared,
        foreign,
        modules,
    } = items;
    let derived_hooks: Vec<&Declared> = declared
        .iter()
        .filter(|declaration| declaration.input.is_some())
        .collect();
    let derived: Vec<&Service> = services
        .iter()
        .filter(|service| service.implements.is_none())
        .collect();
    let functions = resource_params::derived(services, resources);
    let owner_of =
        |resource: &WitResource| kebab(resource.service.as_ref().expect("resolved owner"));
    // The interfaces in the order the package lists them: the derived hooks, then the services.
    let order: Vec<(String, &Ident, &[String])> = derived_hooks
        .iter()
        .map(|hook| (kebab(&hook.ident), &hook.ident, hook.docs.as_slice()))
        .chain(derived.iter().map(|service| {
            (
                kebab(&service.ident),
                &service.ident,
                service.docs.as_slice(),
            )
        }))
        .collect();
    let mut interfaces = Vec::new();
    for (name, ident, docs) in &order {
        let defined: Vec<&Shape> = shapes
            .iter()
            .filter(|shape| placement.get(&shape.ident.to_string()) == Some(*ident))
            .collect();
        // What the interface's text mentions: the hook's input, or the service's functions and
        // its resources' functions, and the fields of the shapes it defines.
        let mut mentioned = match derived_hooks.iter().find(|hook| &hook.ident == *ident) {
            Some(hook) => {
                let mut mentioned = Vec::new();
                let (input, _) = hook.input.as_ref().expect("a derived hook");
                modules.references(input, hook.scope, &mut mentioned);
                mentioned
            }
            None => service_mentions(ident, &functions, modules),
        };
        for shape in &defined {
            mentioned.extend(shape.references.iter().cloned());
        }
        // Each type another interface defines is borrowed from it with `use`, one line per
        // interface in package order. A resource is mentioned only in the interface that owns it,
        // as `resource_params` checks, so none is borrowed.
        let mut uses: Vec<(String, Vec<String>)> = order
            .iter()
            .filter(|(other, _, _)| other != name)
            .map(|(other, _, _)| (other.clone(), Vec::new()))
            .collect();
        // Types and functions share the interface's names: what it uses, defines, owns and
        // declares, in the order the text lists them.
        let mut namespace = Namespace::new(
            format!("the interface `{name}`"),
            ", where types and functions share one namespace",
        );
        let mut borrowed: Vec<(String, &Ident)> = Vec::new();
        for shape in shapes
            .iter()
            .filter(|shape| mentioned.contains(&shape.ident))
        {
            if let Some(owner) = placement.get(&shape.ident.to_string())
                && owner != *ident
            {
                borrowed.push((kebab(owner), &shape.ident));
            }
        }
        for (from, item) in borrowed {
            let Some((_, names)) = uses.iter_mut().find(|(other, _)| *other == from) else {
                continue;
            };
            if !names.contains(&kebab(item)) {
                names.push(kebab(item));
                namespace.claim(item, format!("`{item}`, used from `{from}`,"))?;
            }
        }
        for used in foreign.get(name).into_iter().flatten() {
            for (_, ident) in &used.names {
                namespace.claim(ident, format!("`{ident}`, used from {},", used.identity))?;
            }
        }
        uses.retain(|(_, names)| !names.is_empty());
        for shape in &defined {
            namespace.claim(&shape.ident, format!("the type `{}`", shape.ident))?;
        }
        match derived_hooks.iter().find(|hook| &hook.ident == *ident) {
            Some(hook) => {
                let apply = Ident::new("apply", hook.ident.span());
                namespace.claim(&apply, format!("the callback `apply` of `{}`", hook.ident))?;
            }
            None => {
                let service = derived
                    .iter()
                    .find(|service| &service.ident == *ident)
                    .expect("a derived service");
                for resource in resources
                    .iter()
                    .filter(|resource| owner_of(resource) == *name)
                {
                    namespace.claim(
                        &resource.ident,
                        format!("the resource `{}`", resource.ident),
                    )?;
                    claim_resource(resource)?;
                }
                for function in &service.functions {
                    let item = format!("`{}::{}`", service.ident, function.ident);
                    namespace.claim(&function.ident, item.clone())?;
                    claim_params(&item, &function.params)?;
                }
            }
        }
        interfaces.push(Interface {
            name: name.clone(),
            ident,
            docs,
            defined,
            uses,
        });
    }
    // `placement` defines each type no later than the first interface that mentions it, and no
    // resource is borrowed, so an interface `use`s only the ones before it: the `use`s form no
    // cycle, which WIT would refuse.
    debug_assert!(interfaces.iter().enumerate().all(|(at, interface)| {
        interface
            .uses
            .iter()
            .all(|(from, _)| interfaces[..at].iter().any(|earlier| earlier.name == *from))
    }));
    let mut wit = format!("package {package};\n");
    for interface in &interfaces {
        wit.push('\n');
        for doc in interface.docs {
            wit.push_str(&format!("///{doc}\n"));
        }
        wit.push_str(&format!("interface {} {{\n", escaped(&interface.name)));
        for (from, names) in &interface.uses {
            let names: Vec<String> = names.iter().map(|name| escaped(name)).collect();
            wit.push_str(&format!(
                "  use {}.{{{}}};\n",
                escaped(from),
                names.join(", ")
            ));
        }
        for used in foreign.get(&interface.name).into_iter().flatten() {
            wit.push_str(&used.text());
        }
        for shape in &interface.defined {
            wit.push_str(&shape.wit);
        }
        if let Some(hook) = derived_hooks
            .iter()
            .find(|hook| &hook.ident == interface.ident)
        {
            let (_, spelling) = hook.input.as_ref().expect("a derived hook");
            let result = match hook.composition {
                Composition::Series => String::new(),
                Composition::Waterfall => format!(" -> {spelling}"),
                Composition::Bail => format!(" -> option<{spelling}>"),
            };
            wit.push_str(&format!("  apply: func(input: {spelling}){result};\n"));
        } else if let Some(service) = derived
            .iter()
            .find(|service| &service.ident == interface.ident)
        {
            for resource in resources
                .iter()
                .filter(|resource| owner_of(resource) == interface.name)
            {
                wit.push_str(&resource_text(resource));
            }
            for function in &service.functions {
                wit.push_str(&function_text(function, "  ", ""));
            }
        }
        wit.push_str("}\n");
    }
    for view in views {
        wit.push('\n');
        wit.push_str(&view.text);
    }
    wit.push_str(&format!("\nworld {} {{\n", escaped(world)));
    if kernel.state {
        wit.push_str(&format!(
            "  import {GUEST_STATE};\n  export {GUEST_STATE_INIT};\n"
        ));
    }
    if kernel.resources {
        wit.push_str(&format!(
            "  import {GUEST_RESOURCES};\n  export {GUEST_RESOURCES_INIT};\n"
        ));
    }
    if kernel.context {
        wit.push_str(&format!("  import {GUEST_CONTEXT};\n"));
    }
    for import in imports {
        wit.push_str(&format!("  import {import};\n"));
    }
    for view in views {
        wit.push_str(&format!("  import {};\n", escaped(&view.name)));
    }
    for hook in hooks {
        wit.push_str(&format!("  import {hook};\n  export {hook};\n"));
    }
    for hook in &derived_hooks {
        let name = escaped(&kebab(&hook.ident));
        wit.push_str(&format!("  import {name};\n  export {name};\n"));
    }
    for service in services {
        match &service.implements {
            Some(identity) => wit.push_str(&format!("  export {identity};\n")),
            None => wit.push_str(&format!("  export {};\n", escaped(&kebab(&service.ident)))),
        }
    }
    wit.push_str("}\n");
    let used = interfaces
        .iter()
        .map(|interface| {
            let names = interface
                .uses
                .iter()
                .flat_map(|(_, names)| names.iter().cloned())
                .collect();
            (interface.name.clone(), names)
        })
        .collect();
    Ok((wit, used))
}

/// One function of an interface or a resource, with its `///` lines, at the given indentation.
fn function_text(function: &Function, indent: &str, keyword: &str) -> String {
    let mut text = String::new();
    for doc in &function.docs {
        text.push_str(&format!("{indent}///{doc}\n"));
    }
    let params = function
        .params
        .iter()
        .map(|(ident, _, wit)| format!("{}: {wit}", escaped(&kebab(ident))))
        .collect::<Vec<_>>()
        .join(", ");
    let result = function
        .result
        .as_ref()
        .map(|(_, wit)| format!(" -> {wit}"))
        .unwrap_or_default();
    let asynchronous = if function.asynchronous { "async " } else { "" };
    text.push_str(&format!(
        "{indent}{}: {keyword}{asynchronous}func({params}){result};\n",
        escaped(&kebab(&function.ident))
    ));
    text
}

/// A WIT `resource` block: the constructor, then the methods, then the static functions.
fn resource_text(resource: &WitResource) -> String {
    let mut text = String::new();
    for doc in &resource.docs {
        text.push_str(&format!("  ///{doc}\n"));
    }
    text.push_str(&format!(
        "  resource {} {{\n",
        escaped(&kebab(&resource.ident))
    ));
    if let Some(constructor) = &resource.constructor {
        let params = constructor
            .params
            .iter()
            .map(|(ident, _, wit)| format!("{}: {wit}", escaped(&kebab(ident))))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("    constructor({params});\n"));
    }
    for method in &resource.methods {
        text.push_str(&function_text(method, "    ", ""));
    }
    for function in &resource.statics {
        text.push_str(&function_text(function, "    ", "static "));
    }
    text.push_str("  }\n");
    text
}

/// The kernel's interfaces, as the world names them.
const GUEST_STATE: &str = "tembra:guest/state@1.0.0";
const GUEST_STATE_INIT: &str = "tembra:guest/state-init@1.0.0";
const GUEST_RESOURCES: &str = "tembra:guest/resources@1.0.0";
const GUEST_RESOURCES_INIT: &str = "tembra:guest/resources-init@1.0.0";
pub(crate) const GUEST_CONTEXT: &str = "tembra:guest/context@1.0.0";

/// The kernel's `tembra:guest` package, as `tembra-contracts` bundles it for every host.
const GUEST: &str = tembra_contracts::guest::SOURCE;

/// The kernel's `tembra:guest` package in nested form, appended to the inline WIT so the bindings
/// of a state, a resource or the context resolve it.
pub(crate) fn guest_package() -> syn::Result<String> {
    nested(GUEST)
        .ok_or_else(|| error("the bundled tembra:guest package has no `package ..;` declaration"))
}

/// Whether the inline WIT needs the kernel's bundled package nested in it: not when the consumed
/// directory holds a copy already, one of its bundled `copies` (`bundled_copies`).
pub(crate) fn guest_package_needed(parsed: &Parsed, copies: &Copies) -> syn::Result<bool> {
    Ok(!copies.contains_key(&parsed.guest()?.main.name))
}

/// Claim the Orb's package name, the attribute's `package`, in the WIT the bindings read, which
/// holds one package of a name: an error on the package when the bindings read a bundled package
/// of that name beside it, a copy the consumed directory holds, one of its bundled `copies`
/// (`bundled_copies`), or one the inline WIT nests, the kernel's, when `guest`, or a WASI package
/// of `wasi` (`wasi::packages`), each as `parsed` holds it parsed.
pub(crate) fn claim_package(
    package: &LitStr,
    parsed: &Parsed,
    copies: &Copies,
    guest: bool,
    wasi: &[PackageId],
) -> syn::Result<()> {
    let name = package.value();
    let refused = |reason: String| {
        syn::Error::new(
            package.span(),
            format!(
                "the Orb's package cannot be named {name}: {reason}; give the Orb a package name of its own"
            ),
        )
    };
    if let Some(copy) = copies.keys().find(|copy| copy.to_string() == name) {
        let what = if copy.namespace == "wasi" {
            "the WASI package Tembra serves"
        } else {
            "the kernel's package"
        };
        return Err(refused(format!(
            "the consumed directory holds a copy of {what} of that name, which the bindings read beside it"
        )));
    }
    let nested = |what: &str, why: &str| {
        refused(format!(
            "the bindings read {what} of that name beside it, for {why}"
        ))
    };
    if guest && parsed.guest()?.main.name.to_string() == name {
        return Err(nested(
            "the kernel's package",
            "a state, a resource or the context",
        ));
    }
    if !wasi.is_empty() {
        let (bundled, _) = parsed.bundled()?;
        if wasi
            .iter()
            .any(|id| bundled.packages[*id].name.to_string() == name)
        {
            return Err(nested("the WASI package Tembra serves", "the WASI imports"));
        }
    }
    Ok(())
}

/// The bundled packages the consumed directory holds copies of, each with the encoded files of
/// `deps/` it was decoded from, which hold only the parts of the package they use: none for a copy
/// read from its file, which is whole.
pub(crate) type Copies = BTreeMap<PackageName, Vec<PathBuf>>;

/// The bundled packages the consumed packages, read from the consumed directory, hold copies of,
/// as a contract `cargo tembra build` wrote holds them in its `deps/`: the inline WIT leaves those
/// out, since the bindings would read them twice. Every consumed package named as a bundled one, a
/// WASI package Tembra serves or the kernel's, is checked as `Contract::from_dir` checks it,
/// whether the Orb uses it or not. One decoded from an encoded file of `deps/` has no source to
/// compare: it is a copy, with the files it was decoded from, once `from_dir`'s merge of the
/// encoded files accepts it (`merged`), but for the kernel's package where the macro brings it
/// itself, for a state, a resource or the context, `kernel`: a copy of it decoded so is then an
/// error asking to remove it. Any other must be read from a file of `deps/` itself: a copy in a
/// directory of `deps/` or in the directory itself, even line for line, is `from_dir`'s error
/// naming that directory. And that file must be the bundled file line for line, whatever its line
/// endings (`tembra_contracts::bundled`): a copy nested in a file of another package, the one its
/// `package` line declares as wit-parser reads it, even line for line, is `from_dir`'s error
/// naming that file, and a file of the package that differs, if only in its comments, is an error
/// naming the file, the package and what it is.
pub(crate) fn bundled_copies(parsed: &Parsed, kernel: bool) -> syn::Result<Copies> {
    let Some(dir) = parsed.dir() else {
        return Ok(Copies::new());
    };
    let (consumed, sources) = (dir.packages()?, dir.sources()?);
    // The bundled packages by the names their sources declare: resolved together with the
    // consumed ones, each would be found as its own copy.
    let bundled: Vec<&str> = tembra_contracts::WASI_SOURCES
        .iter()
        .map(|(_, source)| *source)
        .chain([GUEST])
        .filter_map(|source| source.lines().find_map(declared))
        .collect();
    let deps = dir.path().join("deps");
    // Each consumed package named as a bundled one, with a file it was read from: none when it was
    // decoded from an encoded file.
    let named: Vec<(&PackageName, Option<&Path>)> = consumed
        .package_names
        .iter()
        .filter(|(name, _)| bundled.contains(&name.to_string().as_str()))
        .map(|(name, &package)| {
            let file = sources
                .package_paths(package)
                .and_then(|mut files| files.next());
            (name, file)
        })
        .collect();
    let decoded = if named.iter().any(|(_, file)| file.is_none()) {
        merged(parsed, &deps)?
    } else {
        BTreeMap::new()
    };
    let duplicate = |name: &PackageName, place: &Path| {
        error(format!(
            "WIT package {name} in {} is already provided by the standard contracts bundled with Tembra; remove the duplicate",
            place.display()
        ))
    };
    let mut copies = Copies::new();
    for (name, file) in named {
        let Some(file) = file else {
            let files = decoded.get(name).cloned().unwrap_or_default();
            if kernel && name.namespace != "wasi" {
                let files: Vec<String> = files
                    .iter()
                    .map(|file| file.display().to_string())
                    .collect();
                return Err(error(format!(
                    "{}: this copy of {name}, the kernel's package, is encoded; remove it, since the macro brings the kernel's package",
                    files.join(", ")
                )));
            }
            copies.insert(name.clone(), files);
            continue;
        };
        if let Some(place) = file.parent().filter(|place| *place != deps.as_path()) {
            return Err(duplicate(name, place));
        }
        let source = std::fs::read_to_string(file).ok();
        if source.as_deref().is_some_and(tembra_contracts::bundled) {
            copies.insert(name.clone(), Vec::new());
            continue;
        }
        // The file's own package, as wit-parser reads the line declaring it, whatever else that
        // line holds, a comment included.
        let own = source
            .as_deref()
            .and_then(|source| UnresolvedPackageGroup::parse(file, source).ok())
            .map(|parse| parse.main.name);
        if own.is_some_and(|own| own != *name) {
            return Err(duplicate(name, file));
        }
        let what = if name.namespace == "wasi" {
            "a WASI package Tembra serves"
        } else {
            "the kernel's package"
        };
        return Err(error(format!(
            "{}: this copy of {name}, {what}, differs from the one bundled with this macro; build the consumed contract with the `cargo tembra` of the same version",
            file.display()
        )));
    }
    Ok(copies)
}

/// Check the encoded WIT packages of `deps/` as `Contract::from_dir` checks them: each `.wat` file,
/// and each `.wit` or `.wasm` file that starts with the wasm magic `\0asm`, is merged, in the order
/// of the file names, into the bundled packages. The merge refuses a package that disagrees with
/// the bundled one of its name, and the error is `from_dir`'s, naming the file. Each package the
/// files hold comes with the files that hold it.
fn merged(parsed: &Parsed, deps: &Path) -> syn::Result<BTreeMap<PackageName, Vec<PathBuf>>> {
    let failed = |path: &Path, cause: &dyn std::fmt::Display| {
        error(format!(
            "failed to parse WIT package {}: {cause:#}",
            path.display()
        ))
    };
    let mut standard = parsed.bundled()?.0.clone();
    standard
        .push_group(parsed.guest()?.clone())
        .map_err(|cause| error(format!("the bundled tembra:guest package: {cause:#}")))?;
    let mut files = std::fs::read_dir(deps)
        .and_then(|entries| {
            entries
                .map(|entry| Ok(entry?.path()))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .map_err(|cause| failed(deps, &cause))?;
    files.sort();
    let mut decoded: BTreeMap<PackageName, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        let extension = file.extension().and_then(|extension| extension.to_str());
        if file.is_dir() || !matches!(extension, Some("wit" | "wasm" | "wat")) {
            continue;
        }
        let bytes = std::fs::read(&file).map_err(|cause| failed(&file, &cause))?;
        if extension == Some("wat") || bytes.starts_with(b"\0asm") {
            // `Resolve::push_file` decodes the file and merges what it holds; decoded apart first,
            // its packages are known.
            let mut packages = Resolve::default();
            packages
                .push_file(&file)
                .map_err(|cause| failed(&file, &cause))?;
            for name in packages.package_names.keys() {
                decoded.entry(name.clone()).or_default().push(file.clone());
            }
            standard
                .merge(packages)
                .map_err(|cause| failed(&file, &cause))?;
        }
    }
    Ok(decoded)
}

/// The package a line of a WIT package file declares: `ns:name@version` for the line
/// `package ns:name@version;`.
fn declared(line: &str) -> Option<&str> {
    Some(
        line.trim()
            .strip_prefix("package ")?
            .strip_suffix(';')?
            .trim(),
    )
}

/// A WIT package file in nested form: its declaration, a line `package ns:name@version;`, opens
/// a block holding the rest of the file. The file's lines may end with `\n` or `\r\n`, as a
/// checkout writes them; the nested form ends its lines with `\n`.
pub(crate) fn nested(file: &str) -> Option<String> {
    let mut lines = file.lines();
    let mut nested = String::new();
    loop {
        let line = lines.next()?;
        if let Some(name) = declared(line) {
            nested.push_str(&format!("package {name} {{\n"));
            break;
        }
        nested.push_str(line);
        nested.push('\n');
    }
    for line in lines.skip_while(|line| line.trim().is_empty()) {
        nested.push_str(line);
        nested.push('\n');
    }
    nested.push_str("}\n");
    Some(nested)
}

/// A directory of consumed WIT packages, resolved against the crate manifest.
pub(crate) fn absolute(dir: &str) -> syn::Result<PathBuf> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| error("CARGO_MANIFEST_DIR is not set"))?;
    Ok(PathBuf::from(manifest).join(dir))
}

/// The callback of one hook, read from the consumed packages: its name, which compositions its
/// shape admits, and how the bindings spell its input and result.
pub(crate) struct Callback {
    pub name: String,
    /// The callback returns nothing.
    pub series: bool,
    /// The callback returns its input type.
    pub waterfall: bool,
    /// The callback returns an option.
    pub bail: bool,
    pub input: Spelled,
    pub result: Option<Spelled>,
    /// How the import takes the input, as the bindings bind it: `run` lends it so.
    pub passed: Passed,
}

/// A WIT type as the generated bindings spell it in the hook's import module and in its export
/// module, which are distinct Rust types.
pub(crate) struct Spelled {
    pub import: String,
    pub export: String,
    /// Where a value of the type holds owned resource handles, if anywhere: an exporter wraps
    /// each of the author's values there into the bindings' handle type.
    pub handles: Option<Handles>,
}

/// An interface of a consumed package, as the bindings spell it: its freestanding functions and
/// its resources.
pub(crate) struct Consumed {
    pub functions: Vec<ConsumedFunction>,
    pub resources: Vec<ConsumedResource>,
}

/// One function of a consumed interface or resource, parameters and result spelled for the
/// bindings; a method's implicit `self` is left out.
pub(crate) struct ConsumedFunction {
    pub name: String,
    pub asynchronous: bool,
    pub params: Vec<(String, Spelled)>,
    pub result: Option<Spelled>,
}

/// One resource of a consumed interface.
pub(crate) struct ConsumedResource {
    pub name: String,
    pub constructor: Option<Vec<(String, Spelled)>>,
    pub methods: Vec<ConsumedFunction>,
    pub statics: Vec<ConsumedFunction>,
}

#[cfg(test)]
thread_local! {
    /// How many directories of WIT packages this thread has read, for the test that an expansion
    /// reads its consumed directory once.
    pub(crate) static DIRECTORY_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read the WIT packages of a directory, with the files each was read from
/// (`Directory::sources`), once `held_twice` finds none they hold twice.
fn resolve(dir: &Path) -> syn::Result<(Resolve, PackageSourceMap)> {
    #[cfg(test)]
    DIRECTORY_PARSES.set(DIRECTORY_PARSES.get() + 1);
    held_twice(dir)?;
    let mut resolve = Resolve::default();
    let (_, sources) = resolve.push_dir(dir).map_err(|cause| {
        error(format!(
            "cannot read the WIT packages in {}: {cause:#}",
            dir.display()
        ))
    })?;
    Ok((resolve, sources))
}

/// Refuse a package a directory of WIT packages holds both inside an encoded file of `deps/` and
/// as text, in a `.wit` file or a directory of `deps/` or in the directory itself:
/// `Resolve::push_dir`, with which the bindings read the directory too, merges the encoded files
/// first, then adds the text packages, asserting that each is new, and panics. Only when `deps/`
/// holds an encoded file, as `merged` tells one, is each encoded file decoded apart and each text
/// package parsed apart; one that does not read is left to `push_dir`, which reports it. The
/// error names the package and both places.
fn held_twice(dir: &Path) -> syn::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir.join("deps")) else {
        return Ok(());
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    entries.sort();
    let mut encoded: Vec<(PathBuf, Vec<PackageName>)> = Vec::new();
    let mut text = vec![dir.to_path_buf()];
    for path in entries {
        if path.is_dir() {
            text.push(path);
            continue;
        }
        let extension = path.extension().and_then(|extension| extension.to_str());
        if !matches!(extension, Some("wit" | "wasm" | "wat")) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if extension == Some("wat") || bytes.starts_with(b"\0asm") {
            let mut packages = Resolve::default();
            if packages.push_file(&path).is_ok() {
                encoded.push((path, packages.package_names.into_keys().collect()));
            }
        } else {
            text.push(path);
        }
    }
    if encoded.is_empty() {
        return Ok(());
    }
    for place in text {
        let group = if place.is_dir() {
            UnresolvedPackageGroup::parse_dir(&place).ok()
        } else {
            std::fs::read_to_string(&place)
                .ok()
                .and_then(|source| UnresolvedPackageGroup::parse(&place, &source).ok())
        };
        let Some(group) = group else {
            continue;
        };
        for package in std::iter::once(&group.main).chain(&group.nested) {
            let name = &package.name;
            if let Some((file, _)) = encoded.iter().find(|(_, names)| names.contains(name)) {
                return Err(error(format!(
                    "WIT package {name} is held twice in the consumed directory, in {} and, encoded, in {}: the bindings cannot read a package held both ways; remove one of them",
                    place.display(),
                    file.display()
                )));
            }
        }
    }
    Ok(())
}

/// The WIT packages an expansion parses, each set once, the first time something asks for it: the
/// consumed packages of `wit = "<dir>"`, when the attribute names a directory, the bundled WASI
/// packages, which an import in the `wasi` namespace names, and the kernel's package, which a
/// state, a resource or the context uses. An expansion that asks for none parses none.
pub(crate) struct Parsed {
    dir: Option<Directory>,
    bundled: OnceCell<syn::Result<crate::wasi::Bundled>>,
    guest: OnceCell<syn::Result<UnresolvedPackageGroup>>,
}

/// The consumed directory, `wit = "<dir>"`, and its packages with the files they were read from,
/// parsed once, the first time something asks for them.
pub(crate) struct Directory {
    path: PathBuf,
    packages: OnceCell<syn::Result<(Resolve, PackageSourceMap)>>,
}

impl Parsed {
    /// Nothing parsed yet, of the consumed directory `dir`, if the attribute names one.
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Parsed {
            dir: dir.map(|path| Directory {
                path,
                packages: OnceCell::new(),
            }),
            bundled: OnceCell::new(),
            guest: OnceCell::new(),
        }
    }

    /// The consumed directory, if the attribute names one.
    pub(crate) fn dir(&self) -> Option<&Directory> {
        self.dir.as_ref()
    }

    /// The bundled WASI packages, resolved, and each package with its source and its parse, in
    /// dependency order.
    pub(crate) fn bundled(&self) -> syn::Result<&crate::wasi::Bundled> {
        self.bundled
            .get_or_init(crate::wasi::bundled)
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The kernel's bundled `tembra:guest` package, parsed: its name, and what a reader that adds
    /// it to packages of its own pushes there.
    pub(crate) fn guest(&self) -> syn::Result<&UnresolvedPackageGroup> {
        self.guest
            .get_or_init(|| {
                UnresolvedPackageGroup::parse("guest.wit", GUEST).map_err(|(_, cause)| {
                    error(format!("the bundled tembra:guest package: {cause:#}"))
                })
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The consumed packages, owned, for the one reader that adds to them: the directory's,
    /// parsed now if nothing asked for them yet, or none without a directory.
    pub(crate) fn into_consumed(self) -> syn::Result<Resolve> {
        match self.dir {
            Some(Directory { path, packages }) => packages
                .into_inner()
                .unwrap_or_else(|| resolve(&path))
                .map(|(packages, _)| packages),
            None => Ok(Resolve::default()),
        }
    }
}

impl Directory {
    /// The directory's path, as `absolute` gives it.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The directory's packages, parsed the first time.
    pub(crate) fn packages(&self) -> syn::Result<&Resolve> {
        self.parsed().map(|(packages, _)| packages)
    }

    /// The files each of the directory's packages was read from, parsed the first time: the files
    /// of the directory that holds it, or its file of `deps/`, and none for a package decoded from
    /// an encoded file of `deps/`.
    pub(crate) fn sources(&self) -> syn::Result<&PackageSourceMap> {
        self.parsed().map(|(_, sources)| sources)
    }

    /// The directory's packages and their files, parsed the first time.
    fn parsed(&self) -> syn::Result<&(Resolve, PackageSourceMap)> {
        self.packages
            .get_or_init(|| resolve(&self.path))
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// The interface of the consumed packages with the given identity.
pub(crate) fn interface<'a>(
    resolve: &'a Resolve,
    dir: &Path,
    identity: &str,
) -> syn::Result<&'a wit_parser::Interface> {
    resolve
        .interfaces
        .iter()
        .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(identity))
        .map(|(_, interface)| interface)
        .ok_or_else(|| {
            error(format!(
                "{identity} is not an interface of the WIT packages in {}",
                dir.display()
            ))
        })
}

/// The consumed interfaces an Orb implements or imports, spelled for the bindings.
pub(crate) fn consumed<'a>(
    dir: &Directory,
    identities: impl Iterator<Item = &'a String>,
) -> syn::Result<BTreeMap<String, Consumed>> {
    use wit_parser::FunctionKind;
    let resolve = dir.packages()?;
    identities
        .map(|identity| {
            let interface = interface(resolve, dir.path(), identity)?;
            let [namespace, package, name] = crate::hook_path(identity);
            let import = format!("self::{namespace}::{package}::{name}");
            let export = format!("exports::{namespace}::{package}::{name}");
            let spell = |ty: wit_parser::Type| -> syn::Result<Spelled> {
                Ok(Spelled {
                    import: rust_type(resolve, ty, &import)?,
                    export: rust_type(resolve, ty, &export)?,
                    handles: handles(resolve, ty),
                })
            };
            let function = |function: &wit_parser::Function| -> syn::Result<ConsumedFunction> {
                Ok(ConsumedFunction {
                    name: function.item_name().to_owned(),
                    asynchronous: function.kind.is_async(),
                    params: function
                        .params
                        .iter()
                        .filter(|param| param.name != "self")
                        .map(|param| Ok((param.name.clone(), spell(param.ty)?)))
                        .collect::<syn::Result<_>>()?,
                    result: function.result.map(spell).transpose()?,
                })
            };
            let mut consumed = Consumed {
                functions: Vec::new(),
                resources: interface
                    .types
                    .iter()
                    .filter(|(_, id)| matches!(resolve.types[**id].kind, TypeDefKind::Resource))
                    .map(|(name, _)| ConsumedResource {
                        name: name.clone(),
                        constructor: None,
                        methods: Vec::new(),
                        statics: Vec::new(),
                    })
                    .collect(),
            };
            for definition in interface.functions.values() {
                let owner =
                    |consumed: &mut Consumed, id: wit_parser::TypeId| -> syn::Result<usize> {
                        let name = resolve.types[id].name.clone().unwrap_or_default();
                        consumed
                        .resources
                        .iter()
                        .position(|resource| resource.name == name)
                        .ok_or_else(|| {
                            error(format!(
                                "{identity}: the resource {name} is defined in another interface"
                            ))
                        })
                    };
                match definition.kind {
                    FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {
                        consumed.functions.push(function(definition)?);
                    }
                    FunctionKind::Constructor(id) => {
                        let params = function(definition)?.params;
                        let index = owner(&mut consumed, id)?;
                        consumed.resources[index].constructor = Some(params);
                    }
                    FunctionKind::Method(id) | FunctionKind::AsyncMethod(id) => {
                        let method = function(definition)?;
                        let index = owner(&mut consumed, id)?;
                        consumed.resources[index].methods.push(method);
                    }
                    FunctionKind::Static(id) | FunctionKind::AsyncStatic(id) => {
                        let function = function(definition)?;
                        let index = owner(&mut consumed, id)?;
                        consumed.resources[index].statics.push(function);
                    }
                    _ => {
                        return Err(error(format!(
                            "{identity}: the macro does not express property functions ({})",
                            definition.name
                        )));
                    }
                }
            }
            Ok((identity.clone(), consumed))
        })
        .collect()
}

/// What a consumed interface defines, as the bindings name it: each of its types and resources, in
/// the order of the interface's text, by the name the bindings give it; and the interfaces it
/// depends on.
pub(crate) struct Defined {
    items: Vec<(Ident, Definition)>,
    /// The interfaces it `use`s types from, directly or through others, by identity: a world
    /// importing it imports them too.
    pub depends: BTreeSet<String>,
}

/// A type or a resource of a consumed interface: its WIT name, what it is, the WASI interface
/// defining it, if one does, and whether the world's export of the interface holds a copy of it.
pub(crate) struct Definition {
    pub name: String,
    pub kind: Kind,
    /// The WASI interface the definition belongs to: the consumed interface itself, when it is one
    /// of WASI, or the one its alias chain leads into, `wasi:filesystem/types@0.3.0` for the
    /// `descriptor` another Orb's interface `use`s from there.
    pub wasi: Option<String>,
    /// Whether the bindings of the world's export of the interface give the definition a Rust type
    /// of its own, another than its import's (`copied`): false for one the interface `use`s from an
    /// interface the world only imports, which the import and the export alias alike.
    pub copied: bool,
}

/// What an item of a consumed interface is, for the places that name it.
pub(crate) enum Kind {
    /// A resource, whose handle a parameter takes.
    Resource,
    /// A type: a record, a variant, an enum, a flags, or an alias of one or of another type.
    Type {
        /// What the type is, for the errors: "a record".
        what: &'static str,
        /// What the type holds anywhere that is no data, if anything: a resource handle, a stream,
        /// a future or an error context.
        carries: Option<&'static str>,
        /// What the type holds anywhere that the runtime refuses in a signature though it is data,
        /// if anything: an option of an option, a map or a list of a fixed length.
        unsupported: Option<&'static str>,
        /// Whether the type is an option, through aliases, which an `Option` around it nests.
        option: bool,
        /// What the type holds anywhere that the bindings give no `serde` implementation, if
        /// anything: a flags, which they generate without the derives a state asks for.
        unwritable: Option<&'static str>,
    },
}

impl Defined {
    /// The type or resource the bindings name `ident`.
    pub(crate) fn get(&self, ident: &Ident) -> Option<&Definition> {
        self.items
            .iter()
            .find(|(name, _)| name == ident)
            .map(|(_, definition)| definition)
    }

    /// The types and the resources, by the names the bindings give them, for the errors.
    pub(crate) fn listed(&self) -> String {
        let names = |resources: bool| -> Vec<String> {
            self.items
                .iter()
                .filter(|(_, definition)| matches!(definition.kind, Kind::Resource) == resources)
                .map(|(name, _)| format!("`{name}`"))
                .collect()
        };
        let (types, resources) = (names(false), names(true));
        match (types.is_empty(), resources.is_empty()) {
            (true, true) => "it defines none".to_owned(),
            (false, true) => format!("its types are {}", types.join(", ")),
            (true, false) => format!("its resources are {}", resources.join(", ")),
            (false, false) => format!(
                "its types are {}; its resources are {}",
                types.join(", "),
                resources.join(", ")
            ),
        }
    }
}

/// What each consumed interface of `identities` defines, read from the packages of the consumed
/// directory, or from the bundled WASI packages for a WASI interface. `exported` are the consumed
/// interfaces the world exports.
pub(crate) fn defined<'a>(
    parsed: &Parsed,
    identities: impl Iterator<Item = &'a str>,
    exported: &BTreeSet<&str>,
) -> syn::Result<BTreeMap<String, Defined>> {
    use crate::wasi::is_wasi;
    let identities: BTreeSet<&str> = identities.collect();
    let bundled = identities
        .iter()
        .any(|identity| is_wasi(identity))
        .then(|| parsed.bundled())
        .transpose()?
        .map(|(bundled, _)| bundled);
    let consumed = match parsed.dir() {
        Some(dir) if identities.iter().any(|identity| !is_wasi(identity)) => {
            Some((dir.path(), dir.packages()?))
        }
        _ => None,
    };
    let mut defined = BTreeMap::new();
    for identity in identities {
        let (resolve, interface) = match (is_wasi(identity), bundled, consumed) {
            (true, Some(bundled), _) => {
                let interface = bundled
                    .interfaces
                    .iter()
                    .find(|(id, _)| bundled.id_of(*id).as_deref() == Some(identity))
                    .map(|(_, interface)| interface)
                    .ok_or_else(|| {
                        error(format!(
                            "{identity} is not an interface of the bundled WASI packages"
                        ))
                    })?;
                (bundled, interface)
            }
            (false, _, Some((dir, consumed))) => (consumed, interface(consumed, dir, identity)?),
            _ => {
                return Err(error(format!(
                    "{identity} is read from wit = \"<dir>\", the directory with its WIT package"
                )));
            }
        };
        let items = interface
            .types
            .iter()
            .map(|(name, id)| {
                let ty = wit_parser::Type::Id(*id);
                let kind = match dealias(resolve, ty) {
                    wit_parser::Type::Id(target)
                        if matches!(resolve.types[target].kind, TypeDefKind::Resource) =>
                    {
                        Kind::Resource
                    }
                    _ => Kind::Type {
                        what: match resolve.types[*id].kind {
                            TypeDefKind::Record(_) => "a record",
                            TypeDefKind::Variant(_) => "a variant",
                            TypeDefKind::Enum(_) => "an enum",
                            TypeDefKind::Flags(_) => "a flags",
                            _ => "a type",
                        },
                        carries: crate::keyed::no_data(resolve, ty),
                        unsupported: unsupported(resolve, ty),
                        option: optional(resolve, ty),
                        unwritable: unwritable(resolve, ty),
                    },
                };
                let definition = Definition {
                    name: name.clone(),
                    kind,
                    wasi: wasi_origin(resolve, ty),
                    copied: copied(resolve, ty, exported),
                };
                (crate::type_ident(name), definition)
            })
            .collect();
        let mut pending: Vec<wit_parser::InterfaceId> = interface
            .types
            .values()
            .filter_map(|id| resolve.type_interface_dep(*id))
            .collect();
        let (mut seen, mut depends) = (BTreeSet::new(), BTreeSet::new());
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                depends.extend(resolve.id_of(id));
                pending.extend(resolve.interface_direct_deps(id));
            }
        }
        defined.insert(identity.to_owned(), Defined { items, depends });
    }
    Ok(defined)
}

/// Whether the bindings give a WIT type of an interface the world exports a Rust type of its own in
/// the export, another than the import's: whether the type is, or holds, a record, a variant, an
/// enum, a flags or a resource of one of the consumed interfaces the world exports, `exported`,
/// reached through aliases and the types of such interfaces. `wit-bindgen` copies the types of an
/// interface the world imports and exports into the export, and those of the exported interfaces
/// it `use`s, but aliases a type it `use`s from an interface the world only imports to that
/// import's, as the import does: `use types.{admission};` in a hook makes the hook's import, its
/// export and the view of its keyed import all name the one `admission` of `types`.
fn copied(resolve: &Resolve, ty: wit_parser::Type, exported: &BTreeSet<&str>) -> bool {
    let wit_parser::Type::Id(id) = ty else {
        return false;
    };
    let definition = &resolve.types[id];
    if let wit_parser::TypeOwner::Interface(owner) = definition.owner
        && !resolve
            .id_of(owner)
            .is_some_and(|identity| exported.contains(identity.as_str()))
    {
        return false;
    }
    let copied = |ty: wit_parser::Type| copied(resolve, ty, exported);
    match &definition.kind {
        TypeDefKind::Record(_)
        | TypeDefKind::Variant(_)
        | TypeDefKind::Enum(_)
        | TypeDefKind::Flags(_)
        | TypeDefKind::Resource => true,
        TypeDefKind::Type(inner)
        | TypeDefKind::Option(inner)
        | TypeDefKind::List(inner)
        | TypeDefKind::FixedLengthList(inner, _) => copied(*inner),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().copied().any(copied),
        TypeDefKind::Map(key, value) => copied(*key) || copied(*value),
        TypeDefKind::Result(result) => result.ok.into_iter().chain(result.err).any(copied),
        TypeDefKind::Handle(
            wit_parser::Handle::Own(resource) | wit_parser::Handle::Borrow(resource),
        ) => copied(wit_parser::Type::Id(*resource)),
        TypeDefKind::Future(inner) | TypeDefKind::Stream(inner) => inner.is_some_and(copied),
        TypeDefKind::Unknown => false,
    }
}

/// The WASI interface a WIT type belongs to, if one does: the interface defining it, or the one
/// its alias chain leads into, since `use wasi:filesystem/types@0.3.0.{descriptor};` makes
/// `descriptor` an alias of the WASI resource in the interface writing it.
fn wasi_origin(resolve: &Resolve, mut ty: wit_parser::Type) -> Option<String> {
    while let wit_parser::Type::Id(id) = ty {
        let definition = &resolve.types[id];
        if let wit_parser::TypeOwner::Interface(owner) = definition.owner
            && let Some(identity) = resolve.id_of(owner)
            && crate::wasi::is_wasi(&identity)
        {
            return Some(identity);
        }
        let TypeDefKind::Type(inner) = definition.kind else {
            return None;
        };
        ty = inner;
    }
    None
}

/// What a WIT type holds anywhere that `test` finds, if anything: the first answer of `test`,
/// asked about the type itself, then about each type it holds, in order, through records,
/// variants, tuples, options, lists, maps, results and aliases. `test` is asked about the types
/// without an id as well, such as an error context. The walk does not look into what a handle
/// refers to or what a future or a stream carries, and a resource, a flags and an enum hold no
/// type.
pub(crate) fn anywhere<T>(
    resolve: &Resolve,
    ty: wit_parser::Type,
    test: impl Fn(wit_parser::Type) -> Option<T>,
) -> Option<T> {
    fn walk<T>(
        resolve: &Resolve,
        ty: wit_parser::Type,
        test: &dyn Fn(wit_parser::Type) -> Option<T>,
        seen: &mut BTreeSet<wit_parser::TypeId>,
    ) -> Option<T> {
        let wit_parser::Type::Id(id) = ty else {
            return test(ty);
        };
        // A type already looked at answered no, or the walk would have stopped there.
        if !seen.insert(id) {
            return None;
        }
        if let Some(found) = test(ty) {
            return Some(found);
        }
        let mut holds = |ty: wit_parser::Type| walk(resolve, ty, test, seen);
        match &resolve.types[id].kind {
            TypeDefKind::Record(record) => record.fields.iter().find_map(|field| holds(field.ty)),
            TypeDefKind::Variant(variant) => variant
                .cases
                .iter()
                .filter_map(|case| case.ty)
                .find_map(holds),
            TypeDefKind::Tuple(tuple) => tuple.types.iter().copied().find_map(holds),
            TypeDefKind::Option(inner)
            | TypeDefKind::List(inner)
            | TypeDefKind::FixedLengthList(inner, _)
            | TypeDefKind::Type(inner) => holds(*inner),
            TypeDefKind::Map(key, value) => holds(*key).or_else(|| holds(*value)),
            TypeDefKind::Result(result) => result.ok.into_iter().chain(result.err).find_map(holds),
            TypeDefKind::Handle(_)
            | TypeDefKind::Future(_)
            | TypeDefKind::Stream(_)
            | TypeDefKind::Resource
            | TypeDefKind::Flags(_)
            | TypeDefKind::Enum(_)
            | TypeDefKind::Unknown => None,
        }
    }
    walk(resolve, ty, &test, &mut BTreeSet::new())
}

/// What a WIT type of a consumed package holds anywhere that its bindings give no `serde`
/// implementation, if anything, as `unwritable_itself` finds it in each type.
fn unwritable(resolve: &Resolve, ty: wit_parser::Type) -> Option<&'static str> {
    anywhere(resolve, ty, |ty| unwritable_itself(resolve, ty))
}

/// Whether a WIT type of a consumed package is itself one its bindings give no `serde`
/// implementation, and what: a flags, which they generate as a bitflags type without the derives
/// a state asks for, or a list of a fixed length beyond those `serde` implements.
fn unwritable_itself(resolve: &Resolve, ty: wit_parser::Type) -> Option<&'static str> {
    let wit_parser::Type::Id(id) = ty else {
        return None;
    };
    match &resolve.types[id].kind {
        TypeDefKind::Flags(_) => Some("a flags"),
        TypeDefKind::FixedLengthList(_, length) if *length > 32 => {
            Some("a list of a fixed length above 32")
        }
        _ => None,
    }
}

/// What a WIT type holds anywhere that the runtime refuses in a signature though it is data, if
/// anything, as tembra-contracts' `supported` refuses it: an option of an option, which only a
/// tagged codec would tell from a missing value, and a map or a list of a fixed length, which need
/// a value adapter of their own. What is no data, which `supported` refuses too, is
/// `keyed::no_data`'s to find. An option a Rust type spells around an option is `wit_type`'s,
/// `Option<Option<T>>`, or the consumed-type check's, `Option<catalog::Opt>` for an option `opt`.
pub(crate) fn unsupported(resolve: &Resolve, ty: wit_parser::Type) -> Option<&'static str> {
    anywhere(resolve, ty, |ty| {
        let wit_parser::Type::Id(id) = ty else {
            return None;
        };
        match &resolve.types[id].kind {
            TypeDefKind::Option(inner) if optional(resolve, *inner) => {
                Some("an option of an option")
            }
            TypeDefKind::Map(..) => Some("a map"),
            TypeDefKind::FixedLengthList(..) => Some("a list of a fixed length"),
            _ => None,
        }
    })
}

/// Where a WIT type holds owned resource handles, as the bindings name the resources: a handle
/// itself, or an option, a list, a result or a tuple around one, named or not, through aliases. A
/// record or a variant is the bindings' own type, which the author builds with the handles in it.
fn handles(resolve: &Resolve, ty: wit_parser::Type) -> Option<Handles> {
    let wit_parser::Type::Id(id) = dealias(resolve, ty) else {
        return None;
    };
    let part = |ty: Option<wit_parser::Type>| ty.and_then(|ty| handles(resolve, ty));
    match &resolve.types[id].kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => {
            let name = resolve.types[*resource].name.as_deref()?;
            Some(Handles::Resource(crate::type_ident(name)))
        }
        TypeDefKind::Option(inner) => Handles::option(handles(resolve, *inner)),
        TypeDefKind::List(inner) => Handles::list(handles(resolve, *inner)),
        TypeDefKind::Result(result) => Handles::result(part(result.ok), part(result.err)),
        TypeDefKind::Tuple(tuple) => {
            Handles::tuple(tuple.types.iter().map(|ty| handles(resolve, *ty)).collect())
        }
        _ => None,
    }
}

/// What the bindings make of the WIT they read, for what the macro asks them to add to the types
/// they generate.
#[derive(Default)]
pub(crate) struct Bindings {
    /// The records and variants of the world's interfaces, which the bindings generate, that cannot
    /// derive `serde`'s traits: those holding a consumed package's flags type, a resource handle, a
    /// stream or a future, directly or through other types, which the bindings give no `serde`
    /// implementation. They are named as the `additional_derives_ignore` option of `wit-bindgen`
    /// compares them, by `ignored_name`.
    pub unserializable: Vec<String>,
    /// The records, variants and enums of the world's interfaces, which the bindings generate,
    /// that can derive `serde`'s traits but share their name, as `ignored_name` gives it, with one
    /// of `unserializable`, so the ignore option leaves them without the derives as well. They are
    /// named by their full names, as in `types`, for the selectors that give the derives back.
    pub namesakes: BTreeSet<String>,
    /// How the bindings generate each record, variant and enum of an interface, by the type's full
    /// name as the selectors of `additional_type_attributes` write it,
    /// `ns:name/interface@version/type`.
    pub types: BTreeMap<String, Generated>,
}

/// How `wit-bindgen` 0.63 generates a record, a variant or an enum, as `wit-bindgen-core` records
/// it to choose what it derives and implements: whether the type holds a string or a list, whether
/// it holds a resource handle, and whether a function of a world returns it as the error of its
/// result; and, for the derives the macro carries onto the type, whether every value of it holds a
/// resource handle, whether it holds what the bindings give no `serde` implementation, whether the
/// bindings derive serde's traits on it, and the parts of its fields that decide a derive of the
/// standard library.
#[derive(Clone, Default)]
pub(crate) struct Generated {
    pub list: bool,
    pub resource: bool,
    pub error: bool,
    /// No value of the type is without a resource handle: a record with a field that is a handle,
    /// a tuple holding one or such a type. A handle in an option or a list may be absent, and a
    /// variant may take a case without one; `Default`, which makes a value from nothing, cannot.
    pub needs_handle: bool,
    /// The type holds, anywhere, a value the bindings give no `serde` implementation, as
    /// `holds_unserializable` tells: besides a resource handle, a type of the module can hold a
    /// consumed package's flags type, alone or in a consumed type.
    pub unserializable: bool,
    /// In an Orb without a state, where the bindings derive serde's traits on no type, the type
    /// holds, anywhere, a type of a consumed package, which then has no `serde` implementation
    /// either, or a resource handle; with a state, unset.
    pub consumed: bool,
    /// The bindings derive serde's traits on the type: in an Orb with a state, unless the ignore
    /// option leaves the type out for what it holds; a type left out for its name alone, one of
    /// `Bindings::namesakes`, gets them back through a selector and counts as serialized.
    pub serialized: bool,
    /// The parts of the type's fields, or of its cases' payloads, that decide what the generated
    /// type can derive of the standard library's traits.
    pub parts: Vec<Part>,
}

/// A part of a record's fields or a variant's payloads that decides what the generated type can
/// derive, reached through the options, lists, results, tuples and aliases around it.
#[derive(Clone)]
pub(crate) struct Part {
    pub held: Held,
    /// The WIT name of the field or case holding the part.
    pub member: String,
    /// `Default` makes a value without the part: it sits in an option or a list, which `Default`
    /// leaves empty, or in a case of a variant, whose `Default` is its `#[default]` case.
    pub absent_in_default: bool,
}

/// What a part of a type is, as far as it decides a derive of the standard library: a resource
/// handle implements none of those traits, a string or a list is not `Copy`, an `f32` or an `f64`
/// implements no `Eq`, `Ord` or `Hash`, a result no `Default`, and a named type what its bindings,
/// or the macro, give it; the other primitives implement every one of those traits.
#[derive(Clone)]
pub(crate) enum Held {
    /// An owned resource handle.
    Handle,
    /// A string or a list.
    List,
    /// An `f32` or an `f64`, by its name.
    Float(&'static str),
    /// A `result`.
    Result,
    /// A record, a variant, an enum or a flags of the Orb's own package, by its WIT name: its
    /// generated type, or the author's flags struct, implements what the macro knows of it.
    Own(String),
    /// A record, a variant, an enum or a flags of a consumed package: the name the bindings give it,
    /// its kind with an article, for the errors, the interface defining it, and whether it holds a
    /// string or a list, which no impl makes `Copy`. What else its bindings do not give it,
    /// `PartialEq` for a record, `Default` for each, the author may implement for it by hand,
    /// anywhere in the crate; one holding a resource handle is refused before any derive is read.
    Consumed {
        name: String,
        kind: &'static str,
        interface: String,
        list: bool,
    },
}

/// Read the WIT the bindings read, for what they make of its types, where a state, `serde`, makes
/// them derive serde's traits on the types that can carry them: the consumed packages `parsed`
/// holds, which makes this their last reader, the bundled packages the inline WIT nests, the
/// kernel's when `guest` and the WASI packages `wasi` (`wasi::packages`), each as `parsed` holds
/// it parsed, and the module's own WIT, `wit`, which the inline WIT starts with. A WIT the bindings
/// would refuse yields nothing, and the bindings report it.
pub(crate) fn bindings(
    parsed: Parsed,
    wit: &str,
    guest: bool,
    wasi: &[PackageId],
    serde: bool,
) -> Bindings {
    let mut nested = Vec::new();
    if guest {
        let Ok(parse) = parsed.guest() else {
            return Bindings::default();
        };
        nested.push(parse.clone());
    }
    if !wasi.is_empty() {
        let Ok((_, sources)) = parsed.bundled() else {
            return Bindings::default();
        };
        nested.extend(
            sources
                .iter()
                .filter(|(id, ..)| wasi.contains(id))
                .map(|(.., parse)| parse.clone()),
        );
    }
    let Ok(mut resolve) = parsed.into_consumed() else {
        return Bindings::default();
    };
    // The nested packages resolve after the consumed ones, as the bindings resolve the inline WIT,
    // each after the packages it builds on.
    for parse in nested {
        if resolve.push_group(parse).is_err() {
            return Bindings::default();
        }
    }
    #[cfg(test)]
    let before = resolve.packages.len();
    let Ok(own) = resolve.push_str("orb.wit", wit) else {
        return Bindings::default();
    };
    // The text parsed here holds no bundled WASI package, which the expansion parsed once and the
    // loop above pushed parsed; a test build counts it as a second parse of them if it does.
    #[cfg(test)]
    if resolve
        .packages
        .iter()
        .skip(before)
        .any(|(_, package)| package.name.namespace == "wasi")
    {
        crate::wasi::BUNDLED_PARSES.set(crate::wasi::BUNDLED_PARSES.get() + 1);
    }
    // The bindings generate the types of every interface the world imports or exports, and the
    // derives reach those alone.
    let generated: BTreeSet<wit_parser::InterfaceId> = resolve.packages[own]
        .worlds
        .values()
        .flat_map(|world| {
            let world = &resolve.worlds[*world];
            world.imports.values().chain(world.exports.values())
        })
        .filter_map(|item| match item {
            wit_parser::WorldItem::Interface { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let in_world = |owner: wit_parser::TypeOwner| matches!(owner, wit_parser::TypeOwner::Interface(owner) if generated.contains(&owner));
    let unserializable: BTreeSet<String> = resolve
        .types
        .iter()
        .filter(|(_, definition)| {
            matches!(
                definition.kind,
                TypeDefKind::Record(_) | TypeDefKind::Variant(_)
            ) && in_world(definition.owner)
        })
        .filter(|(id, _)| holds_unserializable(&resolve, wit_parser::Type::Id(*id), own, false))
        .filter_map(|(_, definition)| definition.name.as_deref())
        .map(ignored_name)
        .collect();
    // Every record, variant and enum an interface defines, by its full name. `holds` reports owned
    // handles, streams and futures where the bindings count any resource, but a type of the module
    // holds no borrowed handle, bare resource or error context, which no Rust type the macro
    // translates spells, so for such a type the two agree.
    let full_name = |id: wit_parser::TypeId| {
        let definition = &resolve.types[id];
        let wit_parser::TypeOwner::Interface(owner) = definition.owner else {
            return None;
        };
        Some(format!(
            "{}/{}",
            resolve.id_of(owner)?,
            definition.name.as_ref()?
        ))
    };
    // In an Orb with a state, the ignore option also leaves out of the derives the types that only
    // share a name with one in `unserializable`.
    let namesakes: BTreeSet<String> = resolve
        .types
        .iter()
        .filter(|(_, definition)| {
            serde
                && matches!(
                    definition.kind,
                    TypeDefKind::Record(_) | TypeDefKind::Variant(_) | TypeDefKind::Enum(_)
                )
                && in_world(definition.owner)
                && definition
                    .name
                    .as_deref()
                    .is_some_and(|name| unserializable.contains(&ignored_name(name)))
        })
        .filter(|(id, _)| !holds_unserializable(&resolve, wit_parser::Type::Id(*id), own, false))
        .filter_map(|(id, _)| full_name(id))
        .collect();
    let mut types: BTreeMap<String, Generated> = resolve
        .types
        .iter()
        .filter(|(_, definition)| {
            matches!(
                definition.kind,
                TypeDefKind::Record(_) | TypeDefKind::Variant(_) | TypeDefKind::Enum(_)
            )
        })
        .filter_map(|(id, definition)| {
            let name = full_name(id)?;
            let (list, resource) = holds(&resolve, wit_parser::Type::Id(id));
            // The ignore option leaves the type out of serde's derives by its `ignored_name`, where
            // a namesake takes them back through a selector.
            let ignored = definition
                .name
                .as_deref()
                .is_some_and(|name| unserializable.contains(&ignored_name(name)));
            let serialized = serde && (namesakes.contains(&name) || !ignored);
            let unserializable = |stateless: bool| {
                holds_unserializable(&resolve, wit_parser::Type::Id(id), own, stateless)
            };
            let generated = Generated {
                list,
                resource,
                error: false,
                needs_handle: needs_handle(&resolve, wit_parser::Type::Id(id)),
                unserializable: unserializable(false),
                consumed: !serde && unserializable(true),
                serialized,
                parts: parts(&resolve, id, own),
            };
            Some((name, generated))
        })
        .collect();
    // The bindings mark the error of the result of every function of every world, through the
    // aliases `use` leaves, and implement `Display` and `Error` for it.
    for (_, world) in resolve.worlds.iter() {
        for item in world.imports.values().chain(world.exports.values()) {
            let functions: Vec<&wit_parser::Function> = match item {
                wit_parser::WorldItem::Function(function) => vec![function],
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].functions.values().collect()
                }
                wit_parser::WorldItem::Type { .. } => Vec::new(),
            };
            for function in functions {
                let Some(wit_parser::Type::Id(result)) = function.result else {
                    continue;
                };
                let TypeDefKind::Result(result) = &resolve.types[result].kind else {
                    continue;
                };
                let Some(wit_parser::Type::Id(mut error)) = result.err else {
                    continue;
                };
                while let TypeDefKind::Type(wit_parser::Type::Id(aliased)) =
                    resolve.types[error].kind
                {
                    error = aliased;
                }
                if let Some(generated) = full_name(error).and_then(|name| types.get_mut(&name)) {
                    generated.error = true;
                }
            }
        }
    }
    Bindings {
        unserializable: unserializable.into_iter().collect(),
        namesakes,
        types,
    }
}

/// The name by which the `additional_derives_ignore` option of `wit-bindgen` compares a type of
/// the given WIT name: the kebab-case form of the Rust name the bindings give it.
pub(crate) fn ignored_name(name: &str) -> String {
    name.to_upper_camel_case().to_kebab_case()
}

/// Whether a WIT type holds, anywhere, a value the bindings give no `serde` implementation: what
/// is no data, as `keyed::no_data_itself` finds it, a resource handle, a stream, a future or an
/// error context, and what `unwritable_itself` finds, an array longer than `serde`'s or a flags
/// type, but a flags type of `own`, the Orb's package, whose flags types are the author's structs;
/// and, `stateless`, in an Orb without a state, where the bindings derive serde's traits on no
/// type, a record, a variant or an enum of another package too.
fn holds_unserializable(
    resolve: &Resolve,
    ty: wit_parser::Type,
    own: wit_parser::PackageId,
    stateless: bool,
) -> bool {
    let foreign = |id: wit_parser::TypeId| match resolve.types[id].owner {
        wit_parser::TypeOwner::Interface(owner) => resolve.interfaces[owner].package != Some(own),
        _ => true,
    };
    anywhere(resolve, ty, |ty| {
        if let wit_parser::Type::Id(id) = ty {
            match &resolve.types[id].kind {
                TypeDefKind::Flags(_) if !foreign(id) => return None,
                TypeDefKind::Record(_) | TypeDefKind::Variant(_) | TypeDefKind::Enum(_)
                    if stateless && foreign(id) =>
                {
                    return Some(());
                }
                _ => {}
            }
        }
        crate::keyed::no_data_itself(resolve, ty)
            .or_else(|| unwritable_itself(resolve, ty))
            .map(|_| ())
    })
    .is_some()
}

/// The callback of every hook: the single function of its interface in the consumed packages.
pub(crate) fn callbacks<'a>(
    dir: &Directory,
    hooks: impl Iterator<Item = &'a String>,
) -> syn::Result<BTreeMap<String, Callback>> {
    let resolve = dir.packages()?;
    hooks
        .map(|hook| {
            let interface = interface(resolve, dir.path(), hook)?;
            let mut functions = interface.functions.values();
            let (Some(function), None) = (functions.next(), functions.next()) else {
                return Err(error(format!(
                    "hook {hook} must declare exactly one callback"
                )));
            };
            let [param] = function.params.as_slice() else {
                return Err(error(format!(
                    "the callback of hook {hook} takes exactly one input"
                )));
            };
            let [namespace, package, interface] = crate::hook_path(hook);
            let import = format!("self::{namespace}::{package}::{interface}");
            let export = format!("exports::{namespace}::{package}::{interface}");
            let spell = |ty: wit_parser::Type| -> syn::Result<Spelled> {
                Ok(Spelled {
                    import: rust_type(resolve, ty, &import)?,
                    export: rust_type(resolve, ty, &export)?,
                    handles: handles(resolve, ty),
                })
            };
            Ok((
                hook.clone(),
                Callback {
                    name: function.name.clone(),
                    series: function.result.is_none(),
                    waterfall: function
                        .result
                        .is_some_and(|result| same_type(resolve, result, param.ty)),
                    bail: function.result.is_some_and(|ty| optional(resolve, ty)),
                    input: spell(param.ty)?,
                    result: function.result.map(spell).transpose()?,
                    passed: passed(resolve, param.ty),
                },
            ))
        })
        .collect()
}

/// A WIT type with its aliases followed: `type out = admission` is `admission`.
fn dealias(resolve: &Resolve, mut ty: wit_parser::Type) -> wit_parser::Type {
    while let wit_parser::Type::Id(id) = ty
        && let TypeDefKind::Type(inner) = resolve.types[id].kind
    {
        ty = inner;
    }
    ty
}

/// Whether two WIT types are one, through aliases: a record, a variant, an enum, a flags type or
/// a resource is itself, and a list, an option, a result, a tuple or a handle, named or not, is
/// the same shape over the same types. The kernel holds a waterfall callback's input and result
/// to the same rule.
fn same_type(resolve: &Resolve, left: wit_parser::Type, right: wit_parser::Type) -> bool {
    let (left, right) = (dealias(resolve, left), dealias(resolve, right));
    let (wit_parser::Type::Id(left), wit_parser::Type::Id(right)) = (left, right) else {
        return left == right;
    };
    if left == right {
        return true;
    }
    let (left, right) = (&resolve.types[left], &resolve.types[right]);
    let same = |left: wit_parser::Type, right: wit_parser::Type| same_type(resolve, left, right);
    let same_side =
        |left: Option<wit_parser::Type>, right: Option<wit_parser::Type>| match (left, right) {
            (Some(left), Some(right)) => same(left, right),
            (None, None) => true,
            _ => false,
        };
    match (&left.kind, &right.kind) {
        (TypeDefKind::List(left), TypeDefKind::List(right))
        | (TypeDefKind::Option(left), TypeDefKind::Option(right)) => same(*left, *right),
        (TypeDefKind::Result(left), TypeDefKind::Result(right)) => {
            same_side(left.ok, right.ok) && same_side(left.err, right.err)
        }
        (TypeDefKind::Tuple(left), TypeDefKind::Tuple(right)) => {
            left.types.len() == right.types.len()
                && left
                    .types
                    .iter()
                    .zip(&right.types)
                    .all(|(left, right)| same(*left, *right))
        }
        (
            TypeDefKind::Handle(wit_parser::Handle::Own(left)),
            TypeDefKind::Handle(wit_parser::Handle::Own(right)),
        )
        | (
            TypeDefKind::Handle(wit_parser::Handle::Borrow(left)),
            TypeDefKind::Handle(wit_parser::Handle::Borrow(right)),
        ) => same(wit_parser::Type::Id(*left), wit_parser::Type::Id(*right)),
        _ => false,
    }
}

/// What `wit-bindgen` records of a type to choose how an import takes it: whether the type holds a
/// list or a string, and whether it holds an owned handle, a stream or a future, anywhere.
fn holds(resolve: &Resolve, ty: wit_parser::Type) -> (bool, bool) {
    let wit_parser::Type::Id(id) = ty else {
        return (ty == wit_parser::Type::String, false);
    };
    let all = |types: Vec<wit_parser::Type>| {
        types.into_iter().fold((false, false), |(list, own), ty| {
            let (inner_list, inner_own) = holds(resolve, ty);
            (list || inner_list, own || inner_own)
        })
    };
    match &resolve.types[id].kind {
        TypeDefKind::Record(record) => all(record.fields.iter().map(|field| field.ty).collect()),
        TypeDefKind::Variant(variant) => {
            all(variant.cases.iter().filter_map(|case| case.ty).collect())
        }
        TypeDefKind::Tuple(tuple) => all(tuple.types.clone()),
        TypeDefKind::Result(result) => all(result.ok.into_iter().chain(result.err).collect()),
        TypeDefKind::List(inner) => (true, holds(resolve, *inner).1),
        TypeDefKind::Map(key, value) => (true, all(vec![*key, *value]).1),
        TypeDefKind::Option(inner)
        | TypeDefKind::Type(inner)
        | TypeDefKind::FixedLengthList(inner, _) => holds(resolve, *inner),
        TypeDefKind::Handle(wit_parser::Handle::Own(_))
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_) => (false, true),
        _ => (false, false),
    }
}

/// Whether every value of a WIT type holds an owned resource handle, which no `Default` makes: a
/// handle itself, a record or a tuple with such a part, or a variant or a result whose every case
/// has one; an option or a list may be empty, and a variant may take another case.
fn needs_handle(resolve: &Resolve, ty: wit_parser::Type) -> bool {
    let wit_parser::Type::Id(id) = ty else {
        return false;
    };
    let needs = |ty: wit_parser::Type| needs_handle(resolve, ty);
    match &resolve.types[id].kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(_)) => true,
        TypeDefKind::Record(record) => record.fields.iter().any(|field| needs(field.ty)),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().copied().any(needs),
        TypeDefKind::Variant(variant) => {
            variant.cases.iter().all(|case| case.ty.is_some_and(needs))
        }
        TypeDefKind::Result(result) => {
            result.ok.is_some_and(needs) && result.err.is_some_and(needs)
        }
        TypeDefKind::FixedLengthList(inner, length) => *length > 0 && needs(*inner),
        TypeDefKind::Type(inner) => needs(*inner),
        _ => false,
    }
}

/// The parts of a record's fields or a variant's payloads that decide what the generated type can
/// derive (`Part`), each with the field or case holding it; an enum has none.
fn parts(resolve: &Resolve, id: wit_parser::TypeId, own: wit_parser::PackageId) -> Vec<Part> {
    let mut parts = Vec::new();
    match &resolve.types[id].kind {
        TypeDefKind::Record(record) => {
            for field in &record.fields {
                held(resolve, field.ty, own, &field.name, false, &mut parts);
            }
        }
        TypeDefKind::Variant(variant) => {
            for case in &variant.cases {
                if let Some(ty) = case.ty {
                    held(resolve, ty, own, &case.name, true, &mut parts);
                }
            }
        }
        _ => {}
    }
    parts
}

/// The parts the member `member` of a type holds in `ty`, into `parts`: a resource handle, a
/// string, a list or a map and what it holds, an `f32` or an `f64`, a result and what its sides
/// hold, and a named type of the Orb's package or of a consumed one, through the options, lists,
/// maps, tuples, fixed-length lists and aliases around them; an option, a list or a map makes what
/// it holds `absent` in `Default`, as the member may be already.
fn held(
    resolve: &Resolve,
    ty: wit_parser::Type,
    own: wit_parser::PackageId,
    member: &str,
    absent: bool,
    parts: &mut Vec<Part>,
) {
    let part = |held: Held, absent: bool| Part {
        held,
        member: member.to_owned(),
        absent_in_default: absent,
    };
    let id = match ty {
        wit_parser::Type::String => {
            parts.push(part(Held::List, absent));
            return;
        }
        wit_parser::Type::F32 => {
            parts.push(part(Held::Float("f32"), absent));
            return;
        }
        wit_parser::Type::F64 => {
            parts.push(part(Held::Float("f64"), absent));
            return;
        }
        wit_parser::Type::Id(id) => id,
        _ => return,
    };
    let definition = &resolve.types[id];
    let kind = match &definition.kind {
        TypeDefKind::Record(_) => "a record",
        TypeDefKind::Variant(_) => "a variant",
        TypeDefKind::Enum(_) => "an enum",
        TypeDefKind::Flags(_) => "a flags",
        TypeDefKind::Handle(_) => {
            parts.push(part(Held::Handle, absent));
            return;
        }
        TypeDefKind::Type(inner) | TypeDefKind::FixedLengthList(inner, _) => {
            return held(resolve, *inner, own, member, absent, parts);
        }
        TypeDefKind::Option(inner) => return held(resolve, *inner, own, member, true, parts),
        TypeDefKind::List(inner) => {
            parts.push(part(Held::List, absent));
            return held(resolve, *inner, own, member, true, parts);
        }
        TypeDefKind::Map(key, value) => {
            parts.push(part(Held::List, absent));
            held(resolve, *key, own, member, true, parts);
            return held(resolve, *value, own, member, true, parts);
        }
        TypeDefKind::Tuple(tuple) => {
            for ty in &tuple.types {
                held(resolve, *ty, own, member, absent, parts);
            }
            return;
        }
        TypeDefKind::Result(result) => {
            parts.push(part(Held::Result, absent));
            for ty in result.ok.into_iter().chain(result.err) {
                held(resolve, ty, own, member, absent, parts);
            }
            return;
        }
        TypeDefKind::Resource
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_)
        | TypeDefKind::Unknown => return,
    };
    let (Some(name), wit_parser::TypeOwner::Interface(owner)) =
        (definition.name.as_deref(), definition.owner)
    else {
        return;
    };
    if resolve.interfaces[owner].package == Some(own) {
        parts.push(part(Held::Own(name.to_owned()), absent));
        return;
    }
    let (list, _) = holds(resolve, ty);
    parts.push(part(
        Held::Consumed {
            name: crate::type_ident(name).to_string(),
            kind,
            interface: resolve.id_of(owner).unwrap_or_default(),
            list,
        },
        absent,
    ));
}

/// How an imported function takes a part of its parameter, as `wit-bindgen` binds it by default:
/// a parameter holding a string or a list and no owned handle is borrowed at its top layer, which
/// reaches through the anonymous options, results and tuples to the strings, lists and named
/// types they hold; everything else is taken by value.
pub(crate) enum Passed {
    /// By value, with everything the part holds.
    Owned,
    /// A string, as `&str`.
    Str,
    /// A list, as `&[T]`.
    List,
    /// Any other named type holding a string or a list, as `&T`.
    Named,
    /// An option whose value is taken so.
    Option(Box<Passed>),
    /// A result whose sides are taken so; a side without a value is `Owned`.
    Result(Box<Passed>, Box<Passed>),
    /// A tuple whose elements are taken so.
    Tuple(Vec<Passed>),
}

/// How an imported function takes a parameter of this type.
fn passed(resolve: &Resolve, ty: wit_parser::Type) -> Passed {
    let (list, own) = holds(resolve, ty);
    if own || !list {
        return Passed::Owned;
    }
    let named = matches!(ty, wit_parser::Type::Id(id) if resolve.types[id].name.is_some());
    // A string, or an alias of one.
    let wit_parser::Type::Id(id) = dealias(resolve, ty) else {
        return Passed::Str;
    };
    let definition = &resolve.types[id];
    let side =
        |ty: Option<wit_parser::Type>| Box::new(ty.map_or(Passed::Owned, |ty| passed(resolve, ty)));
    match &definition.kind {
        TypeDefKind::List(_) => Passed::List,
        _ if named || definition.name.is_some() => Passed::Named,
        TypeDefKind::Option(inner) => Passed::Option(Box::new(passed(resolve, *inner))),
        TypeDefKind::Result(result) => Passed::Result(side(result.ok), side(result.err)),
        TypeDefKind::Tuple(tuple) => {
            Passed::Tuple(tuple.types.iter().map(|ty| passed(resolve, *ty)).collect())
        }
        TypeDefKind::Map(..) => Passed::Named,
        _ => Passed::Owned,
    }
}

/// Whether a WIT type is an option, through aliases.
fn optional(resolve: &Resolve, mut ty: wit_parser::Type) -> bool {
    loop {
        let wit_parser::Type::Id(id) = ty else {
            return false;
        };
        match &resolve.types[id].kind {
            TypeDefKind::Option(_) => return true,
            TypeDefKind::Type(inner) => ty = *inner,
            _ => return false,
        }
    }
}

/// Spell a WIT type the way `wit-bindgen` names it in `module`, the generated module of its
/// interface: named types in UpperCamelCase, lists and strings from `alloc`, options, results and
/// tuples structurally.
fn rust_type(resolve: &Resolve, ty: wit_parser::Type, module: &str) -> syn::Result<String> {
    use wit_parser::Type;
    Ok(match ty {
        Type::Bool => "bool".into(),
        Type::U8 => "u8".into(),
        Type::U16 => "u16".into(),
        Type::U32 => "u32".into(),
        Type::U64 => "u64".into(),
        Type::S8 => "i8".into(),
        Type::S16 => "i16".into(),
        Type::S32 => "i32".into(),
        Type::S64 => "i64".into(),
        Type::F32 => "f32".into(),
        Type::F64 => "f64".into(),
        Type::Char => "char".into(),
        Type::String => "__tembra_alloc::string::String".into(),
        Type::ErrorContext => {
            return Err(error("a hook callback cannot carry an error context"));
        }
        Type::Id(id) => {
            let definition = &resolve.types[id];
            if let Some(name) = &definition.name {
                return Ok(format!("{module}::{}", crate::type_ident(name)));
            }
            match &definition.kind {
                TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => format!(
                    "{module}::{}",
                    crate::type_ident(resolve.types[*resource].name.as_deref().unwrap_or_default())
                ),
                TypeDefKind::Handle(wit_parser::Handle::Borrow(_)) => {
                    return Err(error(
                        "a borrowed resource in a signature is not expressed by the macro; a resource is handed out owned and borrowed only as a method's self",
                    ));
                }
                TypeDefKind::Option(inner) => {
                    format!(
                        "::core::option::Option<{}>",
                        rust_type(resolve, *inner, module)?
                    )
                }
                TypeDefKind::List(inner) => format!(
                    "__tembra_alloc::vec::Vec<{}>",
                    rust_type(resolve, *inner, module)?
                ),
                TypeDefKind::Result(result) => {
                    let side = |ty: Option<wit_parser::Type>| match ty {
                        Some(ty) => rust_type(resolve, ty, module),
                        None => Ok("()".to_owned()),
                    };
                    format!(
                        "::core::result::Result<{}, {}>",
                        side(result.ok)?,
                        side(result.err)?
                    )
                }
                TypeDefKind::Tuple(tuple) => {
                    let types = tuple
                        .types
                        .iter()
                        .map(|ty| rust_type(resolve, *ty, module))
                        .collect::<syn::Result<Vec<_>>>()?;
                    format!("({},)", types.join(", "))
                }
                TypeDefKind::Type(inner) => rust_type(resolve, *inner, module)?,
                _ => {
                    return Err(error(
                        "this type has no spelling the macro knows; use records, enums, variants, resources, scalars, strings, lists, options, results and tuples",
                    ));
                }
            }
        }
    })
}

pub(crate) fn error(message: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(Span::call_site(), message.to_string())
}

/// A struct with named fields becomes a `record`; the module types its fields name are the
/// caller's to give it.
pub(crate) fn record(item: &ItemStruct) -> syn::Result<Shape> {
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new(item.span(), "a record needs named fields"));
    };
    let mut wit = String::new();
    for doc in docs(&item.attrs)? {
        wit.push_str(&format!("  ///{doc}\n"));
    }
    wit.push_str(&format!("  record {} {{\n", escaped(&kebab(&item.ident))));
    let mut named = Vec::new();
    for field in &fields.named {
        let Some(ident) = &field.ident else {
            return Err(syn::Error::new(field.span(), "a record field needs a name"));
        };
        wit.push_str(&format!(
            "    {}: {},\n",
            escaped(&kebab(ident)),
            wit_type(&field.ty)?
        ));
        named.push((ident.clone(), field.ty.clone()));
    }
    wit.push_str("  }\n");
    Ok(Shape {
        ident: item.ident.clone(),
        wit,
        references: Vec::new(),
        kind: ShapeKind::Record(named),
        scope: 0,
        carried: Default::default(),
    })
}

/// An enum becomes an `enum` when every variant is empty, otherwise a `variant` with one payload
/// per case; the module types the payloads name are the caller's to give it.
pub(crate) fn enumeration(item: &ItemEnum) -> syn::Result<Shape> {
    let plain = item
        .variants
        .iter()
        .all(|variant| matches!(variant.fields, Fields::Unit));
    let mut wit = String::new();
    for doc in docs(&item.attrs)? {
        wit.push_str(&format!("  ///{doc}\n"));
    }
    let keyword = if plain { "enum" } else { "variant" };
    wit.push_str(&format!(
        "  {keyword} {} {{\n",
        escaped(&kebab(&item.ident))
    ));
    let mut cases = Vec::new();
    for variant in &item.variants {
        let case = escaped(&kebab(&variant.ident));
        match &variant.fields {
            Fields::Unit => {
                wit.push_str(&format!("    {case},\n"));
                cases.push((variant.ident.clone(), None));
            }
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                wit.push_str(&format!(
                    "    {case}({}),\n",
                    wit_type(&fields.unnamed[0].ty)?
                ));
                cases.push((variant.ident.clone(), Some(fields.unnamed[0].ty.clone())));
            }
            _ => {
                return Err(syn::Error::new(
                    variant.span(),
                    "a variant carries at most one unnamed payload",
                ));
            }
        }
    }
    wit.push_str("  }\n");
    let kind = if plain {
        ShapeKind::Enum(cases.into_iter().map(|(case, _)| case).collect())
    } else {
        ShapeKind::Variant(cases)
    };
    Ok(Shape {
        ident: item.ident.clone(),
        wit,
        references: Vec::new(),
        kind,
        scope: 0,
        carried: Default::default(),
    })
}

/// Rust types with a WIT spelling of their own; any other name is a type of the module, or one a
/// consumed interface's module names.
pub(crate) const BUILTIN: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64", "bool", "char", "String",
    "Vec", "Option", "Result",
];

/// Whether a Rust type is `()`.
fn unit(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(tuple) if tuple.elems.is_empty())
}

/// Translate an owned Rust type into its WIT spelling.
pub(crate) fn wit_type(ty: &Type) -> syn::Result<String> {
    match ty {
        Type::Path(path) => {
            let segment = path
                .path
                .segments
                .last()
                .ok_or_else(|| syn::Error::new(ty.span(), "empty type path"))?;
            let name = segment.ident.to_string();
            // The type parameters of `Vec`, `Option` and `Result`, exactly `count` of them.
            let args = |count: usize| -> syn::Result<Vec<&Type>> {
                let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                    return Err(syn::Error::new(
                        segment.span(),
                        "type parameters are required",
                    ));
                };
                let types = args
                    .args
                    .iter()
                    .map(|arg| match arg {
                        syn::GenericArgument::Type(ty) => Ok(ty),
                        other => Err(syn::Error::new(other.span(), "unsupported type parameter")),
                    })
                    .collect::<syn::Result<Vec<_>>>()?;
                if types.len() != count {
                    return Err(syn::Error::new(
                        segment.span(),
                        format!(
                            "the macro reads `{name}` by its name as {}, so another `{name}`, such as a module's own alias, cannot stand for it here",
                            if count == 1 {
                                format!("`{name}<T>`")
                            } else {
                                format!("`{name}<T, E>`")
                            }
                        ),
                    ));
                }
                Ok(types)
            };
            Ok(match name.as_str() {
                "i8" => "s8".into(),
                "i16" => "s16".into(),
                "i32" => "s32".into(),
                "i64" => "s64".into(),
                "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" => name,
                "String" => "string".into(),
                "Vec" => format!("list<{}>", wit_type(args(1)?[0])?),
                "Option" => {
                    let inner = args(1)?[0];
                    if unit(inner) {
                        return Err(syn::Error::new(
                            ty.span(),
                            "`Option<()>` has no WIT equivalent, WIT has no empty value for an option to hold: use `bool`",
                        ));
                    }
                    // What `unsupported` finds in a WIT type, as a Rust type spells it.
                    let nested = match inner {
                        Type::Path(path) => path
                            .path
                            .segments
                            .last()
                            .is_some_and(|segment| segment.ident == "Option"),
                        _ => false,
                    };
                    if nested {
                        return Err(syn::Error::new(
                            ty.span(),
                            "`Option<Option<T>>` is an option of an option, which the runtime refuses in a signature: use an enum of the module with a case for each",
                        ));
                    }
                    format!("option<{}>", wit_type(inner)?)
                }
                // WIT leaves out a side that carries nothing: `result<t>` is `Result<T, ()>`,
                // `result<_, e>` is `Result<(), E>`, and `result` is `Result<(), ()>`.
                "Result" => {
                    let sides = args(2)?;
                    let (ok, err) = (sides[0], sides[1]);
                    match (unit(ok), unit(err)) {
                        (true, true) => "result".into(),
                        (true, false) => format!("result<_, {}>", wit_type(err)?),
                        (false, true) => format!("result<{}>", wit_type(ok)?),
                        (false, false) => format!("result<{}, {}>", wit_type(ok)?, wit_type(err)?),
                    }
                }
                // A type of the module, by its WIT name.
                _ => escaped(&kebab(&segment.ident)),
            })
        }
        Type::Tuple(tuple) if tuple.elems.is_empty() => Err(syn::Error::new(
            ty.span(),
            "`()` has no WIT equivalent as a value, WIT has no empty tuple; it reads as nothing only as a function's result or a side of a `Result`",
        )),
        Type::Tuple(tuple) => Ok(format!(
            "tuple<{}>",
            tuple
                .elems
                .iter()
                .map(wit_type)
                .collect::<syn::Result<Vec<_>>>()?
                .join(", ")
        )),
        Type::Reference(_) => Err(syn::Error::new(
            ty.span(),
            "WIT boundaries pass owned values; use String, Vec<T>, ... without &",
        )),
        other => Err(syn::Error::new(
            other.span(),
            "this type has no WIT equivalent",
        )),
    }
}

/// The `///` lines of an item, carried into the WIT: one per line of its documentation, so the
/// text of a block comment or of a `#[doc]` string over several lines stays a comment in the WIT,
/// each line after its own `///`. A block comment's opening and closing lines, when blank, are
/// left out. A character the WIT lexer refuses anywhere, a control character or a bidirectional
/// override, is an error on the attribute.
pub(crate) fn docs(attrs: &[Attribute]) -> syn::Result<Vec<String>> {
    let mut lines = Vec::new();
    for attr in attrs {
        let Meta::NameValue(meta) = &attr.meta else {
            continue;
        };
        if !meta.path.is_ident("doc") {
            continue;
        }
        let Expr::Lit(literal) = &meta.value else {
            continue;
        };
        let Lit::Str(text) = &literal.lit else {
            continue;
        };
        let text = text.value();
        if let Some(refused) = text.chars().find(|character| refused_in_wit(*character)) {
            return Err(syn::Error::new(
                attr.span(),
                format!(
                    "this documentation holds U+{:04X}, a character WIT refuses; the macro carries the documentation into the WIT",
                    u32::from(refused)
                ),
            ));
        }
        let text = text.replace("\r\n", "\n");
        let mut split: Vec<&str> = text.split(['\n', '\r']).collect();
        while split.len() > 1 && split.last().is_some_and(|line| line.trim().is_empty()) {
            split.pop();
        }
        while split.len() > 1 && split.first().is_some_and(|line| line.trim().is_empty()) {
            split.remove(0);
        }
        lines.extend(split.into_iter().map(str::to_owned));
    }
    Ok(lines)
}

/// Whether the WIT lexer refuses a character anywhere in a file, comments included: control
/// characters other than tabs and line breaks, bidirectional overrides and the deprecated
/// codepoints it lists.
fn refused_in_wit(character: char) -> bool {
    matches!(
        character,
        '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{149}'
            | '\u{673}'
            | '\u{f77}'
            | '\u{f79}'
            | '\u{17a3}'
            | '\u{17a4}'
            | '\u{17b4}'
            | '\u{17b5}'
    ) || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}
