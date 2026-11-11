//! `#[orb(package = "ns:name@1.0.0")]` turns a Rust module into a compiled Orb.
//!
//! The module is the source of truth, and every exported thing is marked. A unit struct with
//! `#[service]` and an `impl` block of `pub fn` becomes an exported interface whose functions are
//! the Orb's methods. A free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`
//! participates in another Orb's hook. A struct with named fields and `#[state]` is the
//! kernel-managed state of a service's objects. A struct with `#[resource]` and a `pub fn open()`
//! is a resource of the Orb's root, opened by the kernel at activation and usable by every
//! method and hook callback. Structs with named fields and enums become `record`s, `enum`s and
//! `variant`s only when a service method or a state uses them, directly or through another type;
//! the rest of the module stays ordinary Rust.
//!
//! The macro derives the WIT contract from those items and generates what a WIT-first crate
//! writes by hand: the `wit_bindgen::generate!` invocation with the WIT inline, the `Guest`
//! implementations forwarding to the author's functions, `export!`, and the `no_std` runtime
//! prelude for the Wasm target. The hook interfaces come from the provider's WIT, found in the
//! directory named by `wit = "dir"` on the attribute, relative to the crate manifest; their
//! generated modules are re-exported under their own names, so the author writes
//! `guard::Admission`. The kernel's `tembra:guest` package is bundled with the macro; with
//! `context` on the attribute the module imports its `context` interface and reads the call's
//! principal and cancellation, the object's address and the host's log as `context::...`.
//!
//! The crate using the macro must depend on `wit-bindgen` (features `macros` and `realloc`) and
//! `dlmalloc` (feature `global`), because the generated code names them; a crate with a state
//! also depends on `serde` (`derive`, `alloc`) and `serde_json` (`alloc`). `cargo tembra build`
//! compiles the crate, reads the same WIT and the hook priorities from the component and
//! packages the artifact.
mod wit;

use heck::{ToShoutySnakeCase, ToSnakeCase};
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
        } else if meta.path.is_ident("context") {
            options.context = true;
        } else {
            return Err(meta.error(
                "unknown option; use #[orb(package = \"ns:name@1.0.0\", wit = \"dir\", context)]",
            ));
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
    /// The module imports the kernel's `tembra:guest/context` interface.
    context: bool,
}

/// A type declared in the module, with its WIT definition and the module types it refers to.
struct Shape {
    ident: Ident,
    wit: String,
    references: Vec<Ident>,
}

/// An exported interface: a unit struct marked `#[service]` with an `impl` block of `pub fn`, and
/// the ownership policy of its objects when the attribute names one.
struct Service {
    ident: Ident,
    docs: Vec<String>,
    functions: Vec<Function>,
    policy: Option<Policy>,
}

/// The ownership policy of a service's objects, from `#[service(lifetime = host)]` or
/// `#[service(linger_ms = n)]`; the kernel applies it.
struct Policy {
    /// Objects live until the host closes, instead of while they are referenced.
    host: bool,
    /// How long an unreferenced object stays, in milliseconds.
    linger_ms: u32,
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

/// A hook declaration: a unit struct marked `#[hook("ns:name/hook@version", composition = c)]`.
/// The Orb exports the hook's interface, as the portable hook contract requires of a provider,
/// and imports it as the typed entry to the plan the kernel runs.
struct Declared {
    hook: String,
    composition: Composition,
    ident: Ident,
}

/// How the kernel composes the contributions of a hook.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Composition {
    Series,
    Waterfall,
    Bail,
}

impl Composition {
    fn name(self) -> &'static str {
        match self {
            Self::Series => "series",
            Self::Waterfall => "waterfall",
            Self::Bail => "bail",
        }
    }
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

/// A resource of the Orb's root: a struct marked `#[resource]` whose `impl` block has a
/// `pub fn open() -> Self` and, optionally, a `pub fn close(self)`.
struct Resource {
    ident: Ident,
    version: u32,
    /// Whether the author defined `close`.
    close: bool,
}

