//! The WIT text derived from the module items, and the consumed packages it refers to.
use crate::resource_params::Foreign;
use crate::{Composition, Declared, Function, Handles, Service, Shape, ShapeKind, WitResource};
use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro2::Span;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, Fields, Ident, ItemEnum, ItemStruct, Lit, Meta, Type, ext::IdentExt,
    spanned::Spanned,
};
use wit_parser::{Resolve, TypeDefKind};

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
/// the package, and what each derived interface `use`s from the imported interfaces, by its name.
pub(crate) struct Items<'a> {
    pub shapes: &'a [Shape],
    pub placement: &'a BTreeMap<String, Ident>,
    pub services: &'a [Service],
    pub resources: &'a [WitResource],
    pub declared: &'a [Declared],
    pub foreign: &'a BTreeMap<String, Vec<Foreign>>,
}

/// The names of the shapes a derived hook reaches through its input, directly or through the
/// fields of another shape: they are defined in the first derived hook's interface.
pub(crate) fn hook_shapes(shapes: &[Shape], declared: &[Declared]) -> BTreeSet<String> {
    let by_name: BTreeMap<String, &Shape> = shapes
        .iter()
        .map(|shape| (shape.ident.to_string(), shape))
        .collect();
    let mut pending: Vec<Ident> = Vec::new();
    for (input, _) in declared
        .iter()
        .filter_map(|declaration| declaration.input.as_ref())
    {
        references(input, &mut pending);
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

/// The module types a derived service's interface mentions in its own text: its functions, and
/// the constructors, methods and static functions of the resources it owns.
fn service_mentions(service: &Service, resources: &[WitResource]) -> Vec<Ident> {
    let mut mentioned = Vec::new();
    let owned: Vec<&WitResource> = resources
        .iter()
        .filter(|resource| resource.service.as_ref() == Some(&service.ident))
        .collect();
    let functions = service.functions.iter().chain(
        owned
            .iter()
            .flat_map(|resource| resource.methods.iter().chain(&resource.statics)),
    );
    for function in functions {
        for (_, ty, _) in &function.params {
            references(ty, &mut mentioned);
        }
        if let Some((ty, _)) = &function.result {
            references(ty, &mut mentioned);
        }
    }
    for constructor in owned
        .iter()
        .filter_map(|resource| resource.constructor.as_ref())
    {
        for (_, ty, _) in &constructor.params {
            references(ty, &mut mentioned);
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
    for name in hook_shapes(shapes, declared) {
        let hook = first_hook.expect("a shape a derived hook reaches");
        placed.insert(name, hook.clone());
    }
    let derived: Vec<&Service> = services
        .iter()
        .filter(|service| service.implements.is_none())
        .collect();
    for service in &derived {
        let mut pending = service_mentions(service, resources);
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
/// which other interface, and the struct it is derived from, for the errors.
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
/// service interface, derived or implemented. Interfaces that `use` each other, which WIT
/// refuses, are an error naming them.
pub(crate) fn render<'a>(
    package: &str,
    world: &str,
    items: Items<'_>,
    hooks: impl Iterator<Item = &'a String>,
    imports: &[String],
    views: &[crate::keyed::View],
    kernel: Kernel,
) -> syn::Result<String> {
    let Items {
        shapes,
        placement,
        services,
        resources,
        declared,
        foreign,
    } = items;
    let derived_hooks: Vec<&Declared> = declared
        .iter()
        .filter(|declaration| declaration.input.is_some())
        .collect();
    let derived: Vec<&Service> = services
        .iter()
        .filter(|service| service.implements.is_none())
        .collect();
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
                references(input, &mut mentioned);
                mentioned
            }
            None => {
                let service = derived
                    .iter()
                    .find(|service| &service.ident == *ident)
                    .expect("a derived service");
                service_mentions(service, resources)
            }
        };
        for shape in &defined {
            mentioned.extend(shape.references.iter().cloned());
        }
        // Each name another interface defines is borrowed from it with `use`, the shapes then
        // the resources, one line per interface in package order.
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
        for resource in resources
            .iter()
            .filter(|resource| mentioned.contains(&resource.ident))
        {
            if owner_of(resource) != *name {
                borrowed.push((owner_of(resource), &resource.ident));
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
            for (_, handle) in &used.resources {
                namespace.claim(handle, format!("`{handle}`, used from {},", used.identity))?;
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
    if let Some(cycle) = use_cycle(&interfaces) {
        let first = &interfaces[cycle[0]];
        let steps: Vec<String> = cycle
            .iter()
            .zip(cycle.iter().cycle().skip(1))
            .map(|(&from, &to)| {
                let (from, to) = (&interfaces[from], &interfaces[to]);
                let names = to_names(from, &to.name);
                format!("`{}` uses {} of `{}`", from.name, names, to.name)
            })
            .collect();
        return Err(syn::Error::new(
            first.ident.span(),
            format!(
                "{}: WIT refuses interfaces that depend on themselves through `use`; let one service's interface define or own what the others use",
                steps.join(", ")
            ),
        ));
    }
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
    Ok(wit)
}

/// The names an interface `use`s from another one, for the errors: `{a, b}`.
fn to_names(interface: &Interface<'_>, from: &str) -> String {
    let names = interface
        .uses
        .iter()
        .find(|(other, _)| other == from)
        .map(|(_, names)| names.join(", "))
        .unwrap_or_default();
    format!("`{{{names}}}`")
}

/// A cycle of interfaces that `use` one another, as indexes in `interfaces`, if there is one.
fn use_cycle(interfaces: &[Interface<'_>]) -> Option<Vec<usize>> {
    let edges: Vec<Vec<usize>> = interfaces
        .iter()
        .map(|interface| {
            interface
                .uses
                .iter()
                .filter_map(|(from, _)| interfaces.iter().position(|other| other.name == *from))
                .collect()
        })
        .collect();
    // Depth first, with the path walked so far: an edge back into the path closes a cycle.
    fn visit(
        node: usize,
        edges: &[Vec<usize>],
        path: &mut Vec<usize>,
        done: &mut BTreeSet<usize>,
    ) -> Option<Vec<usize>> {
        if let Some(start) = path.iter().position(|&walked| walked == node) {
            return Some(path[start..].to_vec());
        }
        if !done.insert(node) {
            return None;
        }
        path.push(node);
        for &next in &edges[node] {
            if let Some(cycle) = visit(next, edges, path, done) {
                return Some(cycle);
            }
        }
        path.pop();
        None
    }
    let mut done = BTreeSet::new();
    (0..interfaces.len()).find_map(|node| visit(node, &edges, &mut Vec::new(), &mut done))
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
const GUEST_CONTEXT: &str = "tembra:guest/context@1.0.0";

/// The kernel's `tembra:guest` package, as `tembra-contracts` bundles it for every host.
const GUEST: &str = tembra_contracts::guest::SOURCE;

/// The kernel's `tembra:guest` package in nested form, appended to the inline WIT so the bindings
/// of a state, a resource or the context resolve it.
pub(crate) fn guest_package() -> syn::Result<String> {
    nested(GUEST)
        .ok_or_else(|| error("the bundled tembra:guest package has no `package ..;` declaration"))
}

/// Whether the inline WIT needs the kernel's bundled package nested in it: not when the consumed
/// directory resolves that package already, as a contract `cargo tembra build` wrote does from its
/// `deps/`, since the bindings would read it twice. Such a copy must define what the bundled
/// package defines, its comments aside; another definition is an error naming its file.
pub(crate) fn guest_package_needed(dir: Option<&Path>) -> syn::Result<bool> {
    let Some(dir) = dir else {
        return Ok(true);
    };
    let consumed = resolve(dir)?;
    let mut bundled = Resolve::default();
    let bundled_id = bundled
        .push_str("guest.wit", GUEST)
        .map_err(|cause| error(format!("the bundled tembra:guest package: {cause:#}")))?;
    let name = &bundled.packages[bundled_id].name;
    let Some(&consumed_id) = consumed.package_names.get(name) else {
        return Ok(true);
    };
    if definitions(&consumed, consumed_id) != definitions(&bundled, bundled_id) {
        let place = consumed.packages[consumed_id]
            .interfaces
            .values()
            .next()
            .map(|id| {
                consumed
                    .source_map
                    .render_location(consumed.interfaces[*id].span)
            })
            .unwrap_or_else(|| dir.display().to_string());
        return Err(error(format!(
            "{place}: this copy of {name}, the kernel's package, defines it otherwise than the one bundled with this macro; build the consumed contract with the `cargo tembra` of the same version"
        )));
    }
    Ok(false)
}

/// What a package defines, as text without its documentation: each interface, in name order, with
/// its types and functions, a type by its structure and a named type by its place. Two copies of
/// one package that differ only in their comments read the same.
pub(crate) fn definitions(resolve: &Resolve, package: wit_parser::PackageId) -> String {
    let mut text = String::new();
    let mut interfaces: Vec<(&String, &wit_parser::InterfaceId)> =
        resolve.packages[package].interfaces.iter().collect();
    interfaces.sort();
    for (name, id) in interfaces {
        let interface = &resolve.interfaces[*id];
        text.push_str(&format!("interface {name}\n"));
        for (name, ty) in &interface.types {
            text.push_str(&format!("  type {name} = {}\n", definition(resolve, *ty)));
        }
        for (name, function) in &interface.functions {
            let params: Vec<String> = function
                .params
                .iter()
                .map(|param| format!("{}: {}", param.name, reference(resolve, param.ty)))
                .collect();
            let result = function
                .result
                .map(|ty| reference(resolve, ty))
                .unwrap_or_default();
            let asynchronous = if function.kind.is_async() {
                "async "
            } else {
                ""
            };
            text.push_str(&format!(
                "  {name}: {asynchronous}func({}) -> {result}\n",
                params.join(", ")
            ));
        }
    }
    let mut worlds: Vec<&String> = resolve.packages[package].worlds.keys().collect();
    worlds.sort();
    for name in worlds {
        text.push_str(&format!("world {name}\n"));
    }
    text
}

/// The structure of a type definition, its parts by `reference`.
fn definition(resolve: &Resolve, id: wit_parser::TypeId) -> String {
    let part = |ty: &wit_parser::Type| reference(resolve, *ty);
    let side = |ty: &Option<wit_parser::Type>| ty.as_ref().map_or_else(|| "_".to_owned(), part);
    let joined = |parts: Vec<String>| parts.join(", ");
    match &resolve.types[id].kind {
        TypeDefKind::Record(record) => format!(
            "record {{{}}}",
            joined(
                record
                    .fields
                    .iter()
                    .map(|field| format!("{}: {}", field.name, part(&field.ty)))
                    .collect()
            )
        ),
        TypeDefKind::Variant(variant) => format!(
            "variant {{{}}}",
            joined(
                variant
                    .cases
                    .iter()
                    .map(|case| format!("{}({})", case.name, side(&case.ty)))
                    .collect()
            )
        ),
        TypeDefKind::Enum(cases) => format!(
            "enum {{{}}}",
            joined(cases.cases.iter().map(|case| case.name.clone()).collect())
        ),
        TypeDefKind::Flags(flags) => format!(
            "flags {{{}}}",
            joined(flags.flags.iter().map(|flag| flag.name.clone()).collect())
        ),
        TypeDefKind::Tuple(tuple) => {
            format!("tuple<{}>", joined(tuple.types.iter().map(part).collect()))
        }
        TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => {
            format!(
                "own<{}>",
                reference(resolve, wit_parser::Type::Id(*resource))
            )
        }
        TypeDefKind::Handle(wit_parser::Handle::Borrow(resource)) => {
            format!(
                "borrow<{}>",
                reference(resolve, wit_parser::Type::Id(*resource))
            )
        }
        TypeDefKind::Option(ty) => format!("option<{}>", part(ty)),
        TypeDefKind::Result(result) => {
            format!("result<{}, {}>", side(&result.ok), side(&result.err))
        }
        TypeDefKind::List(ty) => format!("list<{}>", part(ty)),
        TypeDefKind::FixedLengthList(ty, length) => format!("list<{}, {length}>", part(ty)),
        TypeDefKind::Map(key, value) => format!("map<{}, {}>", part(key), part(value)),
        TypeDefKind::Future(ty) => format!("future<{}>", side(ty)),
        TypeDefKind::Stream(ty) => format!("stream<{}>", side(ty)),
        TypeDefKind::Type(ty) => part(ty),
        TypeDefKind::Resource => "resource".to_owned(),
        TypeDefKind::Unknown => "unknown".to_owned(),
    }
}

/// A type as a part of another: a named type by its interface and name, an anonymous one by its
/// structure, a primitive by its name.
fn reference(resolve: &Resolve, ty: wit_parser::Type) -> String {
    let wit_parser::Type::Id(id) = ty else {
        return format!("{ty:?}");
    };
    let definition_of = &resolve.types[id];
    match (&definition_of.name, &definition_of.owner) {
        (Some(name), wit_parser::TypeOwner::Interface(owner)) => {
            let interface = resolve
                .id_of(*owner)
                .or_else(|| resolve.interfaces[*owner].name.clone())
                .unwrap_or_default();
            format!("{interface}.{name}")
        }
        (Some(name), _) => name.clone(),
        (None, _) => definition(resolve, id),
    }
}

/// A WIT package file in nested form: its declaration, a line `package ns:name@version;`, opens
/// a block holding the rest of the file. The file's lines may end with `\n` or `\r\n`, as a
/// checkout writes them; the nested form ends its lines with `\n`.
pub(crate) fn nested(file: &str) -> Option<String> {
    let mut lines = file.lines();
    let mut nested = String::new();
    loop {
        let line = lines.next()?;
        let declared = line
            .trim()
            .strip_prefix("package ")
            .and_then(|rest| rest.strip_suffix(';'));
        if let Some(name) = declared {
            nested.push_str(&format!("package {} {{\n", name.trim()));
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

/// Read the WIT packages of a directory.
pub(crate) fn resolve(dir: &Path) -> syn::Result<Resolve> {
    let mut resolve = Resolve::default();
    resolve.push_dir(dir).map_err(|cause| {
        error(format!(
            "cannot read the WIT packages in {}: {cause:#}",
            dir.display()
        ))
    })?;
    Ok(resolve)
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
    dir: &Path,
    identities: impl Iterator<Item = &'a String>,
) -> syn::Result<BTreeMap<String, Consumed>> {
    use wit_parser::FunctionKind;
    let resolve = resolve(dir)?;
    identities
        .map(|identity| {
            let interface = interface(&resolve, dir, identity)?;
            let [namespace, package, name] = crate::hook_path(identity);
            let import = format!("self::{namespace}::{package}::{name}");
            let export = format!("exports::{namespace}::{package}::{name}");
            let spell = |ty: wit_parser::Type| -> syn::Result<Spelled> {
                Ok(Spelled {
                    import: rust_type(&resolve, ty, &import)?,
                    export: rust_type(&resolve, ty, &export)?,
                    handles: handles(&resolve, ty),
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
    /// The records and variants that cannot derive `serde`'s traits: those holding a consumed
    /// package's flags type, a resource handle, a stream or a future, directly or through other
    /// types, which the bindings give no `serde` implementation. They are named as the
    /// `additional_derives_ignore` option of `wit-bindgen` compares them, by `ignored_name`.
    pub unserializable: Vec<String>,
    /// How the bindings generate each record, variant and enum of an interface, by the type's full
    /// name as the selectors of `additional_type_attributes` write it,
    /// `ns:name/interface@version/type`.
    pub types: BTreeMap<String, Generated>,
}

/// How `wit-bindgen` 0.63 generates a record, a variant or an enum, as `wit-bindgen-core` records
/// it to choose what it derives and implements: whether the type holds a string or a list, whether
/// it holds a resource handle, and whether a function of a world returns it as the error of its
/// result.
#[derive(Clone, Copy, Default)]
pub(crate) struct Generated {
    pub list: bool,
    pub resource: bool,
    pub error: bool,
}

/// Read the WIT the bindings read, `inline` with the consumed packages of `dir`, for what they make
/// of its types; a WIT the bindings would refuse yields nothing, and the bindings report it.
pub(crate) fn bindings(dir: Option<&Path>, inline: &str) -> Bindings {
    let mut resolve = Resolve::default();
    if let Some(dir) = dir
        && resolve.push_dir(dir).is_err()
    {
        return Bindings::default();
    }
    let Ok(own) = resolve.push_str("orb.wit", inline) else {
        return Bindings::default();
    };
    let unserializable: BTreeSet<String> = resolve
        .types
        .iter()
        .filter(|(_, definition)| {
            matches!(
                definition.kind,
                TypeDefKind::Record(_) | TypeDefKind::Variant(_)
            )
        })
        .filter(|(id, _)| {
            holds_unserializable(
                &resolve,
                wit_parser::Type::Id(*id),
                &mut BTreeSet::new(),
                own,
            )
        })
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
    let mut types: BTreeMap<String, Generated> = resolve
        .types
        .iter()
        .filter(|(_, definition)| {
            matches!(
                definition.kind,
                TypeDefKind::Record(_) | TypeDefKind::Variant(_) | TypeDefKind::Enum(_)
            )
        })
        .filter_map(|(id, _)| {
            let name = full_name(id)?;
            let (list, resource) = holds(&resolve, wit_parser::Type::Id(id));
            let generated = Generated {
                list,
                resource,
                error: false,
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
        types,
    }
}

/// The name by which the `additional_derives_ignore` option of `wit-bindgen` compares a type of
/// the given WIT name: the kebab-case form of the Rust name the bindings give it.
pub(crate) fn ignored_name(name: &str) -> String {
    name.to_upper_camel_case().to_kebab_case()
}

/// Whether a WIT type holds, anywhere, a value the bindings give no `serde` implementation: a
/// flags type of another package than `own`, the Orb's, whose flags types are the author's
/// structs, a resource handle, a stream, a future, an error context, or an array longer than
/// `serde`'s.
fn holds_unserializable(
    resolve: &Resolve,
    ty: wit_parser::Type,
    seen: &mut BTreeSet<wit_parser::TypeId>,
    own: wit_parser::PackageId,
) -> bool {
    let id = match ty {
        wit_parser::Type::Id(id) => id,
        wit_parser::Type::ErrorContext => return true,
        _ => return false,
    };
    // A type already looked at answered no, or the walk would have stopped there.
    if !seen.insert(id) {
        return false;
    }
    let mut holds = |ty: wit_parser::Type| holds_unserializable(resolve, ty, seen, own);
    match &resolve.types[id].kind {
        TypeDefKind::Flags(_) => !matches!(resolve.types[id].owner,
            wit_parser::TypeOwner::Interface(owner) if resolve.interfaces[owner].package == Some(own)),
        TypeDefKind::Handle(_)
        | TypeDefKind::Resource
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_) => true,
        TypeDefKind::Record(record) => record.fields.iter().any(|field| holds(field.ty)),
        TypeDefKind::Variant(variant) => variant.cases.iter().filter_map(|case| case.ty).any(holds),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().copied().any(holds),
        TypeDefKind::Option(inner) | TypeDefKind::List(inner) | TypeDefKind::Type(inner) => {
            holds(*inner)
        }
        TypeDefKind::FixedLengthList(inner, length) => *length > 32 || holds(*inner),
        TypeDefKind::Map(key, value) => holds(*key) || holds(*value),
        TypeDefKind::Result(result) => result.ok.into_iter().chain(result.err).any(holds),
        TypeDefKind::Enum(_) | TypeDefKind::Unknown => false,
    }
}

/// The callback of every hook: the single function of its interface in the consumed packages.
pub(crate) fn callbacks<'a>(
    dir: &Path,
    hooks: impl Iterator<Item = &'a String>,
) -> syn::Result<BTreeMap<String, Callback>> {
    let resolve = resolve(dir)?;
    hooks
        .map(|hook| {
            let interface = interface(&resolve, dir, hook)?;
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
                    import: rust_type(&resolve, ty, &import)?,
                    export: rust_type(&resolve, ty, &export)?,
                    handles: handles(&resolve, ty),
                })
            };
            Ok((
                hook.clone(),
                Callback {
                    name: function.name.clone(),
                    series: function.result.is_none(),
                    waterfall: function
                        .result
                        .is_some_and(|result| same_type(&resolve, result, param.ty)),
                    bail: function.result.is_some_and(|ty| optional(&resolve, ty)),
                    input: spell(param.ty)?,
                    result: function.result.map(spell).transpose()?,
                    passed: passed(&resolve, param.ty),
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

fn error(message: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(Span::call_site(), message.to_string())
}

/// A struct with named fields becomes a `record`.
pub(crate) fn record(item: &ItemStruct) -> syn::Result<Shape> {
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new(item.span(), "a record needs named fields"));
    };
    let mut wit = String::new();
    for doc in docs(&item.attrs)? {
        wit.push_str(&format!("  ///{doc}\n"));
    }
    wit.push_str(&format!("  record {} {{\n", escaped(&kebab(&item.ident))));
    let mut references = Vec::new();
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
        self::references(&field.ty, &mut references);
        named.push((ident.clone(), field.ty.clone()));
    }
    wit.push_str("  }\n");
    Ok(Shape {
        ident: item.ident.clone(),
        wit,
        references,
        kind: ShapeKind::Record(named),
        scope: 0,
        carried: Default::default(),
    })
}

/// An enum becomes an `enum` when every variant is empty, otherwise a `variant` with one payload
/// per case.
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
    let mut references = Vec::new();
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
                self::references(&fields.unnamed[0].ty, &mut references);
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
        references,
        kind,
        scope: 0,
        carried: Default::default(),
    })
}

/// Rust types with a WIT spelling of their own; any other name is a type of the module.
const BUILTIN: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64", "bool", "char", "String",
    "Vec", "Option", "Result",
];

/// Collect the module types a Rust type mentions, looking through generics and tuples.
pub(crate) fn references(ty: &Type, out: &mut Vec<Ident>) {
    match ty {
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return;
            };
            if !BUILTIN.contains(&segment.ident.to_string().as_str()) {
                out.push(segment.ident.clone());
            }
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                for arg in &args.args {
                    if let syn::GenericArgument::Type(ty) = arg {
                        references(ty, out);
                    }
                }
            }
        }
        Type::Tuple(tuple) => {
            for element in &tuple.elems {
                references(element, out);
            }
        }
        _ => {}
    }
}

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
