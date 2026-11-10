//! `#[orb(package = "ns:name@1.0.0")]` turns a Rust module into a compiled Orb.
//!
//! The module is the source of truth. The macro reads it and derives the WIT contract: structs
//! with named fields become `record`s, enums become `enum`s or `variant`s, and every unit struct
//! with an `impl` block of `pub fn` becomes an exported interface whose functions are the Orb's
//! methods. It then generates what a WIT-first crate writes by hand: the `wit_bindgen::generate!`
//! invocation with the WIT inline, the `Guest` implementations forwarding to the author's
//! functions, `export!`, and the `no_std` runtime prelude for the Wasm target.
//!
//! The crate using the macro must depend on `wit-bindgen` (features `macros` and `realloc`) and
//! `dlmalloc` (feature `global`), because the generated code names them. `cargo tembra build`
//! compiles the crate, extracts the same WIT from the component and packages the artifact.
mod wit;

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    Fields, FnArg, Ident, Item, ItemMod, LitStr, Pat, ReturnType, Type, Visibility,
    parse_macro_input, spanned::Spanned,
};

/// Declare a compiled Orb from a module; the crate documentation lists the rules.
#[proc_macro_attribute]
pub fn orb(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let mut package = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("package") {
            package = Some(meta.value()?.parse::<LitStr>()?.value());
            Ok(())
        } else {
            Err(meta.error("unknown attribute; use #[orb(package = \"ns:name@1.0.0\")]"))
        }
    });
    parse_macro_input!(attribute with parser);
    let module = parse_macro_input!(item as ItemMod);
    match expand(package, module) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// A type declared in the module, with its WIT definition.
struct Shape {
    ident: Ident,
    wit: String,
}

/// An exported interface: a unit struct with an `impl` block of public functions.
struct Service {
    ident: Ident,
    functions: Vec<Function>,
}

/// One exported function, with the WIT spelling of its parameter and result types.
struct Function {
    ident: Ident,
    docs: Vec<String>,
    params: Vec<(Ident, Type, String)>,
    result: Option<(Type, String)>,
}

