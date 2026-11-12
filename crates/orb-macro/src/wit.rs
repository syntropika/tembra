//! The WIT text derived from the module items, and the consumed packages it refers to.
use crate::{Composition, Declared, Function, Service, Shape, ShapeKind, WitResource};
use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro2::Span;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, Fields, Ident, ItemEnum, ItemStruct, Lit, Meta, Type, spanned::Spanned,
};
use wit_parser::{Resolve, TypeDefKind};

/// WIT names are kebab-case; Rust identifiers map to them one by one.
pub(crate) fn kebab(ident: &Ident) -> String {
    ident.to_string().to_kebab_case()
}

/// The items of the module that enter the WIT: its exported types, its services, its resources
/// and its declared hooks, of which the derived ones are interfaces of the package.
pub(crate) struct Items<'a> {
    pub shapes: &'a [Shape],
    pub services: &'a [Service],
    pub resources: &'a [WitResource],
    pub declared: &'a [Declared],
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

/// Which kernel interfaces the world uses.
pub(crate) struct Kernel {
    pub state: bool,
    pub resources: bool,
    pub context: bool,
}

/// Write the package: one interface per derived hook, with its callback `apply` and, in the first
/// of them, the types the hooks reach; one interface per derived service, the other types in the
/// first of them and borrowed by the others with `use`, each WIT resource in the interface of the
/// service that owns it; then a world using the kernel's interfaces for its states, resources and
/// context, importing every consumed interface, importing and exporting every hook, consumed or
/// derived, and exporting every service interface, derived or implemented.
pub(crate) fn render<'a>(
    package: &str,
    world: &str,
    items: Items<'_>,
    hooks: impl Iterator<Item = &'a String>,
    imports: &[String],
    kernel: Kernel,
) -> String {
    let Items {
        shapes,
        services,
        resources,
        declared,
    } = items;
    let mut wit = format!("package {package};\n");
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
    let in_hooks = hook_shapes(shapes, declared);
    let first_hook = derived_hooks.first().map(|hook| kebab(&hook.ident));
    let first_service = derived.first().map(|service| kebab(&service.ident));
    // An interface borrows with `use` the shapes it mentions and another interface defines: the
    // hooks' shapes from the first derived hook, the others from the first derived service.
    let borrow = |wit: &mut String, mentioned: &[Ident], from: Option<&str>, hooks: bool| {
        let Some(from) = from else {
            return;
        };
        let names: Vec<String> = shapes
            .iter()
            .filter(|shape| {
                mentioned.contains(&shape.ident)
                    && in_hooks.contains(&shape.ident.to_string()) == hooks
            })
            .map(|shape| kebab(&shape.ident))
            .collect();
        if !names.is_empty() {
            wit.push_str(&format!("  use {from}.{{{}}};\n", names.join(", ")));
        }
    };
    for (index, hook) in derived_hooks.iter().enumerate() {
        let (input, spelling) = hook.input.as_ref().expect("a derived hook");
        wit.push('\n');
        for doc in &hook.docs {
            wit.push_str(&format!("///{doc}\n"));
        }
        wit.push_str(&format!("interface {} {{\n", kebab(&hook.ident)));
        if index == 0 {
            for shape in shapes
                .iter()
                .filter(|shape| in_hooks.contains(&shape.ident.to_string()))
            {
                wit.push_str(&shape.wit);
            }
        } else {
            let mut mentioned = Vec::new();
            references(input, &mut mentioned);
            borrow(&mut wit, &mentioned, first_hook.as_deref(), true);
        }
        let result = match hook.composition {
            Composition::Series => String::new(),
            Composition::Waterfall => format!(" -> {spelling}"),
            Composition::Bail => format!(" -> option<{spelling}>"),
        };
        wit.push_str(&format!("  apply: func(input: {spelling}){result};\n}}\n"));
    }
    for (index, service) in derived.iter().enumerate() {
        wit.push('\n');
        for doc in &service.docs {
            wit.push_str(&format!("///{doc}\n"));
        }
        let name = kebab(&service.ident);
        wit.push_str(&format!("interface {name} {{\n"));
        // The interface mentions what its functions name, and what the constructors, methods
        // and static functions of its own resources name.
        let mut mentioned = Vec::new();
        let owned = || {
            resources
                .iter()
                .filter(|resource| owner_of(resource) == name)
        };
        let functions = service
            .functions
            .iter()
            .chain(owned().flat_map(|resource| resource.methods.iter().chain(&resource.statics)));
        for function in functions {
            for (_, ty, _) in &function.params {
                references(ty, &mut mentioned);
            }
            if let Some((ty, _)) = &function.result {
                references(ty, &mut mentioned);
            }
        }
        for constructor in owned().filter_map(|resource| resource.constructor.as_ref()) {
            for (_, ty, _) in &constructor.params {
                references(ty, &mut mentioned);
            }
        }
        if index == 0 {
            // The shapes defined here may name a hook's shape in their fields.
            for shape in shapes
                .iter()
                .filter(|shape| !in_hooks.contains(&shape.ident.to_string()))
            {
                mentioned.extend(shape.references.iter().cloned());
            }
            borrow(&mut wit, &mentioned, first_hook.as_deref(), true);
            for shape in shapes
                .iter()
                .filter(|shape| !in_hooks.contains(&shape.ident.to_string()))
            {
                wit.push_str(&shape.wit);
            }
        } else {
            borrow(&mut wit, &mentioned, first_service.as_deref(), false);
            borrow(&mut wit, &mentioned, first_hook.as_deref(), true);
        }
        // A resource of another service that this one hands out is borrowed with `use`.
        let mut borrowed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for resource in resources {
            if owner_of(resource) != name && mentioned.contains(&resource.ident) {
                borrowed
                    .entry(owner_of(resource))
                    .or_default()
                    .push(kebab(&resource.ident));
            }
        }
        for (owner, names) in borrowed {
            wit.push_str(&format!("  use {owner}.{{{}}};\n", names.join(", ")));
        }
        for resource in resources
            .iter()
            .filter(|resource| owner_of(resource) == name)
        {
            wit.push_str(&resource_text(resource));
        }
        for function in &service.functions {
            wit.push_str(&function_text(function, "  ", ""));
        }
        wit.push_str("}\n");
    }
    wit.push_str(&format!("\nworld {world} {{\n"));
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
    for hook in hooks {
        wit.push_str(&format!("  import {hook};\n  export {hook};\n"));
    }
    for hook in &derived_hooks {
        let name = kebab(&hook.ident);
        wit.push_str(&format!("  import {name};\n  export {name};\n"));
    }
    for service in services {
        match &service.implements {
            Some(identity) => wit.push_str(&format!("  export {identity};\n")),
            None => wit.push_str(&format!("  export {};\n", kebab(&service.ident))),
        }
    }
    wit.push_str("}\n");
    wit
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
        .map(|(ident, _, wit)| format!("{}: {wit}", kebab(ident)))
        .collect::<Vec<_>>()
        .join(", ");
    let result = function
        .result
        .as_ref()
        .map(|(_, wit)| format!(" -> {wit}"))
        .unwrap_or_default();
    text.push_str(&format!(
        "{indent}{}: {keyword}func({params}){result};\n",
        kebab(&function.ident)
    ));
    text
}

