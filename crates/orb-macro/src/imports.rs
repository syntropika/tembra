//! The component side of the common awaitable Orb import API.
#[cfg(test)]
mod tests;
mod values;

use crate::{Analysis, hook_path, import_module, rust_ident, type_ident};
use proc_macro2::TokenStream;
use quote::quote;
use std::collections::BTreeSet;
use values::Values;
use wit_parser::{Function, FunctionKind, Type, TypeDefKind};

/// Hide component ABI calling conventions behind the same owned, awaitable API as native imports.
pub(crate) fn modules(analysis: &Analysis, served: &BTreeSet<String>) -> TokenStream {
    let facade = analysis.options.facade();
    let guest = |field: &syn::Ident, body: TokenStream| {
        if served.contains(&field.to_string()) {
            quote! { ::#facade::__guest! { #body } }
        } else {
            body
        }
    };
    let plain = analysis
        .imports
        .iter()
        .filter(|id| !crate::wasi::is_wasi(id))
        .map(|id| {
            let [_, _, field] = hook_path(id);
            let raw = import_module(1, &hook_path(id));
            let interface = analysis
                .consumed_types
                .interface(id)
                .expect("an imported interface");
            let values = Values {
                types: &analysis.consumed_types,
            };
            let declarations = interface
                .types
                .iter()
                .map(|(name, ty)| values.declaration(name, *ty));
            let functions = interface
                .functions
                .values()
                .filter(|f| {
                    matches!(
                        f.kind,
                        FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
                    )
                })
                .map(|f| function(f, &values, &raw, false));
            let resources = interface.types.iter().filter_map(|(_, id)| {
                let Ok((canonical, TypeDefKind::Resource)) = values.types.definition(Type::Id(*id))
                else {
                    return None;
                };
                if values.types.native_alias(canonical) != *id {
                    return None;
                }
                let methods = interface
                    .functions
                    .values()
                    .filter(|f| match f.kind {
                        FunctionKind::Constructor(resource)
                        | FunctionKind::Static(resource)
                        | FunctionKind::AsyncStatic(resource)
                        | FunctionKind::Method(resource)
                        | FunctionKind::AsyncMethod(resource) => resource == *id,
                        _ => false,
                    })
                    .map(|f| function(f, &values, &raw, false));
                let name = values.ty(Type::Id(*id));
                Some(quote! { impl #name { #(#methods)* } })
            });
            guest(
                &field,
                quote! {
                    /// Awaitable calls retaining the imported interface's WIT calling conventions.
                    pub mod #field {
                        extern crate alloc as __tembra_alloc;
                        #(#declarations)*
                        #(#functions)*
                        #(#resources)*
                    }
                },
            )
        });
    let keyed = analysis.views.iter().map(|view| {
        let field = rust_ident(&view.name);
        let raw = import_module(1, &hook_path(&crate::identity(&analysis.package, &view.name)));
        let interface = analysis.consumed_types.interface(&view.target).expect("a keyed interface");
        let resource = type_ident(interface.name.as_deref().unwrap_or_default());
        let defining = analysis.consumed_types.imported_module(&view.target, 1);
        let values = Values { types: &analysis.consumed_types };
        let declarations = interface.types.keys().filter(|name| view.types.contains(*name)).map(|name| {
            let name = type_ident(name);
            quote! { pub use #defining::#name; }
        });
        let functions = interface.functions.values().map(|f| function(f, &values, &raw, true));
        guest(&field, quote! {
            /// Objects selected by key, using the common awaitable import API.
            pub mod #field {
                extern crate alloc as __tembra_alloc;
                #(#declarations)*
                /// An owned service reference, distinct from provider-owned WIT resources.
                pub struct #resource { inner: #raw::#resource }
                impl #resource { #(#functions)* }
                /// Acquire the object under the importing call's principal.
                pub async fn get(key: __tembra_alloc::vec::Vec<__tembra_alloc::string::String>) -> ::core::result::Result<#resource, __tembra_alloc::string::String> {
                    #raw::get(key).await.map(|inner| #resource { inner })
                }
            }
        })
    });
    quote! { #(#plain)* #(#keyed)* }
}

/// Wrap a raw synchronous or asynchronous binding without adding a suspension point.
fn function(
    function: &Function,
    values: &Values<'_>,
    raw: &TokenStream,
    keyed: bool,
) -> TokenStream {
    let method = keyed
        || matches!(
            function.kind,
            FunctionKind::Method(_) | FunctionKind::AsyncMethod(_)
        );
    let skip = usize::from(method && !keyed);
    let name = if matches!(function.kind, FunctionKind::Constructor(_)) {
        rust_ident("new")
    } else {
        rust_ident(function.name.rsplit('.').next().unwrap_or(&function.name))
    };
    let asynchronous = matches!(
        function.kind,
        FunctionKind::AsyncFreestanding
            | FunctionKind::AsyncStatic(_)
            | FunctionKind::AsyncMethod(_)
    );
    let parameters = function.params.iter().skip(skip).map(|p| {
        let name = rust_ident(&p.name);
        let ty = values.ty(p.ty);
        quote! { #name: #ty }
    });
    let conversions = function
        .params
        .iter()
        .skip(skip)
        .filter(|p| values.types.carries_resource(p.ty))
        .map(|p| {
            let name = rust_ident(&p.name);
            let value = values.convert(p.ty, quote! { #name }, false);
            quote! { let #name = #value; }
        });
    let arguments = function.params.iter().skip(skip).map(|p| {
        let name = rust_ident(&p.name);
        let passed = values.types.passed(p.ty);
        if asynchronous || matches!(passed, crate::wit::Passed::Owned) {
            quote! { #name }
        } else {
            crate::lent(&passed, quote! { &#name })
        }
    });
    let target = if method {
        quote! { self.inner.#name }
    } else {
        match function.kind {
            FunctionKind::Constructor(id)
            | FunctionKind::Static(id)
            | FunctionKind::AsyncStatic(id) => {
                let raw_type = values.types.imported_path(id, 1);
                quote! { #raw_type::#name }
            }
            _ => quote! { #raw::#name },
        }
    };
    let receiver = method.then(|| quote! { &self, });
    let await_ = asynchronous.then(|| quote! { .await });
    let call = quote! { #target(#(#arguments),*) #await_ };
    let (result, body) = match function.result {
        Some(ty) => (values.ty(ty), values.convert(ty, call, true)),
        None => (quote! { () }, call),
    };
    quote! {
        /// Call the imported WIT operation using its declared ABI.
        pub async fn #name(#receiver #(#parameters),*) -> #result {
            #(#conversions)*
            #body
        }
    }
}
