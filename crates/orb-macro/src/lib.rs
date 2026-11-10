//! `#[orb(package = "ns:name@1.0.0")]` turns a Rust module into a compiled Orb.
//!
//! The module is the source of truth, and every exported thing is marked. A unit struct with
//! `#[service]` and an `impl` block of `pub fn` becomes an exported interface whose functions are
//! the Orb's methods. A free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`
//! participates in another Orb's hook. A struct with named fields and `#[state]` is the
//! kernel-managed state of a service's objects. Structs with named fields and enums become
//! `record`s, `enum`s and `variant`s only when a service method or a state uses them, directly
//! or through another type; the rest of the module stays ordinary Rust.
//!
//! The macro derives the WIT contract from those items and generates what a WIT-first crate
//! writes by hand: the `wit_bindgen::generate!` invocation with the WIT inline, the `Guest`
//! implementations forwarding to the author's functions, `export!`, and the `no_std` runtime
//! prelude for the Wasm target. The hook interfaces come from the provider's WIT, found in the
//! directory named by `wit = "dir"` on the attribute, relative to the crate manifest; their
//! generated modules are re-exported under their own names, so the author writes
//! `guard::Admission`. The kernel's `tembra:guest` package is bundled with the macro.
//!
//! The crate using the macro must depend on `wit-bindgen` (features `macros` and `realloc`) and
//! `dlmalloc` (feature `global`), because the generated code names them; a crate with a state
//! also depends on `serde` (`derive`, `alloc`) and `serde_json` (`alloc`). `cargo tembra build`
//! compiles the crate, reads the same WIT and the hook priorities from the component and
//! packages the artifact.
mod wit;

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    Attribute, Fields, FnArg, Ident, Item, ItemFn, ItemMod, LitByteStr, LitInt, LitStr, Pat,
    ReturnType, Token, Type, Visibility, parse_macro_input, spanned::Spanned,
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

/// A hook contribution: a free `pub fn` marked `#[hook("ns:name/hook@version", priority = n)]`.
struct Contribution {
    hook: String,
    priority: i32,
    ident: Ident,
    param: (Ident, Type),
    result: Option<Type>,
}

/// A kernel-managed state: a struct with named fields marked `#[state]`, owned by the objects
/// of one service.
struct State {
    ident: Ident,
    fields: Vec<Ident>,
    /// The owning service, as named by `service = ...` or resolved to the module's only one.
    service: Option<Ident>,
    version: u32,
}

