//! `#[orb(package = "ns:name@1.0.0")]` turns a Rust module into a compiled Orb.
//!
//! The module is the source of truth, and every exported thing is marked. A unit struct with
//! `#[service]` and an `impl` block of `pub fn` becomes an exported interface whose functions are
//! the Orb's methods. A free `pub fn` with `#[contributes("ns:name/hook@version")]` contributes
//! to another Orb's hook. Structs with named fields and enums become `record`s, `enum`s and
//! `variant`s only when a service method uses them, directly or through another type; the rest
//! of the module stays ordinary Rust.
//!
//! The macro derives the WIT contract from those items and generates what a WIT-first crate
//! writes by hand: the `wit_bindgen::generate!` invocation with the WIT inline, the `Guest`
//! implementations forwarding to the author's functions, `export!`, and the `no_std` runtime
//! prelude for the Wasm target. The hook interfaces come from the provider's WIT, found in the
//! directory named by `wit = "dir"` on the attribute, relative to the crate manifest; their
//! generated modules are re-exported under their own names, so the author writes
//! `guard::Admission`.
//!
//! The crate using the macro must depend on `wit-bindgen` (features `macros` and `realloc`) and
//! `dlmalloc` (feature `global`), because the generated code names them. `cargo tembra build`
//! compiles the crate, reads the same WIT from the component and packages the artifact.
mod wit;

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    Attribute, Fields, FnArg, Ident, Item, ItemFn, ItemMod, LitByteStr, LitStr, Pat, ReturnType,
    Type, Visibility, parse_macro_input, spanned::Spanned,
};

/// Declare a compiled Orb from a module; the crate documentation lists the rules.
#[proc_macro_attribute]
pub fn orb(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let mut options = Options::default();
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("package") {
            options.package = Some(meta.value()?.parse::<LitStr>()?.value());
        } else if meta.path.is_ident("wit") {
            options.wit = Some(meta.value()?.parse::<LitStr>()?.value());
        } else {
            return Err(meta
                .error("unknown option; use #[orb(package = \"ns:name@1.0.0\", wit = \"dir\")]"));
        }
        Ok(())
    });
    parse_macro_input!(attribute with parser);
    let module = parse_macro_input!(item as ItemMod);
    match expand(options, module) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// The attribute options.
#[derive(Default)]
struct Options {
    /// The WIT package identity of this Orb.
    package: Option<String>,
    /// A directory with the WIT packages this Orb consumes, relative to the crate manifest.
    wit: Option<String>,
}

/// A type declared in the module, with its WIT definition and the module types it refers to.
struct Shape {
    ident: Ident,
    wit: String,
    references: Vec<Ident>,
}

/// An exported interface: a unit struct marked `#[service]` with an `impl` block of `pub fn`.
struct Service {
    ident: Ident,
    docs: Vec<String>,
    functions: Vec<Function>,
}

/// One exported function, with the WIT spelling of its parameter and result types.
struct Function {
    ident: Ident,
    docs: Vec<String>,
    params: Vec<(Ident, Type, String)>,
    result: Option<(Type, String)>,
}

/// A hook contribution: a free `pub fn` marked `#[contributes("ns:name/hook@version")]`.
struct Contribution {
    hook: String,
    ident: Ident,
    param: (Ident, Type),
    result: Option<Type>,
}

