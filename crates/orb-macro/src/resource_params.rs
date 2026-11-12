//! The resources a derived function takes as parameters: a `#[wit_resource]` struct of the module,
//! owned, `own<t>` in the WIT, and the handle of an imported interface, `texts::Builder`, which the
//! derived interface `use`s from the imported one. Either may sit in an option, a result, a list or
//! a tuple. The runtime moves an owned resource into the interface that defines it, or into an Orb
//! that imports that interface; it refuses at installation an Orb whose function takes a borrowed
//! one, `borrow<t>`, so a `&T` parameter stays refused, as every reference in a signature is.
use crate::{
    Constructor, Declared, Function, Handles, Service, Shape, WitResource, handles_of, rebuild,
    type_ident, wit, with_handles,
};
use quote::quote;
use std::collections::BTreeMap;
use syn::{Ident, Type, spanned::Spanned};

/// The modules the bindings generate for the imported interfaces, by the name the Orb module
/// re-exports each under, `texts`, with the interface's identity.
pub(crate) fn modules(imports: &[String]) -> BTreeMap<String, String> {
    imports
        .iter()
        .map(|identity| {
            let [_, _, interface] = crate::hook_path(identity);
            (interface.to_string(), identity.clone())
        })
        .collect()
}

/// The imported interface and the handle a type names, when it is the handle of an imported
/// interface's resource, written through the interface's module: `texts::Builder`, or any path
/// ending so.
fn imported<'a, 't>(
    ty: &'t Type,
    modules: &'a BTreeMap<String, String>,
) -> Option<(&'a str, &'t Ident)> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segments: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
    let [.., module, handle] = segments.as_slice() else {
        return None;
    };
    if path.qself.is_some() || !matches!(handle.arguments, syn::PathArguments::None) {
        return None;
    }
    let identity = modules.get(&module.ident.to_string())?;
    Some((identity.as_str(), &handle.ident))
}

/// Each handle of an imported interface a type holds, itself or inside options, results, lists and
/// tuples, with its interface.
fn imported_in<'a, 't>(
    ty: &'t Type,
    modules: &'a BTreeMap<String, String>,
    out: &mut Vec<(&'a str, &'t Ident)>,
) {
    if let Some(found) = imported(ty, modules) {
        out.push(found);
        return;
    }
    match ty {
        Type::Path(path) => {
            if let Some(syn::PathArguments::AngleBracketed(arguments)) =
                path.path.segments.last().map(|segment| &segment.arguments)
            {
                for argument in &arguments.args {
                    if let syn::GenericArgument::Type(ty) = argument {
                        imported_in(ty, modules, out);
                    }
                }
            }
        }
        Type::Tuple(tuple) => {
            for element in &tuple.elems {
                imported_in(element, modules, out);
            }
        }
        _ => {}
    }
}

/// The module types a parameter's type mentions, as `wit::references` collects them, less the
/// handles of imported interfaces, which are no types of the module.
pub(crate) fn references(ty: &Type, modules: &BTreeMap<String, String>, out: &mut Vec<Ident>) {
    if imported(ty, modules).is_some() {
        return;
    }
    match ty {
        Type::Path(path) => {
            // The path itself, read as `wit::references` reads it, then its type parameters.
            let mut bare = path.clone();
            let Some(last) = bare.path.segments.last_mut() else {
                return;
            };
            let arguments = std::mem::replace(&mut last.arguments, syn::PathArguments::None);
            wit::references(&Type::Path(bare), out);
            if let syn::PathArguments::AngleBracketed(arguments) = arguments {
                for argument in &arguments.args {
                    if let syn::GenericArgument::Type(ty) = argument {
                        references(ty, modules, out);
                    }
                }
            }
        }
        Type::Tuple(tuple) => {
            for element in &tuple.elems {
                references(element, modules, out);
            }
        }
        _ => {}
    }
}

/// A function or a constructor of a derived interface, for the checks: what it is called in the
/// errors, the service whose interface holds it, its parameters and its result.
struct Derived<'a> {
    name: String,
    interface: &'a Ident,
    params: &'a [(Ident, Type, String)],
    result: Option<&'a Type>,
}

