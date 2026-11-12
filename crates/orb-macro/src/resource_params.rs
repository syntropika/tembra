//! The resources a derived function takes as parameters: a `#[wit_resource]` struct of the module,
//! owned, `own<t>` in the WIT, and the handle of an imported interface, `texts::Builder`, which the
//! derived interface `use`s from the imported one, as `foreign` checks. Either may sit in an
//! option, a result, a list or a tuple. The runtime moves an owned resource into the interface that
//! defines it, or into an Orb that imports that interface, and returns one only from the interface
//! that defines it; it refuses at installation an Orb whose function takes a borrowed one,
//! `borrow<t>`, so a `&T` parameter stays refused, as every reference in a signature is.
use crate::{
    Constructor, Declared, Function, Handles, Service, Shape, WitResource, foreign::Modules,
    handles_of, rebuild, wit, with_handles,
};
use quote::quote;
use std::collections::{BTreeMap, BTreeSet};
use syn::{Ident, Type, spanned::Spanned};

/// A function or a constructor of a derived interface, for the checks: what it is called in the
/// errors, the service whose interface holds it, its parameters and its result.
pub(crate) struct Derived<'a> {
    pub name: String,
    pub interface: &'a Ident,
    pub params: &'a [(Ident, Type, String)],
    pub result: Option<&'a Type>,
}

/// The functions and constructors of the derived interfaces: each derived service's functions, and
/// the constructor, methods and static functions of the resources such a service owns.
pub(crate) fn derived<'a>(
    services: &'a [Service],
    resources: &'a [WitResource],
) -> Vec<Derived<'a>> {
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

