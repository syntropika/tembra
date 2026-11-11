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

/// Write the package: one interface per derived hook, with its callback `apply` and, in the first
/// of them, the types the hooks reach; one interface per derived service, the other types in the
/// first of them and borrowed by the others with `use`, each WIT resource in the interface of the
/// service that owns it; then the public world: importing every consumed interface, importing
/// and exporting every hook the module contributes to, exporting every hook it declares, consumed
/// or derived, and every service interface, derived or implemented. The kernel's private package
/// never appears here: [`build_world`] adds it for the bindings only.
pub(crate) fn render<'a>(
    package: &str,
    world: &str,
    items: Items<'_>,
    contributed: impl Iterator<Item = &'a String>,
    imports: &[String],
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
        let mut mentioned = Vec::new();
        for function in &service.functions {
            for (_, ty, _) in &function.params {
                references(ty, &mut mentioned);
            }
            if let Some((ty, _)) = &function.result {
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
    for import in imports {
        wit.push_str(&format!("  import {import};\n"));
    }
    for hook in contributed {
        wit.push_str(&format!("  import {hook};\n  export {hook};\n"));
    }
    for declaration in declared {
        wit.push_str(&format!("  export {};\n", hook_name(declaration)));
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

/// How the world names a declared hook's interface: by identity when it comes from a consumed
/// package, by its short name when the module derives it.
fn hook_name(declaration: &Declared) -> String {
    match declaration.input {
        Some(_) => kebab(&declaration.ident),
        None => declaration.hook.clone(),
    }
}

/// The world the bindings are generated from: the public world, the kernel's private
/// `tembra:orb/source` world (the lifecycle the host calls and the host operations the component
/// calls), and an import of each declared hook, through which `run` asks the kernel to run it.
/// The same additions the TypeScript build makes before componentizing JavaScript; the contract
/// stays [`render`]'s world.
pub(crate) fn build_world(world: &str, declared: &[Declared]) -> String {
    let mut wit =
        format!("\nworld {world}-orb {{\n  include {world};\n  include {PRIVATE_SOURCE};\n");
    for declaration in declared {
        wit.push_str(&format!("  import {};\n", hook_name(declaration)));
    }
    wit.push_str("}\n");
    wit
}

/// The private package's identity, as `tembra-orb.wit` declares it, and its world of additions.
const PRIVATE_PACKAGE: &str = "tembra:orb@0.1.0";
const PRIVATE_SOURCE: &str = "tembra:orb/source@0.1.0";

/// The kernel's private `tembra:orb` package in nested form, appended to the inline WIT.
///
/// `tembra-orb.wit` is a copy of `tembra_contracts::SOURCE_WIT`, kept inside this crate so the
/// macro needs no file outside it; a test keeps the two identical. Carriage returns are dropped
/// first, so a checkout with CRLF line endings reads the same package.
pub(crate) fn private_package() -> String {
    const PRIVATE: &str = include_str!("tembra-orb.wit");
    let text = PRIVATE.replace("\r\n", "\n");
    let declaration = format!("package {PRIVATE_PACKAGE};\n");
    let (_, body) = text
        .split_once(&declaration)
        .expect("tembra-orb.wit declares the private package");
    format!(
        "package {PRIVATE_PACKAGE} {{\n{}}}\n",
        body.trim_start_matches('\n')
    )
}

/// One service the component exports, as the host's lifecycle check names it: the interface's
/// identity, the JavaScript alias of its name and of each of its functions, resource functions
/// included. The host derives these with `heck`'s lower camel case from the same WIT names.
pub(crate) struct Described {
    pub identity: String,
    pub alias: String,
    pub methods: Vec<String>,
}

/// The services of the public world: every exported interface with functions, except the hook
/// interfaces, which the host serves as callbacks and not as services.
pub(crate) fn described(
    wit: &str,
    dir: Option<&Path>,
    world: &str,
    hooks: &BTreeSet<String>,
) -> syn::Result<Vec<Described>> {
    use heck::ToLowerCamelCase;
    let mut resolve = match dir {
        Some(dir) => resolve(dir)?,
        None => Resolve::default(),
    };
    let package = resolve
        .push_str("orb.wit", wit)
        .map_err(|cause| error(format!("the derived WIT does not resolve: {cause:#}")))?;
    let world = resolve
        .select_world(&[package], Some(world))
        .map_err(|cause| error(format!("the derived world does not resolve: {cause:#}")))?;
    let mut services = Vec::new();
    for item in resolve.worlds[world].exports.values() {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let identity = resolve.id_of(*id).unwrap_or_default();
        let interface = &resolve.interfaces[*id];
        if hooks.contains(&identity) || interface.functions.is_empty() {
            continue;
        }
        services.push(Described {
            identity,
            alias: interface
                .name
                .as_deref()
                .unwrap_or_default()
                .to_lower_camel_case(),
            methods: interface
                .functions
                .keys()
                .map(|name| name.to_lower_camel_case())
                .collect(),
        });
    }
    Ok(services)
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

/// A freestanding function of an imported interface, spelled as wit-bindgen spells the import in
/// `module`: each parameter as the import takes it, and the result.
pub(crate) struct Imported {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub result: Option<String>,
}

/// The freestanding functions of an imported interface, which a call must target at a provider
/// object: the macro wraps each to select the root's reference first. Resource methods target
/// their handle and are not listed. A resource's constructor and static functions are not listed
/// either, and the macro cannot wrap them: calling one fails at run time for want of a
/// selection. Async functions never reach here: reading the consumed interface refuses them.
pub(crate) fn imported(dir: &Path, identity: &str, module: &str) -> syn::Result<Vec<Imported>> {
    let resolve = resolve(dir)?;
    let interface = interface(&resolve, dir, identity)?;
    interface
        .functions
        .values()
        .filter(|function| matches!(function.kind, wit_parser::FunctionKind::Freestanding))
        .map(|function| {
            Ok(Imported {
                name: function.name.clone(),
                params: function
                    .params
                    .iter()
                    .map(|param| {
                        Ok((
                            param.name.clone(),
                            import_param(&resolve, param.ty, module)?,
                        ))
                    })
                    .collect::<syn::Result<_>>()?,
                result: function
                    .result
                    .map(|ty| rust_type(&resolve, ty, module))
                    .transpose()?,
            })
        })
        .collect()
}

/// A parameter of an imported function as wit-bindgen's default owning bindings take it: a
/// string as `&str`, a list as a slice of owned elements, an option around either, a named type
/// holding a string or a list (and no owned handle) by reference, anything else by value.
fn import_param(resolve: &Resolve, ty: wit_parser::Type, module: &str) -> syn::Result<String> {
    use wit_parser::Type;
    let Type::Id(id) = ty else {
        return match ty {
            Type::String => Ok("&str".into()),
            ty => rust_type(resolve, ty, module),
        };
    };
    let definition = &resolve.types[id];
    if definition.name.is_some() {
        let owned = rust_type(resolve, ty, module)?;
        return Ok(
            if carries(
                resolve,
                ty,
                &|kind| matches!(kind, TypeDefKind::List(_)),
                true,
            ) && !carries(
                resolve,
                ty,
                &|kind| matches!(kind, TypeDefKind::Handle(wit_parser::Handle::Own(_))),
                false,
            ) {
                format!("&{owned}")
            } else {
                owned
            },
        );
    }
    match &definition.kind {
        TypeDefKind::Type(inner) => import_param(resolve, *inner, module),
        TypeDefKind::List(element) => Ok(format!("&[{}]", rust_type(resolve, *element, module)?)),
        TypeDefKind::Option(inner) => Ok(format!(
            "Option<{}>",
            import_param(resolve, *inner, module)?
        )),
        _ if carries(
            resolve,
            ty,
            &|kind| matches!(kind, TypeDefKind::List(_)),
            true,
        ) =>
        {
            Err(error(
                "an imported function taking a tuple or a result that holds a string or a list is not expressed by the macro yet",
            ))
        }
        _ => rust_type(resolve, ty, module),
    }
}

/// Whether a type holds, anywhere, a definition `found` accepts (`strings` counts a string too).
fn carries(
    resolve: &Resolve,
    ty: wit_parser::Type,
    found: &dyn Fn(&TypeDefKind) -> bool,
    strings: bool,
) -> bool {
    let wit_parser::Type::Id(id) = ty else {
        return strings && matches!(ty, wit_parser::Type::String);
    };
    let kind = &resolve.types[id].kind;
    if found(kind) {
        return true;
    }
    let inner = |ty: &wit_parser::Type| carries(resolve, *ty, found, strings);
    match kind {
        TypeDefKind::Type(ty) | TypeDefKind::Option(ty) | TypeDefKind::List(ty) => inner(ty),
        TypeDefKind::Record(record) => record.fields.iter().any(|field| inner(&field.ty)),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().any(inner),
        TypeDefKind::Variant(variant) => variant
            .cases
            .iter()
            .any(|case| case.ty.as_ref().is_some_and(inner)),
        TypeDefKind::Result(result) => {
            result.ok.as_ref().is_some_and(inner) || result.err.as_ref().is_some_and(inner)
        }
        _ => false,
    }
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
                    waterfall: function.result == Some(param.ty),
                    bail: function.result.is_some_and(|ty| optional(&resolve, ty)),
                    input: spell(param.ty)?,
                    result: function.result.map(spell).transpose()?,
                },
            ))
        })
        .collect()
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
                    format!("Option<{}>", rust_type(resolve, *inner, module)?)
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
                    format!("Result<{}, {}>", side(result.ok)?, side(result.err)?)
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
    for doc in docs(&item.attrs) {
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
    for doc in docs(&item.attrs) {
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
            let args = || -> syn::Result<Vec<String>> {
                let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                    return Err(syn::Error::new(
                        segment.span(),
                        "type parameters are required",
                    ));
                };
                args.args
                    .iter()
                    .map(|arg| match arg {
                        syn::GenericArgument::Type(Type::Tuple(t)) if t.elems.is_empty() => {
                            Ok("_".to_owned())
                        }
                        syn::GenericArgument::Type(ty) => wit_type(ty),
                        other => Err(syn::Error::new(other.span(), "unsupported type parameter")),
                    })
                    .collect()
            };
            Ok(match name.as_str() {
                "i8" => "s8".into(),
                "i16" => "s16".into(),
                "i32" => "s32".into(),
                "i64" => "s64".into(),
                "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" => name,
                "String" => "string".into(),
                "Vec" => format!("list<{}>", args()?.join(", ")),
                "Option" => format!("option<{}>", args()?.join(", ")),
                "Result" => format!("result<{}>", args()?.join(", ")),
                other => other.to_kebab_case(),
            })
        }
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

/// The `///` lines of an item, carried into the WIT.
pub(crate) fn docs(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(meta) if meta.path.is_ident("doc") => match &meta.value {
                Expr::Lit(lit) => match &lit.lit {
                    Lit::Str(s) => Some(s.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}