/// The functions and constructors of the derived interfaces: each derived service's functions, and
/// the constructor, methods and static functions of the resources such a service owns.
fn derived<'a>(services: &'a [Service], resources: &'a [WitResource]) -> Vec<Derived<'a>> {
    let mut functions = Vec::new();
    let entry = |owner: &Ident, interface: &'a Ident, function: &'a Function| Derived {
        name: format!("{owner}::{}", function.ident),
        interface,
        params: &function.params,
        result: function.result.as_ref().map(|(ty, _)| ty),
    };
    for service in services
        .iter()
        .filter(|service| service.implements.is_none())
    {
        for function in &service.functions {
            functions.push(entry(&service.ident, &service.ident, function));
        }
    }
    for resource in resources
        .iter()
        .filter(|resource| !resource.implemented(services))
    {
        let interface = resource.service.as_ref().expect("resolved owner");
        for function in resource.methods.iter().chain(&resource.statics) {
            functions.push(entry(&resource.ident, interface, function));
        }
        if let Some(Constructor { ident, params }) = &resource.constructor {
            functions.push(Derived {
                name: format!("{}::{ident}", resource.ident),
                interface,
                params,
                result: None,
            });
        }
    }
    functions
}

/// Refuse a resource where the runtime cannot carry it, before the WIT is derived: a parameter of a
/// derived function takes a resource of its own interface, owned, or the handle of an imported
/// interface other than WASI's, which no result of a derived function holds, because the runtime
/// returns a resource only from the interface that defines it; the input of a derived hook holds no
/// resource. A resource of an implemented interface is that interface's: a derived function takes
/// none.
pub(crate) fn check(
    services: &[Service],
    resources: &[WitResource],
    declared: &[Declared],
    modules: &BTreeMap<String, String>,
) -> syn::Result<()> {
    let resource_named = |ident: &Ident| resources.iter().find(|resource| resource.ident == *ident);
    for function in derived(services, resources) {
        for (_, ty, _) in function.params {
            let mut handles = Vec::new();
            imported_in(ty, modules, &mut handles);
            if let Some((identity, handle)) = handles
                .iter()
                .find(|(identity, _)| crate::wasi::is_wasi(identity))
            {
                return Err(syn::Error::new(
                    ty.span(),
                    format!(
                        "`{}` takes `{handle}`, a resource of the WASI interface {identity}: the runtime moves no WASI handle between Orbs",
                        function.name
                    ),
                ));
            }
            let mut mentioned = Vec::new();
            references(ty, modules, &mut mentioned);
            for resource in mentioned.iter().filter_map(resource_named) {
                let owner = resource.service.as_ref().expect("resolved owner");
                let implemented = services
                    .iter()
                    .find(|service| service.ident == *owner)
                    .and_then(|service| service.implements.as_deref());
                if let Some(identity) = implemented {
                    return Err(syn::Error::new(
                        ty.span(),
                        format!(
                            "`{}` takes `{}`, a resource of the implemented interface {identity}: a derived function takes the resources of the interfaces this module derives and the handles of the interfaces it imports",
                            function.name, resource.ident
                        ),
                    ));
                }
                if owner != function.interface {
                    return Err(syn::Error::new(
                        ty.span(),
                        format!(
                            "`{}` takes `{}`, a resource of the interface `{}`: the runtime moves an owned resource only into the interface that defines it, so take it in a function of `{owner}` or of one of its resources",
                            function.name,
                            resource.ident,
                            wit::kebab(owner)
                        ),
                    ));
                }
            }
        }
        if let Some(ty) = function.result {
            let mut handles = Vec::new();
            imported_in(ty, modules, &mut handles);
            if let Some((identity, handle)) = handles.first() {
                return Err(syn::Error::new(
                    ty.span(),
                    format!(
                        "`{}` returns `{handle}`, a resource of the imported interface {identity}: the runtime returns a resource only from the interface that defines it, so a derived function takes such a handle but returns none",
                        function.name
                    ),
                ));
            }
        }
    }
    for declaration in declared {
        let Some((ty, _)) = &declaration.input else {
            continue;
        };
        let mut mentioned = Vec::new();
        wit::references(ty, &mut mentioned);
        if let Some(resource) = mentioned.iter().find_map(resource_named) {
            return Err(syn::Error::new(
                ty.span(),
                format!(
                    "the input of the hook `{}` holds the resource `{}`: a hook's input holds no resource",
                    declaration.ident, resource.ident
                ),
            ));
        }
    }
    Ok(())
}

