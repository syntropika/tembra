//! Awaitable native imports of WIT functions and provider-owned resource methods.
use super::{Import, values::Values};
use crate::{Analysis, rust_ident, type_ident};
use proc_macro2::TokenStream;
use quote::quote;
use wit_parser::{Function, FunctionKind, Type, TypeDefKind};

/// Generate the interface's functions; resource functions live on their owning wrapper.
pub(super) fn functions(import: &Import<'_>, analysis: &Analysis) -> ::std::vec::Vec<TokenStream> {
    analysis
        .consumed_types
        .interface(import.interface)
        .expect("an interface")
        .functions
        .values()
        .filter(|f| {
            matches!(
                f.kind,
                FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
            )
        })
        .map(|f| function(import, f, analysis))
        .collect()
}

/// Generate a non-cloneable wrapper for each resource declared in the defining interface.
pub(super) fn resources(import: &Import<'_>, analysis: &Analysis) -> TokenStream {
    let facade = &analysis.options.facade();
    let types = &analysis.consumed_types;
    let interface = types.interface(import.interface).expect("an interface");
    let resources = interface.types.iter().filter_map(|(name, id)| {
        let ::core::result::Result::Ok((canonical, TypeDefKind::Resource)) = types.definition(Type::Id(*id)) else {
            return ::core::option::Option::None;
        };
        if types.native_alias(canonical) != *id {
            return ::core::option::Option::None;
        }
        let ident = type_ident(name);
        let (identity, nominal_name) = types.resource_nominal(canonical);
        let methods = interface.functions.values().filter(|f| match f.kind {
            FunctionKind::Constructor(resource) | FunctionKind::Static(resource) | FunctionKind::AsyncStatic(resource) | FunctionKind::Method(resource) | FunctionKind::AsyncMethod(resource) => resource == *id,
            _ => false,
        }).map(|f| function(import, f, analysis));
        let doc = format!(" An owned `{identity}#{name}` handle. Dropping it releases its existing SDK ownership claim.");
        ::core::option::Option::Some(quote! {
            #[doc = #doc]
            pub struct #ident { owned: ::core::option::Option<::#facade::portable::Owned> }
            impl #ident { #(#methods)* }
            impl ::core::convert::TryFrom<::#facade::portable::Owned> for #ident {
                type Error = ::#facade::portable::Owned;
                /// Wrap only a handle of this exact nominal WIT resource type.
                fn try_from(owned: ::#facade::portable::Owned) -> ::core::result::Result<Self, Self::Error> {
                    if owned.nominal().interface != #identity || owned.nominal().name != #nominal_name { return ::core::result::Result::Err(owned); }
                    ::core::result::Result::Ok(Self { owned: ::core::option::Option::Some(owned) })
                }
            }
            impl ::core::ops::Drop for #ident {
                /// Queue this WIT drop at its importing callback's settlement checkpoint.
                fn drop(&mut self) {
                    if let ::core::option::Option::Some(owned) = self.owned.take() { super::__tembra_import::drop_resource(owned); }
                }
            }
            impl ::core::convert::From<#ident> for ::#facade::portable::Owned {
                /// Move the existing owner without duplicating its claim.
                fn from(mut value: #ident) -> Self { value.owned.take().expect("a live owner") }
            }
        })
    });
    quote! { #(#resources)* }
}

/// Generate an awaitable call while retaining the provider's original WIT name and kind.
fn function(import: &Import<'_>, function: &Function, analysis: &Analysis) -> TokenStream {
    let facade = &analysis.options.facade();
    let values = Values {
        types: &analysis.consumed_types,
        facade,
    };
    let field = &import.field;
    let method = matches!(
        function.kind,
        FunctionKind::Method(_) | FunctionKind::AsyncMethod(_)
    );
    let name = match function.kind {
        FunctionKind::Constructor(_) => rust_ident("new"),
        _ => rust_ident(function.name.rsplit('.').next().unwrap_or(&function.name)),
    };
    let parameters = function.params.iter().skip(usize::from(method)).map(|p| {
        let name = rust_ident(&p.name);
        let ty = values.ty(p.ty);
        quote! { #name: #ty }
    });
    let encoded = function.params.iter().skip(usize::from(method)).map(|p| {
        let name = rust_ident(&p.name);
        values.convert(p.ty, quote! { #name }, false)
    });
    let (result, decode) = match function.result {
        ::core::option::Option::Some(ty) => (
            values.ty(ty),
            values.convert(ty, quote! { __tembra_delivered.value }, true),
        ),
        ::core::option::Option::None => (quote! { () }, quote! { ::core::result::Result::Ok(()) }),
    };
    let (receiver, reference) = if method {
        (
            quote! { &self, },
            quote! { self.owned.as_ref().expect("a live owner") },
        )
    } else if import.view.is_some() {
        (quote! { &self, }, quote! { self.claim.reference() })
    } else {
        (
            quote! {},
            quote! { __tembra_call.imports.#field.reference() },
        )
    };
    let wit_name = &function.name;
    let docs = super::docs(function.docs.contents.as_deref());
    quote! {
        #(#[doc = #docs])*
        pub async fn #name(#receiver #(#parameters),*) -> #result {
            let __tembra_call = super::__tembra_import::current();
            let mut __tembra_payload = ::#facade::portable::Payload::data(::#facade::serde_json::Value::Null);
            let __tembra_arguments = (|| -> ::core::result::Result<::#facade::serde_json::Value, ::std::string::String> {
                ::core::result::Result::Ok(::#facade::serde_json::Value::Array(::std::vec![#(#encoded?),*]))
            })();
            let __tembra_answered = async {
                let __tembra_arguments = __tembra_arguments.map_err(|message| ::#facade::Error::invalid(::#facade::core::Reason::Arguments, message))?;
                let __tembra_input = __tembra_payload.map_value(|_| ::core::result::Result::Ok(__tembra_arguments))?;
                let __tembra_delivered = __tembra_call.issue(#reference.issue_portable::<_, ::#facade::portable::Delivered>(&__tembra_call.context, #wit_name, __tembra_input)).await?;
                if !__tembra_delivered.streams.is_empty() {
                    return ::core::result::Result::Err(::#facade::Error::invalid(::#facade::core::Reason::Result, "unexpected stream result"));
                }
                let mut __tembra_owned: ::std::vec::Vec<::core::option::Option<::#facade::portable::Owned>> = __tembra_delivered.owned.into_iter().map(::core::option::Option::Some).collect();
                let __tembra_decoded = (|| -> ::core::result::Result<#result, ::std::string::String> { #decode })()
                    .map_err(|message| ::#facade::Error::invalid(::#facade::core::Reason::Result, message))?;
                if __tembra_owned.iter().any(::core::option::Option::is_some) {
                    return ::core::result::Result::Err(::#facade::Error::invalid(::#facade::core::Reason::Result, "the result holds an unused resource"));
                }
                ::core::result::Result::Ok(__tembra_decoded)
            }.await;
            __tembra_call.answer(__tembra_answered).await
        }
    }
}