fn expand(options: Options, module: ItemMod) -> syn::Result<proc_macro2::TokenStream> {
    let package = options
        .package
        .ok_or_else(|| syn::Error::new(module.span(), "missing package = \"ns:name@1.0.0\""))?;
    let (namespace, rest) = package
        .split_once(':')
        .ok_or_else(|| syn::Error::new(module.span(), "package must be ns:name@version"))?;
    let (name, version) = match rest.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (rest, None),
    };
    let world = wit::kebab(&module.ident);
    let Some((_, items)) = module.content else {
        return Err(syn::Error::new(
            module.span(),
            "the module needs a body { ... }",
        ));
    };

    // Read the module: marked services, contributions and states, candidate types, and
    // everything else to copy through unchanged.
    let mut candidates: Vec<(Item, Shape)> = Vec::new();
    let mut services = Vec::new();
    let mut contributions = Vec::new();
    let mut states = Vec::new();
    let mut passthrough = Vec::new();
    let mut impls = Vec::new();
    for item in items {
        match item {
            Item::Struct(mut s) => {
                if let Some(attribute) = take(&mut s.attrs, "state") {
                    states.push(state(&s, &attribute)?);
                    let shape = wit::record(&s)?;
                    candidates.push((Item::Struct(s), shape));
                } else if let Some(attribute) = take(&mut s.attrs, "service") {
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
                    return Err(syn::Error::new(
                        attribute.span(),
                        "#[contributes] is now #[hook(\"ns:name/hook@version\", priority = n)]",
                    ));
                }
                if let Some(attribute) = take(&mut function.attrs, "hook") {
                    contributions.push(contribution(&function, &attribute)?);
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
    // The module names the world; WIT refuses a world and an interface with one name.
    if let Some(service) = services
        .iter()
        .find(|service| wit::kebab(&service.ident) == world)
    {
        return Err(syn::Error::new(
            service.ident.span(),
            format!(
                "`{}` is also the name of the world, taken from the module; rename one of them",
                service.ident
            ),
        ));
    }
    let hooks: BTreeMap<String, i32> = contributions
        .iter()
        .map(|contribution| (contribution.hook.clone(), contribution.priority))
        .collect();
    if hooks.len() != contributions.len() {
        return Err(syn::Error::new(
            Span::call_site(),
            "one contribution per hook",
        ));
    }
    // Each state belongs to the objects of one service, and an object declares exactly one state.
    let mut owners = BTreeMap::new();
    for state in &mut states {
        let owner = match &state.service {
            Some(ident) => services
                .iter()
                .find(|service| &service.ident == ident)
                .ok_or_else(|| {
                    syn::Error::new(
                        ident.span(),
                        format!("`{ident}` is not a #[service] of this module"),
                    )
                })?,
            None if services.len() == 1 => &services[0],
            None => {
                return Err(syn::Error::new(
                    state.ident.span(),
                    "name the service whose objects own this state: #[state(service = Name)]",
                ));
            }
        };
        if let Some(previous) = owners.insert(owner.ident.to_string(), state.ident.to_string()) {
            return Err(syn::Error::new(
                state.ident.span(),
                format!(
                    "`{}` already owns the state `{previous}`; an object declares exactly one state, so group the fields in one struct",
                    owner.ident
                ),
            ));
        }
        state.service = Some(owner.ident.clone());
    }

    // Only the types a service method or a state uses, directly or through another type, enter
    // the WIT.
    let shapes = reachable(candidates, &services, &states, &mut passthrough)?;

    // Consumed packages: the hook callbacks are read from the provider's WIT.
    let wit_dir = options.wit.as_deref().map(wit::absolute).transpose()?;
    let callbacks = match (&wit_dir, hooks.is_empty()) {
        (_, true) => Default::default(),
        (Some(dir), false) => wit::callbacks(dir, hooks.keys())?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "contributions need wit = \"<dir>\", the directory with the provider's WIT package",
            ));
        }
    };
    let wit = wit::render(
        &package,
        &world,
        &shapes,
        &services,
        hooks.keys(),
        !states.is_empty(),
    );

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
    let hook_modules = hooks.keys().map(|hook| {
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
    // The priorities travel with the component, like the WIT.
    let hooks_section = if hooks.is_empty() {
        quote! {}
    } else {
        let json = format!(
            "{{{}}}",
            hooks
                .iter()
                .map(|(hook, priority)| format!("\"{hook}\":{priority}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let bytes = LitByteStr::new(json.as_bytes(), Span::call_site());
        let len = proc_macro2::Literal::usize_unsuffixed(json.len());
        quote! {
            #[cfg(target_arch = "wasm32")]
            #[unsafe(link_section = "tembra:hooks")]
            #[used]
            static __TEMBRA_HOOKS: [u8; #len] = *#bytes;
        }
    };
    let state_code = state_code(&states, &package, name, version);
    let wit_literal = LitStr::new(&wit, Span::call_site());
    let wit_bytes = LitByteStr::new(wit.as_bytes(), Span::call_site());
    let wit_len = proc_macro2::Literal::usize_unsuffixed(wit.len());
    let world_literal = LitStr::new(&world, Span::call_site());
    // The bindings read the author's WIT and, nested in it, the kernel package a state uses; the
    // hook packages come from the consumed directory. Foreign interfaces need `generate_all`.
    let inline = if states.is_empty() {
        wit_literal.clone()
    } else {
        LitStr::new(
            &format!("{wit}\n{}", wit::guest_package()),
            Span::call_site(),
        )
    };
    let path = wit_dir.as_ref().map(|dir| {
        let dir = LitStr::new(&dir.display().to_string(), Span::call_site());
        quote! { path: #dir, }
    });
    let derives = (!states.is_empty())
        .then(|| quote! { additional_derives: [::serde::Serialize, ::serde::Deserialize], });
    let generate = quote! {
        ::wit_bindgen::generate!({
            #path
            inline: #inline,
            world: #world_literal,
            generate_all,
            #derives
        });
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

            #hooks_section

            #generate

            #reexports
            #(#hook_modules)*

            #(#passthrough)*

            struct __TembraExports;
            #(#bridges)*
            #(#contribution_bridges)*
            #state_code
            export!(__TembraExports);
        }
    })
}

/// Remove and return the marker attribute with the given name, if present.
fn take(attrs: &mut Vec<Attribute>, name: &str) -> Option<Attribute> {
    let index = attrs.iter().position(|attr| attr.path().is_ident(name))?;
    Some(attrs.remove(index))
}

/// Keep, in module order, the candidate types that a service method or a state reaches; the
/// others go back to the module as ordinary Rust. A reference to a type the module does not
/// declare is an error.
fn reachable(
    candidates: Vec<(Item, Shape)>,
    services: &[Service],
    states: &[State],
    passthrough: &mut Vec<Item>,
) -> syn::Result<Vec<Shape>> {
    let declared: BTreeMap<String, usize> = candidates
        .iter()
        .enumerate()
        .map(|(index, (_, shape))| (shape.ident.to_string(), index))
        .collect();
    let mut pending: Vec<Ident> = states.iter().map(|state| state.ident.clone()).collect();
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

/// Read a hook contribution: a `pub fn` taking the hook input and returning its output, with the
/// hook's name and an optional priority (default 0) on the attribute.
fn contribution(function: &ItemFn, attribute: &Attribute) -> syn::Result<Contribution> {
    let (hook, priority) = attribute.parse_args_with(|input: syn::parse::ParseStream| {
        let hook: LitStr = input.parse()?;
        let mut priority = 0;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            let key: Ident = input.parse()?;
            if key != "priority" {
                return Err(syn::Error::new(
                    key.span(),
                    "unknown option; use #[hook(\"ns:name/hook@version\", priority = n)]",
                ));
            }
            input.parse::<Token![=]>()?;
            let negative = input.parse::<Option<Token![-]>>()?.is_some();
            let value: i32 = input.parse::<LitInt>()?.base10_parse()?;
            priority = if negative { -value } else { value };
        }
        Ok((hook, priority))
    })?;
    let id = hook.value();
    if !id.contains(':') || !id.contains('/') || id.contains(['"', '\\']) {
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
        priority,
        ident: function.sig.ident.clone(),
        param: (pat.ident.clone(), (*typed.ty).clone()),
        result: result(&function.sig.output).cloned(),
    })
}

/// Read a state: a struct with named fields, the owning service and the version (default 1).
fn state(item: &syn::ItemStruct, attribute: &Attribute) -> syn::Result<State> {
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new(
            attribute.span(),
            "a state is a struct with named fields",
        ));
    };
    let mut service = None;
    let mut version = 1;
    if !matches!(attribute.meta, syn::Meta::Path(_)) {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("service") {
                service = Some(meta.value()?.parse::<Ident>()?);
            } else if meta.path.is_ident("version") {
                let literal: LitInt = meta.value()?.parse()?;
                version = literal.base10_parse::<u32>()?;
                if version == 0 {
                    return Err(syn::Error::new(literal.span(), "versions start at 1"));
                }
            } else {
                return Err(meta.error("unknown option; use #[state(service = Name, version = n)]"));
            }
            Ok(())
        })?;
    }
    Ok(State {
        ident: item.ident.clone(),
        fields: fields
            .named
            .iter()
            .filter_map(|field| field.ident.clone())
            .collect(),
        service,
        version,
    })
}