fn expand(options: Options, module: ItemMod) -> syn::Result<proc_macro2::TokenStream> {
    let package = options
        .package
        .ok_or_else(|| syn::Error::new(module.span(), "missing package = \"ns:name@1.0.0\""))?;
    let (namespace, rest) = package
        .split_once(':')
        .ok_or_else(|| syn::Error::new(module.span(), "package must be ns:name@version"))?;
    let name = rest.split_once('@').map_or(rest, |(name, _)| name);
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
    let mut declared: Vec<Declared> = Vec::new();
    let mut states = Vec::new();
    let mut resources: Vec<Resource> = Vec::new();
    let mut passthrough = Vec::new();
    let mut impls = Vec::new();
    for item in items {
        match item {
            Item::Struct(mut s) => {
                if let Some(attribute) = take(&mut s.attrs, "hook") {
                    declared.push(declaration(&s, &attribute)?);
                    passthrough.push(Item::Struct(s));
                } else if let Some(attribute) = take(&mut s.attrs, "resource") {
                    resources.push(resource(&s, &attribute)?);
                    // A resource enters the WIT only when a service method uses it.
                    if matches!(s.fields, Fields::Named(_)) {
                        let shape = wit::record(&s)?;
                        candidates.push((Item::Struct(s), shape));
                    } else {
                        passthrough.push(Item::Struct(s));
                    }
                } else if let Some(attribute) = take(&mut s.attrs, "state") {
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
                        policy: policy(&attribute)?,
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
    for resource in &mut resources {
        let functions = impls
            .iter()
            .filter(|item| {
                matches!(&*item.self_ty, Type::Path(path) if path.path.is_ident(&resource.ident))
            })
            .flat_map(|item| &item.items)
            .filter_map(|item| match item {
                syn::ImplItem::Fn(function) if matches!(function.vis, Visibility::Public(_)) => {
                    Some(function)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let opens = functions.iter().any(|function| {
            function.sig.ident == "open"
                && function.sig.inputs.is_empty()
                && !matches!(function.sig.output, ReturnType::Default)
        });
        if !opens {
            return Err(syn::Error::new(
                resource.ident.span(),
                format!(
                    "a resource needs `pub fn open() -> {0}` in an impl block; `pub fn close(self)` is optional",
                    resource.ident
                ),
            ));
        }
        resource.close = functions.iter().any(|function| {
            function.sig.ident == "close"
                && function.sig.inputs.len() == 1
                && matches!(function.sig.inputs.first(), Some(FnArg::Receiver(_)))
        });
    }
    // An Orb exports services, declares hooks or contributes to them; a module doing none of
    // those would be an empty world.
    if services.is_empty() && contributions.is_empty() && declared.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "nothing to export: mark a unit struct with #[service] and give it an impl block of pub fn, or mark a hook with #[hook]",
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
    // A declared hook is run by this Orb and answered by other Orbs' contributions.
    let mut declarations: BTreeMap<String, &Declared> = BTreeMap::new();
    for declaration in &declared {
        if hooks.contains_key(&declaration.hook) {
            return Err(syn::Error::new(
                declaration.ident.span(),
                format!(
                    "`{}` declares the hook {}, which this module also contributes to; an Orb does one or the other",
                    declaration.ident, declaration.hook
                ),
            ));
        }
        if declarations
            .insert(declaration.hook.clone(), declaration)
            .is_some()
        {
            return Err(syn::Error::new(
                declaration.ident.span(),
                "one declaration per hook",
            ));
        }
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

    // Consumed packages: the callback of every hook, contributed or declared, is read from its
    // WIT package.
    let wit_dir = options.wit.as_deref().map(wit::absolute).transpose()?;
    let callbacks = match (&wit_dir, hooks.is_empty() && declarations.is_empty()) {
        (_, true) => Default::default(),
        (Some(dir), false) => wit::callbacks(dir, hooks.keys().chain(declarations.keys()))?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "hooks need wit = \"<dir>\", the directory with the hook's WIT package",
            ));
        }
    };
    for declaration in declarations.values() {
        let callback = &callbacks[&declaration.hook];
        let (fits, rule) = match declaration.composition {
            Composition::Series => (callback.series, "a series callback returns nothing"),
            Composition::Waterfall => (
                callback.waterfall,
                "a waterfall callback returns its input type",
            ),
            Composition::Bail => (callback.bail, "a bail callback returns an option"),
        };
        if !fits {
            return Err(syn::Error::new(
                declaration.ident.span(),
                format!(
                    "the callback of {} does not fit a {} hook: {rule}",
                    declaration.hook,
                    declaration.composition.name()
                ),
            ));
        }
    }
    let wit = wit::render(
        &package,
        &world,
        &shapes,
        &services,
        hooks.keys().chain(declarations.keys()),
        wit::Kernel {
            state: !states.is_empty(),
            resources: !resources.is_empty(),
            context: options.context,
        },
    );

    // Generate the Rust side: the bindings from the inline WIT, the author's types replaced by
    // the generated ones, the bridges to the author's functions, and the no_std prelude.
    let module_ident = module.ident;
    let vis = module.vis;
    let attrs = module.attrs;
    let ns_ident = format_ident!("{}", namespace.to_snake_case());
    let name_ident = format_ident!("{}", name.to_snake_case());
    let shape_idents = shapes.iter().map(|shape| &shape.ident).collect::<Vec<_>>();
    // The generated types carry the author's names and public fields, so the author's functions
    // and the generated traits speak about one type. A shape is reached from a service, so a
    // module with shapes has a first service.
    let reexports = match services.first() {
        Some(first) if !shapes.is_empty() => {
            let first_interface = format_ident!("{}", first.ident.to_string().to_snake_case());
            quote! { pub use exports::#ns_ident::#name_ident::#first_interface::{#(#shape_idents),*}; }
        }
        _ => quote! {},
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
    // The kernel's context interface is re-exported as `context`, so the author writes
    // `context::principal()`.
    let context_module = options
        .context
        .then(|| quote! { pub use self::tembra::guest::context; });
    // Each hook's generated module is re-exported under its interface name: the export module
    // of a contributed hook, whose callback forwards to the author's function, and the import
    // module of a declared hook, whose types `run` takes and returns.
    let hook_modules = hooks
        .keys()
        .map(|hook| {
            let [ns, package, interface] = hook_path(hook);
            quote! { pub use exports::#ns::#package::#interface; }
        })
        .chain(declarations.keys().map(|hook| {
            let [ns, package, interface] = hook_path(hook);
            quote! { pub use self::#ns::#package::#interface; }
        }));
    let contribution_bridges = contributions.iter().map(|contribution| {
        let [ns, package, interface] = hook_path(&contribution.hook);
        let callback = format_ident!("{}", callbacks[&contribution.hook].name.to_snake_case());
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
    // The manifest travels with the component, like the WIT: the contributions with their
    // priorities, the declarations with their composition, and the services that chose a
    // lifetime or a linger.
    let policies = services
        .iter()
        .filter_map(|service| {
            let policy = service.policy.as_ref()?;
            let lifetime = if policy.host { "host" } else { "referenced" };
            Some(format!(
                "\"{}\":{{\"lifetime\":\"{lifetime}\",\"linger_ms\":{}}}",
                identity(&package, &wit::kebab(&service.ident)),
                policy.linger_ms
            ))
        })
        .collect::<Vec<_>>();
    let manifest_section = if hooks.is_empty() && declarations.is_empty() && policies.is_empty() {
        quote! {}
    } else {
        let contributes = hooks
            .iter()
            .map(|(hook, priority)| format!("\"{hook}\":{priority}"))
            .collect::<Vec<_>>()
            .join(",");
        let declares = declarations
            .iter()
            .map(|(hook, declaration)| format!("\"{hook}\":\"{}\"", declaration.composition.name()))
            .collect::<Vec<_>>()
            .join(",");
        let services = policies.join(",");
        let json = format!(
            "{{\"contributes\":{{{contributes}}},\"declares\":{{{declares}}},\"services\":{{{services}}}}}"
        );
        let bytes = LitByteStr::new(json.as_bytes(), Span::call_site());
        let len = proc_macro2::Literal::usize_unsuffixed(json.len());
        quote! {
            #[cfg(target_arch = "wasm32")]
            #[unsafe(link_section = "tembra:manifest")]
            #[used]
            static __TEMBRA_MANIFEST: [u8; #len] = *#bytes;
        }
    };
    let state_code = state_code(&states, &package);
    let resource_code = resource_code(&resources);
    let declaration_code = declaration_code(&declared, &callbacks)?;
    let wit_literal = LitStr::new(&wit, Span::call_site());
    let wit_bytes = LitByteStr::new(wit.as_bytes(), Span::call_site());
    let wit_len = proc_macro2::Literal::usize_unsuffixed(wit.len());
    let world_literal = LitStr::new(&world, Span::call_site());
    // The bindings read the author's WIT and, nested in it, the kernel package a state, a
    // resource or the context uses; the hook packages come from the consumed directory. Foreign
    // interfaces need `generate_all`.
    let inline = if states.is_empty() && resources.is_empty() && !options.context {
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

            #manifest_section

            #generate

            #reexports
            #context_module
            #(#hook_modules)*

            #(#passthrough)*

            struct __TembraExports;
            #(#bridges)*
            #(#contribution_bridges)*
            #declaration_code
            #state_code
            #resource_code
            export!(__TembraExports);
        }
    })
}

/// Remove and return the marker attribute with the given name, if present.
fn take(attrs: &mut Vec<Attribute>, name: &str) -> Option<Attribute> {
    let index = attrs.iter().position(|attr| attr.path().is_ident(name))?;
    Some(attrs.remove(index))
}

/// The versioned identity of an interface of this package: `ns:name/interface@version`.
fn identity(package: &str, interface: &str) -> String {
    match package.split_once('@') {
        Some((name, version)) => format!("{name}/{interface}@{version}"),
        None => format!("{package}/{interface}"),
    }
}

/// Read the ownership policy a `#[service]` attribute names, if any: `lifetime = host` pins the
/// objects until the host closes, `linger_ms = n` keeps an unreferenced object for `n`
/// milliseconds; a host-lifetime object cannot linger, as the kernel rules.
fn policy(attribute: &Attribute) -> syn::Result<Option<Policy>> {
    if matches!(attribute.meta, syn::Meta::Path(_)) {
        return Ok(None);
    }
    let (mut host, mut linger_ms) = (false, 0);
    attribute.parse_nested_meta(|meta| {
        if meta.path.is_ident("lifetime") {
            let value: Ident = meta.value()?.parse()?;
            host = match value.to_string().as_str() {
                "host" => true,
                "referenced" => false,
                _ => {
                    return Err(syn::Error::new(
                        value.span(),
                        "the lifetime is referenced (the default) or host",
                    ));
                }
            };
        } else if meta.path.is_ident("linger_ms") {
            let literal: LitInt = meta.value()?.parse()?;
            linger_ms = literal.base10_parse::<u32>()?;
        } else {
            return Err(meta.error(
                "unknown option; use #[service(lifetime = host)] or #[service(linger_ms = n)]",
            ));
        }
        Ok(())
    })?;
    if host && linger_ms != 0 {
        return Err(syn::Error::new(
            attribute.span(),
            "host-lifetime objects cannot linger: they live until the host closes",
        ));
    }
    Ok(Some(Policy { host, linger_ms }))
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

/// Read a hook declaration: a unit struct, with the hook's name and its composition on the
/// attribute.
fn declaration(item: &syn::ItemStruct, attribute: &Attribute) -> syn::Result<Declared> {
    if !matches!(item.fields, Fields::Unit) {
        return Err(syn::Error::new(
            attribute.span(),
            "a hook declaration is a unit struct: #[hook(\"ns:name/hook@version\", composition = waterfall)] pub struct Name;",
        ));
    }
    let (hook, composition) = attribute.parse_args_with(|input: syn::parse::ParseStream| {
        let hook: LitStr = input.parse()?;
        input.parse::<Token![,]>()?;
        let key: Ident = input.parse()?;
        if key != "composition" {
            return Err(syn::Error::new(
                key.span(),
                "unknown option; a declared hook names its composition: #[hook(\"ns:name/hook@version\", composition = series | waterfall | bail)]",
            ));
        }
        input.parse::<Token![=]>()?;
        let value: Ident = input.parse()?;
        let composition = match value.to_string().as_str() {
            "series" => Composition::Series,
            "waterfall" => Composition::Waterfall,
            "bail" => Composition::Bail,
            _ => {
                return Err(syn::Error::new(
                    value.span(),
                    "the composition is series, waterfall or bail",
                ));
            }
        };
        Ok((hook, composition))
    })?;
    let id = hook.value();
    if !id.contains(':') || !id.contains('/') || id.contains(['"', '\\']) {
        return Err(syn::Error::new(
            hook.span(),
            "a hook is named ns:name/interface@version",
        ));
    }
    Ok(Declared {
        hook: id,
        composition,
        ident: item.ident.clone(),
    })
}

/// The code of the declared hooks: for each, the export the contract requires, which traps
/// because the contributions answer the hook, and `run` on the author's struct, which calls the
/// matching import so the kernel runs the plan.
fn declaration_code(
    declared: &[Declared],
    callbacks: &BTreeMap<String, wit::Callback>,
) -> syn::Result<proc_macro2::TokenStream> {
    if declared.is_empty() {
        return Ok(quote! {});
    }
    let items = declared
        .iter()
        .map(|declaration| {
            let callback = &callbacks[&declaration.hook];
            let [ns, package, interface] = hook_path(&declaration.hook);
            let name = format_ident!("{}", callback.name.to_snake_case());
            let export_input: Type = syn::parse_str(&callback.input.export)?;
            let import_input: Type = syn::parse_str(&callback.input.import)?;
            let output = |spelling: &str| -> syn::Result<proc_macro2::TokenStream> {
                let ty: Type = syn::parse_str(spelling)?;
                Ok(quote! { -> #ty })
            };
            let export_result = match &callback.result {
                Some(spelled) => output(&spelled.export)?,
                None => quote! {},
            };
            let import_result = match &callback.result {
                Some(spelled) => output(&spelled.import)?,
                None => quote! {},
            };
            let ident = &declaration.ident;
            let message = format!(
                "the hook {} is declared by this Orb: its contributions answer it, this export never does",
                declaration.hook
            );
            let doc = format!(
                " Run the hook `{}`: the kernel calls its contributions in plan order and returns the composed result.",
                declaration.hook
            );
            Ok(quote! {
                impl exports::#ns::#package::#interface::Guest for __TembraExports {
                    fn #name(_input: #export_input) #export_result {
                        ::core::panic!(#message)
                    }
                }
                impl #ident {
                    #[doc = #doc]
                    pub fn run(input: #import_input) #import_result {
                        self::#ns::#package::#interface::#name(input)
                    }
                }
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;
    // The bindings spell strings and lists from `alloc`; the module may not name that crate.
    Ok(quote! {
        extern crate alloc as __tembra_alloc;
        #(#items)*
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
fn state_code(states: &[State], package: &str) -> proc_macro2::TokenStream {
    if states.is_empty() {
        return quote! {};
    }
    let declarations = states.iter().map(|state| {
        let service = state.service.as_ref().expect("resolved owner");
        let identity = LitStr::new(&identity(package, &wit::kebab(service)), Span::call_site());
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

/// Read a resource: any struct, with the version of its contract (default 1).
fn resource(item: &syn::ItemStruct, attribute: &Attribute) -> syn::Result<Resource> {
    let mut version = 1;
    if !matches!(attribute.meta, syn::Meta::Path(_)) {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("version") {
                let literal: LitInt = meta.value()?.parse()?;
                version = literal.base10_parse::<u32>()?;
                if version == 0 {
                    return Err(syn::Error::new(literal.span(), "versions start at 1"));
                }
            } else {
                return Err(meta.error("unknown option; use #[resource(version = n)]"));
            }
            Ok(())
        })?;
    }
    Ok(Resource {
        ident: item.ident.clone(),
        version,
        close: false,
    })
}

/// The Rust side of the resources: the kernel's `resources-init` callbacks listing, opening and
/// closing them, one slot in the component's memory per resource, and `with` on each type.
fn resource_code(resources: &[Resource]) -> proc_macro2::TokenStream {
    if resources.is_empty() {
        return quote! {};
    }
    let slot = |resource: &Resource| {
        format_ident!(
            "__TEMBRA_RESOURCE_{}",
            resource.ident.to_string().to_shouty_snake_case()
        )
    };
    let declarations = resources.iter().map(|resource| {
        let name = wit::kebab(&resource.ident);
        let version = resource.version;
        quote! {
            exports::tembra::guest::resources_init::Declaration {
                name: __tembra_resource::String::from(#name),
                version: #version,
            }
        }
    });
    let openers = resources.iter().map(|resource| {
        let name = wit::kebab(&resource.ident);
        let ident = &resource.ident;
        let slot = slot(resource);
        quote! { #name => { #slot.open(#ident::open()); 1 } }
    });
    let closers = resources.iter().map(|resource| {
        let name = wit::kebab(&resource.ident);
        let slot = slot(resource);
        if resource.close {
            quote! { #name => { if let Some(value) = #slot.close() { value.close(); } } }
        } else {
            quote! { #name => { #slot.close(); } }
        }
    });
    let slots = resources.iter().map(|resource| {
        let ident = &resource.ident;
        let slot = slot(resource);
        let name = wit::kebab(&resource.ident);
        let doc = format!(
            " Use the resource `{name}` of this Orb's root, opened by the kernel at activation; the kernel confirms it is open, the value lives in this component."
        );
        quote! {
            static #slot: __tembra_resource::Slot<#ident> = __tembra_resource::Slot::new();
            impl #ident {
                #[doc = #doc]
                pub fn with<R>(use_: impl FnOnce(&mut Self) -> R) -> R {
                    let _handle = self::tembra::guest::resources::read(#name);
                    #slot.with(use_)
                }
            }
        }
    });
    quote! {
        // The kernel lists the root's resources when it creates the root, then opens and closes
        // each of them in its own order through these callbacks.
        impl exports::tembra::guest::resources_init::Guest for __TembraExports {
            fn declare() -> __tembra_resource::Vec<exports::tembra::guest::resources_init::Declaration> {
                __tembra_resource::Vec::from([#(#declarations),*])
            }
            fn open(name: __tembra_resource::String) -> u32 {
                match name.as_str() {
                    #(#openers)*
                    _ => ::core::panic!("unknown resource"),
                }
            }
            fn close(name: __tembra_resource::String, _handle: u32) {
                match name.as_str() {
                    #(#closers)*
                    _ => {}
                }
            }
        }
        #(#slots)*
        /// A resource's value lives in the component while the resource is open.
        #[doc(hidden)]
        pub mod __tembra_resource {
            extern crate alloc;
            pub use alloc::{string::String, vec::Vec};

            /// One resource's value, absent until the kernel opens it and after it closes it.
            pub struct Slot<T>(::core::cell::RefCell<Option<T>>);

            // SAFETY: a component runs one export at a time on one thread, so the slot is never
            // reached from two threads; the RefCell refuses a reentrant use at run time.
            unsafe impl<T> Sync for Slot<T> {}

            impl<T> Slot<T> {
                pub const fn new() -> Self {
                    Self(::core::cell::RefCell::new(None))
                }

                pub fn open(&self, value: T) {
                    *self.0.borrow_mut() = Some(value);
                }

                pub fn close(&self) -> Option<T> {
                    self.0.borrow_mut().take()
                }

                pub fn with<R>(&self, use_: impl FnOnce(&mut T) -> R) -> R {
                    use_(self.0.borrow_mut().as_mut().expect("the resource is not open"))
                }
            }

            impl<T> Default for Slot<T> {
                fn default() -> Self {
                    Self::new()
                }
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

/// The generated module of a hook interface: `exports::<ns>::<package>::<interface>` for the
/// export, `<ns>::<package>::<interface>` for the import.
pub(crate) fn hook_path(hook: &str) -> [Ident; 3] {
    let (package, rest) = hook.split_once('/').unwrap_or((hook, ""));
    let (namespace, name) = package.split_once(':').unwrap_or((package, ""));
    let interface = rest.split('@').next().unwrap_or(rest);
    [namespace, name, interface].map(|part| format_ident!("{}", part.to_snake_case()))
}

#[cfg(test)]
mod tests;