/// Refuse a resource of the module where the runtime cannot carry it, before the WIT is derived: a
/// parameter of a derived function takes a resource of its own interface, owned, because the
/// runtime moves an owned resource only into the interface that defines it, and the input of a
/// derived hook holds no resource. A resource of an implemented interface is that interface's: a
/// derived function takes none. What a consumed interface's module names, `foreign` checks.
pub(crate) fn check(
    services: &[Service],
    resources: &[WitResource],
    declared: &[Declared],
    modules: &Modules,
) -> syn::Result<()> {
    let resource_named = |ident: &Ident| resources.iter().find(|resource| resource.ident == *ident);
    for function in derived(services, resources) {
        for (_, ty, _) in function.params {
            let mut mentioned = Vec::new();
            modules.references(ty, &mut mentioned);
            for resource in mentioned.iter().filter_map(resource_named) {
                if let Some(owner) = foreign_owner(services, resource, function.interface) {
                    return Err(taking_error(&function, ty, resource, None, owner));
                }
            }
        }
    }
    for declaration in declared {
        let Some((ty, _)) = &declaration.input else {
            continue;
        };
        let mut mentioned = Vec::new();
        modules.references(ty, &mut mentioned);
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

/// Refuse, once the types that enter the WIT are known, a resource the runtime would not carry
/// through a derived function: its result holds, anywhere, only resources of its own interface,
/// itself or in the records and variants of the module it reaches, because the runtime returns a
/// resource only from the interface that defines it; the records and variants a parameter takes
/// hold none of another interface either, which the runtime would not move in; and those of a
/// derived hook's input hold no resource. What a parameter or a hook's input names itself `check`
/// has refused already.
pub(crate) fn check_reached(
    services: &[Service],
    resources: &[WitResource],
    declared: &[Declared],
    shapes: &[Shape],
    modules: &Modules,
) -> syn::Result<()> {
    for function in derived(services, resources) {
        for (_, ty, _) in function.params {
            for (resource, via) in held(ty, shapes, resources, modules) {
                if let Some(owner) = foreign_owner(services, resource, function.interface) {
                    return Err(taking_error(&function, ty, resource, via, owner));
                }
            }
        }
        let Some(ty) = function.result else {
            continue;
        };
        for (resource, via) in held(ty, shapes, resources, modules) {
            let Some(owner) = foreign_owner(services, resource, function.interface) else {
                continue;
            };
            let returned = holding(resource, via);
            let message = match owner {
                Owner::Implemented(identity) => format!(
                    "`{}` returns {returned} a resource of the implemented interface {identity}: the runtime returns a resource only from the interface that defines it",
                    function.name
                ),
                Owner::Interface(owner) => format!(
                    "`{}` returns {returned} a resource of the interface `{}`: the runtime returns a resource only from the interface that defines it, so return it from a function of `{owner}` or of one of its resources",
                    function.name,
                    wit::kebab(owner)
                ),
            };
            return Err(syn::Error::new(ty.span(), message));
        }
    }
    for declaration in declared {
        let Some((ty, _)) = &declaration.input else {
            continue;
        };
        if let Some((resource, Some(via))) = held(ty, shapes, resources, modules).first() {
            return Err(syn::Error::new(
                ty.span(),
                format!(
                    "the input of the hook `{}` holds `{via}`, which holds the resource `{}`: a hook's input holds no resource",
                    declaration.ident, resource.ident
                ),
            ));
        }
    }
    Ok(())
}

/// Where a resource a derived function of `interface` names is defined, when it is not that
/// interface, the only one the runtime moves it into or returns it from.
enum Owner<'a> {
    /// An implemented interface, by its identity.
    Implemented(&'a str),
    /// Another derived interface, by the service whose interface it is.
    Interface(&'a Ident),
}

/// The interface defining `resource`, when it is another than `interface`.
fn foreign_owner<'a>(
    services: &'a [Service],
    resource: &'a WitResource,
    interface: &Ident,
) -> Option<Owner<'a>> {
    let owner = resource.service.as_ref().expect("resolved owner");
    let implemented = services
        .iter()
        .find(|service| service.ident == *owner)
        .and_then(|service| service.implements.as_deref());
    match implemented {
        Some(identity) => Some(Owner::Implemented(identity)),
        None if owner != interface => Some(Owner::Interface(owner)),
        None => None,
    }
}

/// The resource a type holds, for the errors: itself, or the record or variant that holds it.
fn holding(resource: &WitResource, via: Option<&Ident>) -> String {
    match via {
        Some(via) => format!("`{via}`, which holds `{}`,", resource.ident),
        None => format!("`{}`,", resource.ident),
    }
}

/// The error of a derived function taking a resource of another interface, on the parameter's type.
fn taking_error(
    function: &Derived<'_>,
    ty: &Type,
    resource: &WitResource,
    via: Option<&Ident>,
    owner: Owner<'_>,
) -> syn::Error {
    let taken = holding(resource, via);
    let message = match owner {
        Owner::Implemented(identity) => format!(
            "`{}` takes {taken} a resource of the implemented interface {identity}: a derived function takes the resources of the interfaces this module derives and the handles of the interfaces it imports",
            function.name
        ),
        Owner::Interface(owner) => format!(
            "`{}` takes {taken} a resource of the interface `{}`: the runtime moves an owned resource only into the interface that defines it, so take it in a function of `{owner}` or of one of its resources",
            function.name,
            wit::kebab(owner)
        ),
    };
    syn::Error::new(ty.span(), message)
}

/// Each resource of the module a type holds, with the record or variant of the module that holds
/// it, if one does: the resource itself or inside options, results, lists and tuples, or in the
/// fields and cases of the records and variants the type reaches, followed by name as the WIT
/// names them.
fn held<'a>(
    ty: &Type,
    shapes: &'a [Shape],
    resources: &'a [WitResource],
    modules: &Modules,
) -> Vec<(&'a WitResource, Option<&'a Ident>)> {
    let mut mentioned = Vec::new();
    modules.references(ty, &mut mentioned);
    let mut pending: Vec<(Ident, Option<&'a Ident>)> =
        mentioned.into_iter().map(|ident| (ident, None)).collect();
    let mut seen = BTreeSet::new();
    let mut held = Vec::new();
    while let Some((ident, via)) = pending.pop() {
        if !seen.insert(ident.to_string()) {
            continue;
        }
        if let Some(resource) = resources.iter().find(|resource| resource.ident == ident) {
            held.push((resource, via));
        } else if let Some(shape) = shapes.iter().find(|shape| shape.ident == ident) {
            let via = via.or(Some(&shape.ident));
            pending.extend(
                shape
                    .references
                    .iter()
                    .map(|reference| (reference.clone(), via)),
            );
        }
    }
    held
}

/// The parameters of a derived function's bridge and the arguments it calls the author's function
/// with: a parameter holding a resource of the module is the bindings' handle type there, and each
/// handle is taken back into the author's value, which consumes it; any other parameter, one a
/// consumed interface's module names included, crosses as it is.
pub(crate) fn bridged(
    params: &[(Ident, Type, String)],
    module: &proc_macro2::TokenStream,
    resources: &BTreeMap<String, Ident>,
    modules: &Modules,
) -> (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) {
    params
        .iter()
        .map(|(ident, ty, _)| match handles_of(ty, resources, modules) {
            Some(handles) => {
                let spelled = with_handles(ty, resources, module, modules);
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