/// A WIT `resource` block: the constructor, then the methods, then the static functions.
fn resource_text(resource: &WitResource) -> String {
    let mut text = String::new();
    for doc in &resource.docs {
        text.push_str(&format!("  ///{doc}\n"));
    }
    text.push_str(&format!("  resource {} {{\n", kebab(&resource.ident)));
    if let Some(constructor) = &resource.constructor {
        let params = constructor
            .params
            .iter()
            .map(|(ident, _, wit)| format!("{}: {wit}", kebab(ident)))
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

/// The kernel's `tembra:guest` package in nested form, appended to the inline WIT so the bindings
/// of a state resolve it; `cargo tembra` reads the same package from `tembra-contracts`.
pub(crate) fn guest_package() -> syn::Result<String> {
    const GUEST: &str = include_str!("../../contracts/wit/tembra/guest.wit");
    nested(GUEST)
        .ok_or_else(|| error("the bundled tembra:guest package has no `package ..;` declaration"))
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
}

/// A WIT type as the generated bindings spell it in the hook's import module and in its export
/// module, which are distinct Rust types.
pub(crate) struct Spelled {
    pub import: String,
    pub export: String,
    /// The UpperCamelCase name of the resource this type is an owned handle of, if it is one: an
    /// exporter wraps the author's value into the bindings' handle type.
    pub resource: Option<String>,
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
fn resolve(dir: &Path) -> syn::Result<Resolve> {
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
fn interface<'a>(
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
                    resource: owned_resource(&resolve, ty),
                })
            };
            let function = |function: &wit_parser::Function| -> syn::Result<ConsumedFunction> {
                Ok(ConsumedFunction {
                    name: function.item_name().to_owned(),
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
                    FunctionKind::Freestanding => consumed.functions.push(function(definition)?),
                    FunctionKind::Constructor(id) => {
                        let params = function(definition)?.params;
                        let index = owner(&mut consumed, id)?;
                        consumed.resources[index].constructor = Some(params);
                    }
                    FunctionKind::Method(id) => {
                        let method = function(definition)?;
                        let index = owner(&mut consumed, id)?;
                        consumed.resources[index].methods.push(method);
                    }
                    FunctionKind::Static(id) => {
                        let function = function(definition)?;
                        let index = owner(&mut consumed, id)?;
                        consumed.resources[index].statics.push(function);
                    }
                    _ => {
                        return Err(error(format!(
                            "{identity}: the macro does not express async functions ({})",
                            definition.name
                        )));
                    }
                }
            }
            Ok((identity.clone(), consumed))
        })
        .collect()
}

