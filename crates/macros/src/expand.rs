use proc_macro2::{Ident, Span, TokenStream, TokenTree};
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{
    FnArg, GenericArgument, ItemTrait, LitInt, LitStr, Pat, PathArguments, ReturnType, Token,
    TraitItem, Type, ext::IdentExt, parse::Parser, punctuated::Punctuated, spanned::Spanned,
};

#[path = "member.rs"]
mod member;
#[path = "options.rs"]
mod options;
#[path = "validation.rs"]
mod validation;

pub fn contract(attributes: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut cache_source = attributes.clone();
    cache_source.extend(item.clone());
    let descriptor_cache = fresh_identifier(&cache_source, "__TEMBRA_DESCRIPTOR_CACHE");
    let descriptor_slot = Ident::new("__tembra_descriptor_slot", Span::mixed_site());
    let descriptor_value = Ident::new("__tembra_cached_descriptor", Span::mixed_site());
    let descriptor_build = Ident::new("__tembra_build_descriptor", Span::mixed_site());
    let options = options::parse(attributes)?;
    let provider_type = fresh_provider_type(&item);
    // Mixed-site spans keep generated receiver ownership separate from user argument bindings.
    let receiver_local = Ident::new("__tembra_provider", Span::mixed_site());
    let capture_local = Ident::new("__tembra_capture", Span::mixed_site());
    let declaration: ItemTrait = syn::parse2(item)?;
    if !declaration.generics.params.is_empty()
        || declaration.generics.where_clause.is_some()
        || !declaration.supertraits.is_empty()
        || declaration.unsafety.is_some()
        || declaration.auto_token.is_some()
    {
        return ::std::result::Result::Err(syn::Error::new_spanned(
            &declaration,
            "contract traits cannot have generics, supertraits, unsafe or auto modifiers",
        ));
    }
    let name = options.name.ok_or_else(|| {
        syn::Error::new(declaration.ident.span(), "contract requires name = \"...\"")
    })?;
    let version = options.version.ok_or_else(|| {
        syn::Error::new(
            declaration.ident.span(),
            "contract requires version = integer",
        )
    })?;
    if version.base10_parse::<u64>()? == 0 {
        return ::std::result::Result::Err(syn::Error::new(
            version.span(),
            "version must be positive",
        ));
    }
    if !options.remote && (options.host.is_some() || options.variant.is_some()) {
        return ::std::result::Result::Err(syn::Error::new(
            name.span(),
            "host/variant require a remote contract",
        ));
    }
    let marker = &declaration.ident;
    let visibility = &declaration.vis;
    let attrs = &declaration.attrs;
    let provider = format_ident!("{}Provider", marker);
    let reference = format_ident!("{}Ref", marker);
    let input_name = format_ident!("{}Input", marker);
    let default_input = options.input.is_none();
    let input = options
        .input
        .unwrap_or_else(|| syn::parse_quote!(#input_name));
    let input_declaration = if default_input {
        quote! {
            #[derive(
                ::core::clone::Clone, ::core::fmt::Debug, ::core::default::Default,
                ::tembra::serde::Serialize, ::tembra::serde::Deserialize,
                ::tembra::schemars::JsonSchema,
            )]
            #[serde(crate = "::tembra::serde")]
            #[schemars(crate = "::tembra::schemars", description = "")]
            /// Acquisition input for this contract, with no declared input fields.
            #visibility struct #input_name {}
        }
    } else {
        quote!()
    };
    let variant = options
        .variant
        .clone()
        .unwrap_or_else(|| LitStr::new("main", name.span()));
    let mut builder = if options.remote {
        let host = options.host.ok_or_else(|| {
            syn::Error::new(name.span(), "remote contracts require host = \"...\"")
        })?;
        quote! {
            ::tembra::contracts::ContractBuilder::remote(#name, #version)
                .placement(::tembra::contracts::Placement::Named { name: #host.into() })
                .select_variant(#variant)
        }
    } else {
        quote!(::tembra::contracts::ContractBuilder::local(#name, #version))
    };
    builder = quote!(#builder.input::<#input>().map_err(__contract_error)?);
    let mut members = member::Members {
        builder,
        provider_methods: Vec::new(),
        reference_methods: Vec::new(),
        installations: Vec::new(),
        argument_types: Vec::new(),
    };
    let scope = member::Scope {
        remote: options.remote,
        marker,
        visibility,
        version: &version,
        receiver_local: &receiver_local,
        capture_local: &capture_local,
    };
    for item in &declaration.items {
        member::emit(item, &scope, &mut members)?;
    }
    let member::Members {
        builder,
        provider_methods,
        reference_methods,
        installations,
        argument_types,
    } = members;
    let variant_install = if options.remote {
        // The selected descriptor variant is the single provider shape for this declaration.
        quote!(let implementation = implementation.variant(#variant);)
    } else {
        quote!()
    };
    ::std::result::Result::Ok(quote! {
        #input_declaration
        #(#argument_types)*
        /// Contract token used for checked provider registration and reference acquisition.
        #(#attrs)*
        #[derive(
            ::core::clone::Clone, ::core::marker::Copy,
            ::core::fmt::Debug, ::core::default::Default,
        )]
        #visibility struct #marker;
        /// Provider interface; synchronous factories declare the managed implementation.
        #visibility trait #provider {
            #(#provider_methods)*
        }
        #[derive(::core::clone::Clone)]
        /// Typed consumer view over a core-managed claim.
        /// Calls resolve the installed implementation on each invocation.
        #visibility struct #reference {
            raw: ::tembra::core::Reference,
            context: ::std::option::Option<::tembra::core::Context>,
        }
        impl #reference {
            /// Checks the consumer token before wrapping an independently managed claim.
            /// # Errors
            /// Refuses a token with a different descriptor fingerprint without dispatching.
            #visibility fn from_reference(
                raw: ::tembra::core::Reference,
            ) -> ::tembra::Result<Self> {
                let descriptor = <#marker as ::tembra::ContractToken>::descriptor(&#marker)?;
                if raw.contract().fingerprint() != descriptor.fingerprint() {
                    return ::std::result::Result::Err(::tembra::RuntimeError::new(
                        ::tembra::core::ErrorCode::ContractMismatch,
                        "consumer_contract",
                        "reference consumer token differs from this typed contract",
                    ));
                }
                ::std::result::Result::Ok(Self { raw, context: ::std::option::Option::None })
            }
            /// Returns the managed reference that owns this wrapper's claim.
            #visibility fn core(&self) -> &::tembra::core::Reference { &self.raw }
            /// Selects the existing root and code version for subsequent method calls.
            #visibility fn with_context(&self, context: &::tembra::Context) -> Self {
                Self {
                    raw: self.raw.clone(),
                    context: ::std::option::Option::Some(context.core().clone()),
                }
            }
            #(#reference_methods)*
        }
        impl ::tembra::ContractToken for #marker {
            type Input = #input;
            type Reference = #reference;
            fn descriptor(&self) -> ::tembra::Result<::std::rc::Rc<::tembra::contracts::Contract>> {
                ::std::thread_local! {
                    static #descriptor_cache: ::std::cell::OnceCell<
                        ::std::rc::Rc<::tembra::contracts::Contract>,
                    > = const { ::std::cell::OnceCell::new() };
                }
                let #descriptor_build = || {
                    let __contract_error = |error: ::tembra::contracts::ContractError| {
                        ::tembra::RuntimeError::invalid(error.reason, error.message)
                    };
                    #builder.build().map(::std::rc::Rc::new).map_err(__contract_error)
                };
                #descriptor_cache.try_with(|#descriptor_slot| {
                    if let ::std::option::Option::Some(#descriptor_value) = #descriptor_slot.get() {
                        return ::std::result::Result::Ok(#descriptor_value.clone());
                    }
                    let #descriptor_value = #descriptor_build()?;
                    // Schema callbacks run before initialization; reentry retains first success.
                    ::std::result::Result::Ok(
                        #descriptor_slot.get_or_init(|| #descriptor_value).clone(),
                    )
                }).unwrap_or_else(|_| #descriptor_build())
            }
            fn bind(&self, raw: ::tembra::core::Reference) -> ::tembra::Result<Self::Reference> {
                #reference::from_reference(raw)
            }
        }
        impl<#provider_type: #provider + 'static>
            ::tembra::ProviderContract<#provider_type> for #marker
        {
            fn implementation(
                &self, provider: #provider_type,
            ) -> ::tembra::Result<::tembra::core::Implementation> {
                let #receiver_local = ::std::rc::Rc::new(provider);
                let implementation = ::tembra::core::Implementation::new()
                    .retain_owner(#receiver_local.clone());
                #(#installations)*
                #variant_install
                ::std::result::Result::Ok(implementation)
            }
        }
    })
}

fn result_type(output: &ReturnType) -> syn::Result<&Type> {
    let ReturnType::Type(_, ty) = output else {
        return ::std::result::Result::Err(syn::Error::new(
            output.span(),
            "async methods must return Result<T>",
        ));
    };
    if let Type::Path(path) = ty.as_ref()
        && let ::std::option::Option::Some(segment) = path.path.segments.last()
        && segment.ident == "Result"
        && let PathArguments::AngleBracketed(arguments) = &segment.arguments
        && arguments.args.len() == 1
        && let ::std::option::Option::Some(GenericArgument::Type(value)) = arguments.args.first()
    {
        return ::std::result::Result::Ok(value);
    }
    ::std::result::Result::Err(syn::Error::new_spanned(
        ty,
        "async methods must return Tembra Result<T>",
    ))
}

fn native_schema(ty: &TokenStream, version: &LitInt) -> TokenStream {
    quote!(::std::option::Option::Some(
        ::tembra::contracts::ValueSchema::Native(::std::format!(
            "{}@{}",
            ::std::any::type_name::<#ty>().chars()
                .filter(|character| !character.is_whitespace())
                .collect::<::std::string::String>(),
            #version,
        ))
    ))
}

fn pascal(name: &str) -> ::std::string::String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map_or_else(::std::string::String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
        })
        .collect()
}

fn fresh_provider_type(item: &TokenStream) -> Ident {
    fresh_identifier(item, "__TembraProvider")
}

fn fresh_identifier(item: &TokenStream, prefix: &str) -> Ident {
    fn collect(tokens: TokenStream, identifiers: &mut HashSet<::std::string::String>) {
        for token in tokens {
            match token {
                TokenTree::Ident(ident) => {
                    identifiers.insert(ident.unraw().to_string());
                }
                TokenTree::Group(group) => collect(group.stream(), identifiers),
                _ => {}
            }
        }
    }
    let mut identifiers = HashSet::new();
    collect(item.clone(), &mut identifiers);
    let mut index = 0;
    loop {
        let name = ::std::format!("{prefix}{index}");
        if !identifiers.contains(&name) {
            return Ident::new(&name, Span::mixed_site());
        }
        index += 1;
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