fn expand(package: Option<String>, module: ItemMod) -> syn::Result<proc_macro2::TokenStream> {
    let package = package
        .ok_or_else(|| syn::Error::new(module.span(), "missing package = \"ns:name@1.0.0\""))?;
    let (namespace, rest) = package
        .split_once(':')
        .ok_or_else(|| syn::Error::new(module.span(), "package must be ns:name@version"))?;
    let name = rest.split('@').next().unwrap_or(rest);
    let world = wit::kebab(&module.ident);
    let Some((_, items)) = module.content else {
        return Err(syn::Error::new(
            module.span(),
            "the module needs a body { ... }",
        ));
    };

    // Read the module: types, services, and everything else to copy through unchanged.
    let mut shapes = Vec::new();
    let mut services = Vec::new();
    let mut passthrough = Vec::new();
    let mut impls = Vec::new();
    for item in items {
        match item {
            Item::Struct(ref s) if matches!(s.fields, Fields::Named(_)) => {
                shapes.push(wit::record(s)?);
            }
            Item::Struct(ref s) if matches!(s.fields, Fields::Unit) => {
                services.push(Service {
                    ident: s.ident.clone(),
                    functions: Vec::new(),
                });
                passthrough.push(item);
            }
            Item::Enum(ref e) => shapes.push(wit::enumeration(e)?),
            Item::Impl(ref i) if i.trait_.is_none() => {
                impls.push(i.clone());
                passthrough.push(item);
            }
            other => passthrough.push(other),
        }
    }
    for item in &impls {
        let Type::Path(path) = &*item.self_ty else {
            continue;
        };
        let Some(service) = services
            .iter_mut()
            .find(|service| path.path.is_ident(&service.ident))
        else {
            continue;
        };
        for impl_item in &item.items {
            let syn::ImplItem::Fn(function) = impl_item else {
                continue;
            };
            if matches!(function.vis, Visibility::Public(_)) {
                service.functions.push(method(function)?);
            }
        }
    }
    services.retain(|service| !service.functions.is_empty());
    if services.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "no service found: declare a unit struct with an impl block of pub fn",
        ));
    }
    let wit = wit::render(&package, &world, &shapes, &services);

    // Generate the Rust side: the bindings from the inline WIT, the author's types replaced by
    // the generated ones, the bridge to the author's functions, and the no_std prelude.
    let module_ident = module.ident;
    let vis = module.vis;
    let attrs = module.attrs;
    let ns_ident = format_ident!("{}", namespace.to_snake_case());
    let name_ident = format_ident!("{}", name.to_snake_case());
    let first_interface = format_ident!("{}", services[0].ident.to_string().to_snake_case());
    let shape_idents = shapes.iter().map(|shape| &shape.ident).collect::<Vec<_>>();
    // The generated types carry the author's names and public fields, so the author's functions
    // and the generated traits speak about one type.
    let reexports = if shapes.is_empty() {
        quote! {}
    } else {
        quote! { pub use exports::#ns_ident::#name_ident::#first_interface::{#(#shape_idents),*}; }
    };
    let bridges = services.iter().map(|service| {
        let interface = format_ident!("{}", service.ident.to_string().to_snake_case());
        let target = &service.ident;
        let functions = service.functions.iter().map(|function| {
            let ident = &function.ident;
            let params = function
                .params
                .iter()
                .map(|(ident, ty, _)| quote! { #ident: #ty });
            let names = function.params.iter().map(|(ident, _, _)| ident);
            let result = function
                .result
                .as_ref()
                .map(|(ty, _)| quote! { -> #ty })
                .unwrap_or_default();
            quote! {
                fn #ident(#(#params),*) #result { #target::#ident(#(#names),*) }
            }
        });
        quote! {
            impl exports::#ns_ident::#name_ident::#interface::Guest for __TembraExports {
                #(#functions)*
            }
        }
    });
    let wit_literal = LitStr::new(&wit, Span::call_site());
    let world_literal = LitStr::new(&world, Span::call_site());
    Ok(quote! {
        #(#attrs)*
        #vis mod #module_ident {
            /// The WIT derived from this module.
            pub const WIT: &str = #wit_literal;

            // The runtime prelude of a `no_std` component. On the host the module builds with std,
            // so `cargo check` and rust-analyzer need no Wasm target.
            #[cfg(target_arch = "wasm32")]
            #[global_allocator]
            static __TEMBRA_ALLOCATOR: ::dlmalloc::GlobalDlmalloc = ::dlmalloc::GlobalDlmalloc;

            #[cfg(target_arch = "wasm32")]
            #[panic_handler]
            fn __tembra_panic(_: &::core::panic::PanicInfo<'_>) -> ! {
                ::core::arch::wasm32::unreachable()
            }

            ::wit_bindgen::generate!({ inline: #wit_literal, world: #world_literal });

            #reexports

            #(#passthrough)*

            struct __TembraExports;
            #(#bridges)*
            export!(__TembraExports);
        }
    })
}

/// Read one public function of a service: a plain name for each owned parameter, no receiver.
fn method(function: &syn::ImplItemFn) -> syn::Result<Function> {
    let mut params = Vec::new();
    for input in &function.sig.inputs {
        match input {
            FnArg::Receiver(receiver) => {
                return Err(syn::Error::new(
                    receiver.span(),
                    "WIT functions have no receiver; remove self",
                ));
            }
            FnArg::Typed(typed) => {
                let Pat::Ident(pat) = &*typed.pat else {
                    return Err(syn::Error::new(
                        typed.span(),
                        "parameters need a plain name",
                    ));
                };
                params.push((
                    pat.ident.clone(),
                    (*typed.ty).clone(),
                    wit::wit_type(&typed.ty)?,
                ));
            }
        }
    }
    let result = match &function.sig.output {
        ReturnType::Default => None,
        ReturnType::Type(_, ty) => match &**ty {
            Type::Tuple(tuple) if tuple.elems.is_empty() => None,
            other => Some(((**ty).clone(), wit::wit_type(other)?)),
        },
    };
    Ok(Function {
        ident: function.sig.ident.clone(),
        docs: wit::docs(&function.attrs),
        params,
        result,
    })
}