fn expand(options: Options, module: ItemMod) -> syn::Result<proc_macro2::TokenStream> {
    let package = options
        .package
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

    // Read the module: marked services and contributions, candidate types, and everything
    // else to copy through unchanged.
    let mut candidates: Vec<(Item, Shape)> = Vec::new();
    let mut services = Vec::new();
    let mut contributions = Vec::new();
    let mut passthrough = Vec::new();
    let mut impls = Vec::new();
    for item in items {
        match item {
            Item::Struct(mut s) => {
                if let Some(attribute) = take(&mut s.attrs, "state") {
                    return Err(syn::Error::new(
                        attribute.span(),
                        "kernel-managed state is not available to compiled Orbs yet; keep state in statics",
                    ));
                }
                if let Some(attribute) = take(&mut s.attrs, "service") {
                    if !matches!(s.fields, Fields::Unit) {
                        return Err(syn::Error::new(
                            attribute.span(),
                            "a service is a unit struct: its methods are the pub fn of its impl blocks",
                        ));
                    }
                    services.push(Service {
                        ident: s.ident.clone(),
                        docs: wit::docs(&s.attrs),
                        functions: Vec::new(),
                    });
                    passthrough.push(Item::Struct(s));
                } else if matches!(s.fields, Fields::Named(_)) {
                    let shape = wit::record(&s)?;
                    candidates.push((Item::Struct(s), shape));
                } else {
                    passthrough.push(Item::Struct(s));
                }
            }
            Item::Enum(e) => {
                let shape = wit::enumeration(&e)?;
                candidates.push((Item::Enum(e), shape));
            }
            Item::Impl(i) if i.trait_.is_none() => {
                if let Some(attribute) = i.attrs.iter().find(|a| a.path().is_ident("service")) {
                    return Err(syn::Error::new(
                        attribute.span(),
                        "put #[service] on the unit struct, not on its impl block",
                    ));
                }
                impls.push(i.clone());
                passthrough.push(Item::Impl(i));
            }
            Item::Fn(mut function) => {
                if let Some(attribute) = take(&mut function.attrs, "contributes") {
                    let hook: LitStr = attribute.parse_args()?;
                    contributions.push(contribution(&function, &hook)?);
                }
                passthrough.push(Item::Fn(function));
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
    if services.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "no service found: mark a unit struct with #[service] and give it an impl block of pub fn",
        ));
    }
    if let Some(service) = services.iter().find(|service| service.functions.is_empty()) {
        return Err(syn::Error::new(
            service.ident.span(),
            "a service needs at least one pub fn in an impl block",
        ));
    }
    let hooks: BTreeSet<String> = contributions
        .iter()
        .map(|contribution| contribution.hook.clone())
        .collect();
    if hooks.len() != contributions.len() {
        return Err(syn::Error::new(
            Span::call_site(),
            "one contribution per hook",
        ));
    }

    // Only the types a service method uses, directly or through another type, enter the WIT.
    let shapes = reachable(candidates, &services, &mut passthrough)?;

    // Consumed packages: the hook callbacks are read from the provider's WIT.
    let wit_dir = options.wit.as_deref().map(wit::absolute).transpose()?;
    let callbacks = match (&wit_dir, hooks.is_empty()) {
        (_, true) => Default::default(),
        (Some(dir), false) => wit::callbacks(dir, &hooks)?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "contributions need wit = \"<dir>\", the directory with the provider's WIT package",
            ));
        }
    };
    let wit = wit::render(&package, &world, &shapes, &services, &hooks);

    // Generate the Rust side: the bindings from the inline WIT, the author's types replaced by
    // the generated ones, the bridges to the author's functions, and the no_std prelude.
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
    // Each hook's generated module is re-exported under its interface name, and its callback
    // forwards to the author's function.
    let hook_modules = hooks.iter().map(|hook| {
        let [ns, package, interface] = hook_path(hook);
        quote! { pub use exports::#ns::#package::#interface; }
    });
    let contribution_bridges = contributions.iter().map(|contribution| {
        let [ns, package, interface] = hook_path(&contribution.hook);
        let callback = format_ident!("{}", callbacks[&contribution.hook].to_snake_case());
        let target = &contribution.ident;
        let (param, ty) = &contribution.param;
        let result = contribution
            .result
            .as_ref()
            .map(|ty| quote! { -> #ty })
            .unwrap_or_default();
        quote! {
            impl exports::#ns::#package::#interface::Guest for __TembraExports {
                fn #callback(#param: #ty) #result { #target(#param) }
            }
        }
    });
    let wit_literal = LitStr::new(&wit, Span::call_site());
    let wit_bytes = LitByteStr::new(wit.as_bytes(), Span::call_site());
    let wit_len = proc_macro2::Literal::usize_unsuffixed(wit.len());
    let world_literal = LitStr::new(&world, Span::call_site());
    let generate = match &wit_dir {
        Some(dir) => {
            let dir = LitStr::new(&dir.display().to_string(), Span::call_site());
            // Foreign interfaces need `generate_all`, or wit-bindgen asks for a `with` mapping.
            quote! { ::wit_bindgen::generate!({ path: #dir, inline: #wit_literal, world: #world_literal, generate_all }); }
        }
        None => {
            quote! { ::wit_bindgen::generate!({ inline: #wit_literal, world: #world_literal }); }
        }
    };
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

            // The exact WIT of this module travels inside the component as a custom section, so
            // `cargo tembra build` packages the contract as written here: hook imports the
            // linker drops and `///` comments the binary type section loses.
            #[cfg(target_arch = "wasm32")]
            #[unsafe(link_section = "tembra:wit")]
            #[used]
            static __TEMBRA_WIT: [u8; #wit_len] = *#wit_bytes;

            #generate

            #reexports
            #(#hook_modules)*

            #(#passthrough)*

            struct __TembraExports;
            #(#bridges)*
            #(#contribution_bridges)*
            export!(__TembraExports);
        }
    })
}

/// Remove and return the marker attribute with the given name, if present.
fn take(attrs: &mut Vec<Attribute>, name: &str) -> Option<Attribute> {
    let index = attrs.iter().position(|attr| attr.path().is_ident(name))?;
    Some(attrs.remove(index))
}

/// Keep, in module order, the candidate types that a service method reaches; the others go back
/// to the module as ordinary Rust. A reference to a type the module does not declare is an error.
fn reachable(
    candidates: Vec<(Item, Shape)>,
    services: &[Service],
    passthrough: &mut Vec<Item>,
) -> syn::Result<Vec<Shape>> {
    let declared: BTreeMap<String, usize> = candidates
        .iter()
        .enumerate()
        .map(|(index, (_, shape))| (shape.ident.to_string(), index))
        .collect();
    let mut pending: Vec<Ident> = Vec::new();
    for service in services {
        for function in &service.functions {
            for (_, ty, _) in &function.params {
                wit::references(ty, &mut pending);
            }
            if let Some((ty, _)) = &function.result {
                wit::references(ty, &mut pending);
            }
        }
    }
    let mut kept = BTreeSet::new();
    while let Some(ident) = pending.pop() {
        let Some(&index) = declared.get(&ident.to_string()) else {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{ident}` is used by a service method but is not a struct or enum of this module"
                ),
            ));
        };
        if kept.insert(index) {
            pending.extend(candidates[index].1.references.iter().cloned());
        }
    }
    let mut shapes = Vec::new();
    for (index, (item, shape)) in candidates.into_iter().enumerate() {
        if kept.contains(&index) {
            shapes.push(shape);
        } else {
            passthrough.push(item);
        }
    }
    Ok(shapes)
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
    Ok(Function {
        ident: function.sig.ident.clone(),
        docs: wit::docs(&function.attrs),
        params,
        result: match result(&function.sig.output) {
            Some(ty) => Some((ty.clone(), wit::wit_type(ty)?)),
            None => None,
        },
    })
}

/// Read a hook contribution: a `pub fn` taking the hook input and returning its output.
fn contribution(function: &ItemFn, hook: &LitStr) -> syn::Result<Contribution> {
    let id = hook.value();
    if !id.contains(':') || !id.contains('/') {
        return Err(syn::Error::new(
            hook.span(),
            "a hook is named ns:name/interface@version",
        ));
    }
    if !matches!(function.vis, Visibility::Public(_)) {
        return Err(syn::Error::new(
            function.sig.span(),
            "a contribution is a pub fn",
        ));
    }
    let mut inputs = function.sig.inputs.iter();
    let (Some(FnArg::Typed(typed)), None) = (inputs.next(), inputs.next()) else {
        return Err(syn::Error::new(
            function.sig.span(),
            "a hook callback takes exactly one parameter, the hook input",
        ));
    };
    let Pat::Ident(pat) = &*typed.pat else {
        return Err(syn::Error::new(
            typed.span(),
            "parameters need a plain name",
        ));
    };
    Ok(Contribution {
        hook: id,
        ident: function.sig.ident.clone(),
        param: (pat.ident.clone(), (*typed.ty).clone()),
        result: result(&function.sig.output).cloned(),
    })
}

/// The declared result type, with `()` read as no result.
fn result(output: &ReturnType) -> Option<&Type> {
    match output {
        ReturnType::Default => None,
        ReturnType::Type(_, ty) => match &**ty {
            Type::Tuple(tuple) if tuple.elems.is_empty() => None,
            other => Some(other),
        },
    }
}

/// The generated module of a hook interface: `exports::<ns>::<package>::<interface>`.
fn hook_path(hook: &str) -> [Ident; 3] {
    let (package, rest) = hook.split_once('/').unwrap_or((hook, ""));
    let (namespace, name) = package.split_once(':').unwrap_or((package, ""));
    let interface = rest.split('@').next().unwrap_or(rest);
    [namespace, name, interface].map(|part| format_ident!("{}", part.to_snake_case()))
}