/// The Rust side of the states: the kernel's `state-init` callback answering one declaration per
/// owning service, the accessors on each state type, and the JSON helpers they share.
fn state_code(
    states: &[State],
    package: &str,
    name: &str,
    version: Option<&str>,
) -> proc_macro2::TokenStream {
    if states.is_empty() {
        return quote! {};
    }
    let package_name = package.split('@').next().unwrap_or(package);
    let _ = name;
    let declarations = states.iter().map(|state| {
        let service = state.service.as_ref().expect("resolved owner");
        let identity = match version {
            Some(version) => format!("{package_name}/{}@{version}", wit::kebab(service)),
            None => format!("{package_name}/{}", wit::kebab(service)),
        };
        let identity = LitStr::new(&identity, Span::call_site());
        let ident = &state.ident;
        let version = state.version;
        quote! {
            #identity => Some(exports::tembra::guest::state_init::Declaration {
                initial: __tembra_state::to_json(&<#ident as ::core::default::Default>::default()),
                version: #version,
            }),
        }
    });
    let accessors = states.iter().map(|state| {
        let ident = &state.ident;
        let fields = &state.fields;
        let service = state.service.as_ref().expect("resolved owner").to_string();
        let read_doc = format!(
            " The state of the `{service}` object running the current call, read from the kernel."
        );
        let update_doc = format!(
            " Change the state of the `{service}` object running the current call and store it in the kernel; returns the stored value."
        );
        quote! {
            impl ::core::default::Default for #ident {
                fn default() -> Self {
                    Self { #(#fields: ::core::default::Default::default()),* }
                }
            }
            impl #ident {
                #[doc = #read_doc]
                pub fn read() -> Self {
                    __tembra_state::read()
                }
                #[doc = #update_doc]
                pub fn update(change: impl FnOnce(&mut Self)) -> Self {
                    let mut value = Self::read();
                    change(&mut value);
                    __tembra_state::update(&value)
                }
            }
        }
    });
    quote! {
        // The kernel asks for each service object's state when it creates the object.
        impl exports::tembra::guest::state_init::Guest for __TembraExports {
            fn declare(
                service: __tembra_state::String,
            ) -> Option<exports::tembra::guest::state_init::Declaration> {
                match service.as_str() {
                    #(#declarations)*
                    _ => None,
                }
            }
        }
        #(#accessors)*
        /// The kernel keeps a state as JSON; serde maps it to the state type and back.
        #[doc(hidden)]
        pub mod __tembra_state {
            extern crate alloc;
            pub use alloc::string::String;

            pub fn read<T: ::serde::de::DeserializeOwned>() -> T {
                parse(&super::tembra::guest::state::read())
            }

            pub fn update<T: ::serde::Serialize + ::serde::de::DeserializeOwned>(value: &T) -> T {
                parse(&super::tembra::guest::state::update(&to_json(value)))
            }

            pub fn to_json<T: ::serde::Serialize>(value: &T) -> String {
                ::serde_json::to_string(value).expect("a state serializes to JSON")
            }

            fn parse<T: ::serde::de::DeserializeOwned>(text: &str) -> T {
                ::serde_json::from_str(text).expect("the kernel state has the shape of this type")
            }
        }
    }
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