/// The resource a WIT type is an owned handle of, through aliases, as the bindings name it.
fn owned_resource(resolve: &Resolve, mut ty: wit_parser::Type) -> Option<String> {
    loop {
        let wit_parser::Type::Id(id) = ty else {
            return None;
        };
        match &resolve.types[id].kind {
            TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => {
                return resolve.types[*resource]
                    .name
                    .as_deref()
                    .map(ToUpperCamelCase::to_upper_camel_case);
            }
            TypeDefKind::Type(inner) => ty = *inner,
            _ => return None,
        }
    }
}

/// The records and variants of the bindings that cannot derive `serde`'s traits: those holding a
/// flags type, a resource handle, a stream or a future, directly or through other types, which
/// the bindings give no `serde` implementation. They are named as the `additional_derives_ignore`
/// option of `wit-bindgen` compares them, by the kebab-case form of their Rust name. `inline` is
/// the WIT the bindings read, `dir` the consumed packages it refers to; a WIT the bindings would
/// refuse yields no names, and the bindings report it.
pub(crate) fn unserializable(dir: Option<&Path>, inline: &str) -> Vec<String> {
    let mut resolve = Resolve::default();
    if let Some(dir) = dir
        && resolve.push_dir(dir).is_err()
    {
        return Vec::new();
    }
    if resolve.push_str("orb.wit", inline).is_err() {
        return Vec::new();
    }
    let names: BTreeSet<String> = resolve
        .types
        .iter()
        .filter(|(_, definition)| {
            matches!(
                definition.kind,
                TypeDefKind::Record(_) | TypeDefKind::Variant(_)
            )
        })
        .filter(|(id, _)| {
            holds_unserializable(&resolve, wit_parser::Type::Id(*id), &mut BTreeSet::new())
        })
        .filter_map(|(_, definition)| definition.name.as_deref())
        .map(|name| name.to_upper_camel_case().to_kebab_case())
        .collect();
    names.into_iter().collect()
}

/// Whether a WIT type holds, anywhere, a value the bindings give no `serde` implementation: a
/// flags type, a resource handle, a stream, a future, an error context, or an array longer than
/// `serde`'s.
fn holds_unserializable(
    resolve: &Resolve,
    ty: wit_parser::Type,
    seen: &mut BTreeSet<wit_parser::TypeId>,
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
    let mut holds = |ty: wit_parser::Type| holds_unserializable(resolve, ty, seen);
    match &resolve.types[id].kind {
        TypeDefKind::Flags(_)
        | TypeDefKind::Handle(_)
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
                    resource: owned_resource(&resolve, ty),
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
                return Ok(format!("{module}::{}", name.to_upper_camel_case()));
            }
            match &definition.kind {
                TypeDefKind::Handle(wit_parser::Handle::Own(resource)) => format!(
                    "{module}::{}",
                    resolve.types[*resource]
                        .name
                        .as_deref()
                        .unwrap_or_default()
                        .to_upper_camel_case()
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
    wit.push_str(&format!("  record {} {{\n", kebab(&item.ident)));
    let mut references = Vec::new();
    let mut named = Vec::new();
    for field in &fields.named {
        let Some(ident) = &field.ident else {
            return Err(syn::Error::new(field.span(), "a record field needs a name"));
        };
        wit.push_str(&format!(
            "    {}: {},\n",
            kebab(ident),
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
    wit.push_str(&format!("  {keyword} {} {{\n", kebab(&item.ident)));
    let mut references = Vec::new();
    let mut cases = Vec::new();
    for variant in &item.variants {
        let case = kebab(&variant.ident);
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
                other => other.to_kebab_case(),
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