/// What a derived interface `use`s from an imported one: its identity, and each resource whose
/// handle the interface's functions take, by its WIT name and by the handle's Rust name, whose
/// kebab-case form names it in the derived interface.
pub(crate) struct Foreign {
    pub identity: String,
    pub resources: Vec<(String, Ident)>,
}

impl Foreign {
    /// The `use` line of the interface's text: `use ns:name/interface@version.{builder};`, each
    /// resource renamed with `as` where its WIT name is not the kebab-case form of its handle.
    pub(crate) fn text(&self) -> String {
        let names: Vec<String> = self
            .resources
            .iter()
            .map(|(name, handle)| {
                let local = wit::kebab(handle);
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

/// The imported interfaces' resources each derived interface `use`s, by the interface's WIT name:
/// those whose handles its functions and its resources' functions take. Each handle names a
/// resource of its interface, as the consumed WIT defines it, and no type of the module that
/// enters the WIT, nor a resource of the module, takes its name, which the derived interface gives
/// the handle.
pub(crate) fn foreign(
    services: &[Service],
    resources: &[WitResource],
    shapes: &[Shape],
    modules: &BTreeMap<String, String>,
    consumed: &BTreeMap<String, wit::Consumed>,
) -> syn::Result<BTreeMap<String, Vec<Foreign>>> {
    let mut foreign: BTreeMap<String, Vec<Foreign>> = BTreeMap::new();
    for function in derived(services, resources) {
        let mut handles = Vec::new();
        for (_, ty, _) in function.params {
            imported_in(ty, modules, &mut handles);
        }
        for (identity, handle) in handles {
            let defined = consumed[identity]
                .resources
                .iter()
                .find(|resource| type_ident(&resource.name) == *handle);
            let Some(defined) = defined else {
                let names: Vec<String> = consumed[identity]
                    .resources
                    .iter()
                    .map(|resource| format!("`{}`", type_ident(&resource.name)))
                    .collect();
                let known = if names.is_empty() {
                    "it defines none".to_owned()
                } else {
                    format!("its resources are {}", names.join(", "))
                };
                return Err(syn::Error::new(
                    handle.span(),
                    format!(
                        "`{}` takes `{handle}`, which names no resource of the imported interface {identity}: {known}",
                        function.name
                    ),
                ));
            };
            let clash = shapes
                .iter()
                .map(|shape| &shape.ident)
                .chain(resources.iter().map(|resource| &resource.ident))
                .find(|ident| *ident == handle);
            if let Some(clash) = clash {
                return Err(syn::Error::new(
                    clash.span(),
                    format!(
                        "`{clash}` of this module has the name of `{handle}`, the resource `{}` of the imported interface {identity}, which `{}` takes: the interface knows both by one name, so rename the module's `{clash}`",
                        defined.name, function.name
                    ),
                ));
            }
            let uses = foreign.entry(wit::kebab(function.interface)).or_default();
            let index = match uses.iter().position(|used| used.identity == identity) {
                Some(index) => index,
                None => {
                    uses.push(Foreign {
                        identity: identity.to_owned(),
                        resources: Vec::new(),
                    });
                    uses.len() - 1
                }
            };
            let named = &mut uses[index].resources;
            if !named.iter().any(|(_, known)| known == handle) {
                named.push((defined.name.clone(), handle.clone()));
            }
        }
    }
    Ok(foreign)
}

/// The parameters of a derived function's bridge and the arguments it calls the author's function
/// with: a parameter holding a resource of the module is the bindings' handle type there, and each
/// handle is taken back into the author's value, which consumes it; any other parameter, an
/// imported interface's handle included, crosses as it is.
pub(crate) fn bridged(
    params: &[(Ident, Type, String)],
    module: &proc_macro2::TokenStream,
    resources: &BTreeMap<String, Ident>,
) -> (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) {
    params
        .iter()
        .map(|(ident, ty, _)| match handles_of(ty, resources) {
            Some(handles) => {
                let spelled = with_handles(ty, resources, module);
                (
                    quote! { #ident: #spelled },
                    taken(&handles, quote! { #ident }),
                )
            }
            None => (quote! { #ident: #ty }, quote! { #ident }),
        })
        .unzip()
}

/// An expression taking each handle `handles` places in `value` back into the author's value, its
/// type inferred from the author's parameter, and rebuilding the containers around it.
fn taken(handles: &Handles, value: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    rebuild(handles, value, &|_, value| quote! { #value.into_inner() })
}
