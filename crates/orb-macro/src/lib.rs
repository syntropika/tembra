//! `#[orb(package = "ns:name@1.0.0")]` turns a Rust module into a compiled Orb.
//!
//! The module is the source of truth, and every exported thing is marked. A unit struct with
//! `#[service]` and an `impl` block of `pub fn` becomes an exported interface whose functions are
//! the Orb's methods. A free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`
//! participates in another Orb's hook. A struct with named fields and `#[state]` is the
//! kernel-managed state of a service's objects. A struct with `#[resource]` and a function marked
//! `#[open]` is a resource of the Orb's root, opened by the kernel at activation and usable by
//! every method and hook callback. Structs with named fields and enums become `record`s, `enum`s and
//! `variant`s only when a service method or a state uses them, directly or through another type;
//! the rest of the module stays ordinary Rust.
//!
//! The macro derives the WIT contract from those items and generates what a WIT-first crate
//! writes by hand: the `wit_bindgen::generate!` invocation with the WIT inline, the `Guest`
//! implementations forwarding to the author's functions, `export!`, and the `no_std` runtime
//! prelude for the Wasm target. The hook interfaces come from the provider's WIT, found in the
//! directory named by `wit = "dir"` on the attribute, relative to the crate manifest; their
//! generated modules are re-exported under their own names, so the author writes
//! `guard::Admission`.
//!
//! The kernel's side is the private `tembra:orb` package the host links for every Orb built
//! against it, JavaScript or Rust: the component exports its `lifecycle` and imports its `host`.
//! The macro implements the lifecycle from the marked items: `describe` reports the services with
//! their lifetime, the hooks declared and contributed to; `create` declares each object's state,
//! the root's hooks, resources and references to the providers of its imports; `validate`,
//! `open` and `close` call the marked functions. The package never enters the contract: the
//! bindings are generated from a build world that adds it, as the TypeScript build does. With
//! `context` on the attribute the module reads the call's principal and cancellation, the
//! object's address and the host's log as `context::...`.
//!
//! The crate using the macro must depend on `wit-bindgen` (features `macros`, `realloc` and
//! `async`, for the lifecycle's `open` and `close`) and `dlmalloc` (feature `global`), because the
//! generated code names them; a crate with a state also depends on `serde` (`derive`, `alloc`) and
//! `serde_json` (`alloc`). The Orb's name and API version come from the crate manifest's
//! `[package.metadata.tembra]`, as for `cargo tembra build`, which compiles the crate, reads the
//! same WIT from the component and packages the artifact.
mod wit;

use heck::{ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
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
        } else if meta.path.is_ident("import") {
            options
                .imports
                .push(meta.value()?.parse::<LitStr>()?.value());
        } else if meta.path.is_ident("context") {
            options.context = true;
        } else {
            return Err(meta.error(
                "unknown option; use #[orb(package = \"ns:name@1.0.0\", wit = \"dir\", import = \"ns:name/interface@version\", context)]",
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
    /// The module reads its call's context through the kernel: a `context` module of functions.
    context: bool,
    /// Interfaces of other Orbs the module imports, from the consumed WIT packages; the installer
    /// binds each to a provider.
    imports: Vec<String>,
}

/// A type declared in the module, with its WIT definition and the module types it refers to.
struct Shape {
    ident: Ident,
    wit: String,
    references: Vec<Ident>,
    /// How the type is built, for the conversions between the two sides of a derived hook.
    kind: ShapeKind,
    /// The module declaring the type, by scope; 0 is the Orb module.
    scope: usize,
}

/// The construction of a shape: the named fields of a record, the cases of an enum, the cases of
/// a variant with their payload.
enum ShapeKind {
    Record(Vec<(Ident, Type)>),
    Enum(Vec<Ident>),
    Variant(Vec<(Ident, Option<Type>)>),
}

/// An exported interface: a unit struct marked `#[service]` with an `impl` block of `pub fn`, and
/// the ownership policy of its objects when the attribute names one. A plain `#[service]`
/// derives the interface from the `impl`; `#[service("ns:name/interface@version")]` implements an
/// interface of a consumed WIT package.
struct Service {
    ident: Ident,
    docs: Vec<String>,
    functions: Vec<Function>,
    policy: Option<Policy>,
    implements: Option<String>,
    /// The module declaring the service, by scope; 0 is the Orb module.
    scope: usize,
}

impl Service {
    /// The versioned identity of the service's interface.
    fn identity(&self, package: &str) -> String {
        self.implements
            .clone()
            .unwrap_or_else(|| identity(package, &wit::kebab(&self.ident)))
    }
}

/// A WIT resource: a struct marked `#[wit_resource]` whose instances callers own through handles.
/// Its function marked `#[constructor]` is the constructor, its `pub fn` taking `&self` are the
/// methods and the other `pub fn` its static functions. It belongs to the interface of one
/// service: derived with it, or defined by the WIT the service implements.
struct WitResource {
    ident: Ident,
    docs: Vec<String>,
    /// The owning service, as named by `service = ...` or resolved to the module's only one.
    service: Option<Ident>,
    constructor: Option<Constructor>,
    methods: Vec<Function>,
    statics: Vec<Function>,
    /// The module declaring the resource, by scope; 0 is the Orb module.
    scope: usize,
}

/// A WIT resource's constructor: the function marked `#[constructor]` and its parameters, with
/// the WIT spelling of their types.
struct Constructor {
    ident: Ident,
    params: Vec<(Ident, Type, String)>,
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
    /// The module declaring the function, by scope; 0 is the Orb module.
    scope: usize,
}

/// A hook declaration: a unit struct marked `#[hook(composition = c, input = Type)]`, which
/// derives the hook's interface from the module under the struct's name, or
/// `#[hook("ns:name/hook@version", composition = c)]`, which names an interface of a consumed
/// package. The Orb exports the hook's interface, as the portable hook contract requires of a
/// provider, and imports it as the typed entry to the plan the kernel runs.
struct Declared {
    /// The hook's identity: named on the attribute, or derived from the package and the struct.
    hook: String,
    composition: Composition,
    ident: Ident,
    docs: Vec<String>,
    /// The callback's input when the hook is derived from the module: the author's type and its
    /// WIT spelling. `None` for a hook named from a consumed package.
    input: Option<(Type, String)>,
    /// The module declaring the struct, by scope; 0 is the Orb module.
    scope: usize,
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
    /// The function marked `#[validate]`, `fn(&self) -> Result<(), String>`: every update runs it
    /// before the kernel stores the value, and a reload runs it on the value the new instance
    /// inherits.
    validator: Option<Ident>,
    /// The identity of the owning service's interface, once the owner is resolved.
    identity: String,
    /// The module declaring the state, by scope; 0 is the Orb module.
    scope: usize,
}

/// A resource of the Orb's root: a struct marked `#[resource]` whose `impl` block has a function
/// marked `#[open]`, `fn() -> Self`, and, optionally, one marked `#[close]`, `fn(self)`.
struct Resource {
    ident: Ident,
    version: u32,
    /// The function marked `#[open]`, once found.
    open: Option<Ident>,
    /// The function marked `#[close]`, if any.
    close: Option<Ident>,
    /// The module declaring the resource, by scope; 0 is the Orb module.
    scope: usize,
}

/// A marker on a function of an `impl` block. The functions the generated code calls by role
/// carry one, whatever their names and visibility; a function without one is ordinary Rust.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// `#[validate]`: a state's rule, `fn(&self) -> Result<(), String>`.
    Validate,
    /// `#[open]`: a root resource's opening, `fn() -> Self`.
    Open,
    /// `#[close]`: a root resource's closing, `fn(self)`.
    Close,
    /// `#[constructor]`: a WIT resource's constructor, `fn(..) -> Self` without `self`.
    Constructor,
}

impl Marker {
    const ALL: [Marker; 4] = [
        Marker::Validate,
        Marker::Open,
        Marker::Close,
        Marker::Constructor,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Open => "open",
            Self::Close => "close",
            Self::Constructor => "constructor",
        }
    }

    /// The shape of the function the marker goes on, for the errors.
    fn shape(self) -> &'static str {
        match self {
            Self::Validate => "`fn(&self) -> Result<(), String>` of a #[state] struct",
            Self::Open => "`fn() -> Self` of a #[resource] struct",
            Self::Close => "`fn(self)` of a #[resource] struct",
            Self::Constructor => "`fn(..) -> Self`, without self, of a #[wit_resource] struct",
        }
    }

    /// The kind of struct whose `impl` block the marker goes in, for the errors.
    fn place(self) -> &'static str {
        match self {
            Self::Validate => "#[state]",
            Self::Open | Self::Close => "#[resource]",
            Self::Constructor => "#[wit_resource]",
        }
    }
}

/// A marked function of an inherent `impl` block, with the marker removed from the function.
struct Marked {
    marker: Marker,
    /// The type of the `impl` block.
    self_ty: Ident,
    function: syn::ImplItemFn,
    /// Where the marker was written, for the errors.
    span: Span,
    /// The module of the `impl` block, by scope; 0 is the Orb module.
    scope: usize,
}

/// The one function of `ident`'s `impl` blocks, in the module `scope`, carrying `marker`, if any;
/// two are an error.
fn marked_function<'a>(
    marked: &'a [Marked],
    marker: Marker,
    ident: &Ident,
    scope: usize,
) -> syn::Result<Option<&'a Marked>> {
    let mut found = marked.iter().filter(|candidate| {
        candidate.marker == marker && &candidate.self_ty == ident && candidate.scope == scope
    });
    let first = found.next();
    if let Some(second) = found.next() {
        return Err(syn::Error::new(
            second.span,
            format!("`{ident}` has two functions marked #[{}]", marker.name()),
        ));
    }
    Ok(first)
}

/// A module of the Orb as the scan rebuilds it: the author's items copied through, the items the
/// macro generates for it, and its sub-modules. The Orb module itself is the root, without a name.
struct Module {
    ident: Option<Ident>,
    attrs: Vec<Attribute>,
    vis: Visibility,
    unsafety: Option<syn::token::Unsafe>,
    items: Vec<Item>,
    generated: Vec<proc_macro2::TokenStream>,
    children: Vec<Module>,
}

impl Module {
    /// The module at `path` below this one, which the scan registered.
    fn at(&mut self, path: &[Ident]) -> &mut Module {
        let mut module = self;
        for segment in path {
            module = module
                .children
                .iter_mut()
                .find(|child| child.ident.as_ref() == Some(segment))
                .expect("a scanned sub-module");
        }
        module
    }

    fn tokens(&self) -> proc_macro2::TokenStream {
        let items = &self.items;
        let generated = &self.generated;
        let children = self.children.iter().map(Module::tokens);
        let body = quote! { #(#items)* #(#generated)* #(#children)* };
        match &self.ident {
            Some(ident) => {
                let attrs = &self.attrs;
                let vis = &self.vis;
                let unsafety = &self.unsafety;
                quote! { #(#attrs)* #vis #unsafety mod #ident { #body } }
            }
            None => body,
        }
    }
}

/// What the scan of the Orb module collects, across its sub-modules.
struct Scan {
    package: String,
    /// The path of each module from the Orb module, by scope; scope 0 is the Orb module itself.
    modules: Vec<Vec<Ident>>,
    candidates: Vec<(Item, Shape)>,
    services: Vec<Service>,
    contributions: Vec<Contribution>,
    declared: Vec<Declared>,
    states: Vec<State>,
    resources: Vec<Resource>,
    wit_resources: Vec<WitResource>,
    impls: Vec<(usize, syn::ItemImpl)>,
    marked: Vec<Marked>,
}

/// Whether an item of a sub-module is visible to the Orb module: `pub`, or `pub(..)`.
fn reaches_root(vis: &Visibility) -> bool {
    !matches!(vis, Visibility::Inherited)
}

impl Scan {
    /// Read the items of one module: the marked ones are collected with their scope, the
    /// sub-modules are read in turn, and everything else is copied through.
    fn scan(&mut self, items: Vec<Item>, scope: usize) -> syn::Result<(Vec<Item>, Vec<Module>)> {
        let mut passthrough = Vec::new();
        let mut children = Vec::new();
        let nested = scope != 0;
        for item in items {
            match item {
                Item::Mod(module) => {
                    let Some((_, inner)) = module.content else {
                        return Err(syn::Error::new(
                            module.ident.span(),
                            "a sub-module of an Orb is written inline, `mod name { .. }`: the macro reads its items",
                        ));
                    };
                    let child = self.modules.len();
                    let mut path = self.modules[scope].clone();
                    path.push(module.ident.clone());
                    self.modules.push(path);
                    let (items, grandchildren) = self.scan(inner, child)?;
                    children.push(Module {
                        ident: Some(module.ident),
                        attrs: module.attrs,
                        vis: module.vis,
                        unsafety: module.unsafety,
                        items,
                        generated: Vec::new(),
                        children: grandchildren,
                    });
                }
                Item::Struct(mut s) => {
                    if let Some(attribute) = take(&mut s.attrs, "hook") {
                        let mut declaration = declaration(&s, &attribute, &self.package)?;
                        declaration.scope = scope;
                        self.declared.push(declaration);
                        passthrough.push(Item::Struct(s));
                    } else if let Some(attribute) = take(&mut s.attrs, "wit_resource") {
                        if nested && !reaches_root(&s.vis) {
                            return Err(syn::Error::new(
                                s.ident.span(),
                                format!(
                                    "`{}` is a resource of a sub-module: make it visible to the Orb module, `pub` or `pub(super)`, whose services hand it out",
                                    s.ident
                                ),
                            ));
                        }
                        let mut resource = wit_resource(&s, &attribute)?;
                        resource.scope = scope;
                        self.wit_resources.push(resource);
                        passthrough.push(Item::Struct(s));
                    } else if let Some(attribute) = take(&mut s.attrs, "resource") {
                        if nested && !reaches_root(&s.vis) {
                            return Err(syn::Error::new(
                                s.ident.span(),
                                format!(
                                    "`{}` is a resource of a sub-module: make it visible to the Orb module, `pub` or `pub(super)`, which opens and closes it",
                                    s.ident
                                ),
                            ));
                        }
                        let mut resource = resource(&s, &attribute)?;
                        resource.scope = scope;
                        self.resources.push(resource);
                        // A resource enters the WIT only when a service method uses it.
                        if matches!(s.fields, Fields::Named(_)) {
                            let mut shape = wit::record(&s)?;
                            shape.scope = scope;
                            self.candidates.push((Item::Struct(s), shape));
                        } else {
                            passthrough.push(Item::Struct(s));
                        }
                    } else if let Some(attribute) = take(&mut s.attrs, "state") {
                        if nested {
                            let fields_visible = match &s.fields {
                                Fields::Named(fields) => {
                                    fields.named.iter().all(|field| reaches_root(&field.vis))
                                }
                                _ => true,
                            };
                            if !reaches_root(&s.vis) || !fields_visible {
                                return Err(syn::Error::new(
                                    s.ident.span(),
                                    format!(
                                        "`{}` is a state of a sub-module: make the struct and its fields visible to the Orb module, `pub` or `pub(super)`, which builds and reads it",
                                        s.ident
                                    ),
                                ));
                            }
                        }
                        let mut state = state(&s, &attribute)?;
                        state.scope = scope;
                        self.states.push(state);
                        let mut shape = wit::record(&s)?;
                        shape.scope = scope;
                        self.candidates.push((Item::Struct(s), shape));
                    } else if let Some(attribute) = take(&mut s.attrs, "service") {
                        if !matches!(s.fields, Fields::Unit) {
                            return Err(syn::Error::new(
                                attribute.span(),
                                "a service is a unit struct: its methods are the pub fn of its impl blocks",
                            ));
                        }
                        let (implements, policy) = service_attribute(&attribute)?;
                        self.services.push(Service {
                            ident: s.ident.clone(),
                            docs: wit::docs(&s.attrs),
                            functions: Vec::new(),
                            policy,
                            implements,
                            scope,
                        });
                        passthrough.push(Item::Struct(s));
                    } else if matches!(s.fields, Fields::Named(_)) {
                        let mut shape = wit::record(&s)?;
                        shape.scope = scope;
                        self.candidates.push((Item::Struct(s), shape));
                    } else {
                        passthrough.push(Item::Struct(s));
                    }
                }
                Item::Enum(e) => {
                    let mut shape = wit::enumeration(&e)?;
                    shape.scope = scope;
                    self.candidates.push((Item::Enum(e), shape));
                }
                Item::Impl(mut i) if i.trait_.is_none() => {
                    if let Some(attribute) = i.attrs.iter().find(|a| a.path().is_ident("service")) {
                        return Err(syn::Error::new(
                            attribute.span(),
                            "put #[service] on the unit struct, not on its impl block",
                        ));
                    }
                    // The marked functions are read here and their markers removed: the
                    // compiler knows no attribute of those names.
                    for impl_item in &mut i.items {
                        let syn::ImplItem::Fn(function) = impl_item else {
                            continue;
                        };
                        let mut carried = Vec::new();
                        for marker in Marker::ALL {
                            if let Some(attribute) = take(&mut function.attrs, marker.name()) {
                                carried.push((marker, attribute.span()));
                            }
                        }
                        if carried.len() > 1 {
                            return Err(syn::Error::new(
                                carried[1].1,
                                format!(
                                    "`{}` carries two markers; a function has one role",
                                    function.sig.ident
                                ),
                            ));
                        }
                        let Some((marker, span)) = carried.pop() else {
                            continue;
                        };
                        if nested && !reaches_root(&function.vis) {
                            return Err(syn::Error::new(
                                span,
                                format!(
                                    "#[{}] on a function of a sub-module: make it visible to the Orb module, `pub` or `pub(super)`, which calls it",
                                    marker.name()
                                ),
                            ));
                        }
                        let self_ty = match &*i.self_ty {
                            Type::Path(path) => path.path.get_ident().cloned(),
                            _ => None,
                        };
                        let Some(self_ty) = self_ty else {
                            return Err(syn::Error::new(
                                span,
                                format!(
                                    "#[{}] goes in the impl block of a {} struct of this module",
                                    marker.name(),
                                    marker.place()
                                ),
                            ));
                        };
                        self.marked.push(Marked {
                            marker,
                            self_ty,
                            function: function.clone(),
                            span,
                            scope,
                        });
                    }
                    self.impls.push((scope, i.clone()));
                    passthrough.push(Item::Impl(i));
                }
                Item::Impl(i) => {
                    // A trait impl is copied through unread; a marker belongs to an inherent
                    // block.
                    for impl_item in &i.items {
                        let syn::ImplItem::Fn(function) = impl_item else {
                            continue;
                        };
                        for marker in Marker::ALL {
                            if let Some(attribute) = function
                                .attrs
                                .iter()
                                .find(|attribute| attribute.path().is_ident(marker.name()))
                            {
                                return Err(syn::Error::new(
                                    attribute.span(),
                                    format!(
                                        "#[{}] goes in an inherent impl block of a {} struct, not in a trait impl",
                                        marker.name(),
                                        marker.place()
                                    ),
                                ));
                            }
                        }
                    }
                    passthrough.push(Item::Impl(i));
                }
                Item::Fn(mut function) => {
                    if let Some(attribute) = take(&mut function.attrs, "contributes") {
                        return Err(syn::Error::new(
                            attribute.span(),
                            "#[contributes] is now #[hook(\"ns:name/hook@version\", priority = n)]",
                        ));
                    }
                    for marker in Marker::ALL {
                        if let Some(attribute) = take(&mut function.attrs, marker.name()) {
                            return Err(syn::Error::new(
                                attribute.span(),
                                format!(
                                    "#[{}] goes on a function in the impl block of a {} struct, not on a free function",
                                    marker.name(),
                                    marker.place()
                                ),
                            ));
                        }
                    }
                    if let Some(attribute) = take(&mut function.attrs, "hook") {
                        let mut contribution = contribution(&function, &attribute)?;
                        contribution.scope = scope;
                        self.contributions.push(contribution);
                    }
                    passthrough.push(Item::Fn(function));
                }
                other => passthrough.push(other),
            }
        }
        Ok((passthrough, children))
    }
}

/// `super::` once per level below the Orb module: how a sub-module's code names the root.
fn supers(depth: usize) -> proc_macro2::TokenStream {
    let supers = std::iter::repeat_n(quote! { super:: }, depth);
    quote! { #(#supers)* }
}

/// The path of an item from the Orb module: its module's path, then its name.
fn from_root(path: &[Ident], ident: &Ident) -> proc_macro2::TokenStream {
    quote! { #(#path::)* #ident }
}

/// The path of an item from a module `depth` levels below the Orb module.
fn relative(depth: usize, path: &[Ident], ident: &Ident) -> proc_macro2::TokenStream {
    let root = supers(depth);
    let target = from_root(path, ident);
    quote! { #root #target }
}

/// The generated export module of an interface, as a module `depth` levels below the root names it.
fn export_module(depth: usize, [ns, package, interface]: &[Ident; 3]) -> proc_macro2::TokenStream {
    let root = supers(depth);
    quote! { #root exports::#ns::#package::#interface }
}

/// The generated import module of an interface, as a module `depth` levels below the root names it.
fn import_module(depth: usize, [ns, package, interface]: &[Ident; 3]) -> proc_macro2::TokenStream {
    if depth == 0 {
        quote! { self::#ns::#package::#interface }
    } else {
        let root = supers(depth);
        quote! { #root #ns::#package::#interface }
    }
}

/// A type spelled from a consumed WIT, as a module `depth` levels below the root names it: the
/// spellings name the generated modules from the Orb module.
fn rooted(spelling: &str, depth: usize) -> String {
    if depth == 0 {
        return spelling.to_owned();
    }
    let root = "super::".repeat(depth);
    spelling
        .replace("exports::", &format!("{root}exports::"))
        .replace("self::", &root)
}

/// The module at `path`, for the errors: the Orb module itself when the path is empty.
fn describe(path: &[Ident]) -> String {
    if path.is_empty() {
        return "the Orb module".into();
    }
    path.iter()
        .map(|segment| format!("`{segment}`"))
        .collect::<Vec<_>>()
        .join("::")
}

/// The service owning a state or a WIT resource: the one named, else the only service of the
/// item's module, else the only service of the Orb.
fn owner_of<'a>(
    services: &'a [Service],
    named: Option<&Ident>,
    scope: usize,
    span: Span,
    hint: &str,
) -> syn::Result<&'a Service> {
    if let Some(ident) = named {
        return services
            .iter()
            .find(|service| &service.ident == ident)
            .ok_or_else(|| {
                syn::Error::new(
                    ident.span(),
                    format!("`{ident}` is not a #[service] of this module"),
                )
            });
    }
    let mut local = services.iter().filter(|service| service.scope == scope);
    match (local.next(), local.next(), services.len()) {
        (Some(only), None, _) => Ok(only),
        (_, _, 1) => Ok(&services[0]),
        _ => Err(syn::Error::new(span, hint)),
    }
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

    // Read the module and its sub-modules: marked services, hooks, states and resources,
    // candidate types, and everything else to copy through unchanged.
    let mut scan = Scan {
        package: package.clone(),
        modules: vec![Vec::new()],
        candidates: Vec::new(),
        services: Vec::new(),
        contributions: Vec::new(),
        declared: Vec::new(),
        states: Vec::new(),
        resources: Vec::new(),
        wit_resources: Vec::new(),
        impls: Vec::new(),
        marked: Vec::new(),
    };
    let (items, children) = scan.scan(items, 0)?;
    let mut tree = Module {
        ident: None,
        attrs: Vec::new(),
        vis: Visibility::Inherited,
        unsafety: None,
        items,
        generated: Vec::new(),
        children,
    };
    let Scan {
        modules,
        candidates,
        mut services,
        contributions,
        declared,
        mut states,
        mut resources,
        mut wit_resources,
        impls,
        marked,
        ..
    } = scan;
    // A service's methods are the pub fn of the impl blocks of its own module.
    for (scope, item) in &impls {
        let Type::Path(path) = &*item.self_ty else {
            continue;
        };
        let Some(service) = services
            .iter_mut()
            .find(|service| service.scope == *scope && path.path.is_ident(&service.ident))
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
    // Every marker goes on a struct of its kind, in the marker's module; the function's shape is
    // checked below.
    for marked in &marked {
        let fits = match marked.marker {
            Marker::Validate => states
                .iter()
                .any(|state| state.ident == marked.self_ty && state.scope == marked.scope),
            Marker::Open | Marker::Close => resources
                .iter()
                .any(|resource| resource.ident == marked.self_ty && resource.scope == marked.scope),
            Marker::Constructor => wit_resources
                .iter()
                .any(|resource| resource.ident == marked.self_ty && resource.scope == marked.scope),
        };
        if !fits {
            return Err(syn::Error::new(
                marked.span,
                format!(
                    "#[{}] marks a function of a {} struct; `{}` is not one",
                    marked.marker.name(),
                    marked.marker.place(),
                    marked.self_ty
                ),
            ));
        }
    }
    for resource in &mut wit_resources {
        let constructor = marked_function(
            &marked,
            Marker::Constructor,
            &resource.ident,
            resource.scope,
        )?;
        if let Some(marked) = constructor {
            let function = &marked.function;
            let returns_self = match result(&function.sig.output) {
                Some(Type::Path(path)) => {
                    path.path.is_ident("Self") || path.path.is_ident(&resource.ident)
                }
                _ => false,
            };
            let takes_self = matches!(function.sig.inputs.first(), Some(FnArg::Receiver(_)));
            if takes_self || !returns_self {
                return Err(syn::Error::new(
                    marked.span,
                    format!("#[constructor] marks {}", Marker::Constructor.shape()),
                ));
            }
            resource.constructor = Some(Constructor {
                ident: function.sig.ident.clone(),
                params: typed_params(function)?,
            });
        }
        for function in public_functions(&impls, &resource.ident, resource.scope) {
            if constructor.is_some_and(|marked| marked.function.sig.ident == function.sig.ident) {
                continue;
            }
            match resource_function(function)? {
                ResourceFunction::Method(method) => resource.methods.push(method),
                ResourceFunction::Static(function) => resource.statics.push(function),
            }
        }
    }
    for resource in &mut resources {
        let Some(open) = marked_function(&marked, Marker::Open, &resource.ident, resource.scope)?
        else {
            return Err(syn::Error::new(
                resource.ident.span(),
                format!(
                    "a resource needs a function marked #[open] returning `{0}` in an impl block; one marked #[close], taking `self`, is optional",
                    resource.ident
                ),
            ));
        };
        let signature = &open.function.sig;
        if !signature.inputs.is_empty() || matches!(signature.output, ReturnType::Default) {
            return Err(syn::Error::new(
                open.span,
                format!("#[open] marks {}", Marker::Open.shape()),
            ));
        }
        resource.open = Some(signature.ident.clone());
        if let Some(close) =
            marked_function(&marked, Marker::Close, &resource.ident, resource.scope)?
        {
            let signature = &close.function.sig;
            let takes_self = signature.inputs.len() == 1
                && matches!(signature.inputs.first(), Some(FnArg::Receiver(receiver))
                    if receiver.reference.is_none());
            if !takes_self {
                return Err(syn::Error::new(
                    close.span,
                    format!("#[close] marks {}", Marker::Close.shape()),
                ));
            }
            resource.close = Some(signature.ident.clone());
        }
    }
    for state in &mut states {
        if let Some(validator) =
            marked_function(&marked, Marker::Validate, &state.ident, state.scope)?
        {
            let signature = &validator.function.sig;
            let borrows_self = signature.inputs.len() == 1
                && matches!(signature.inputs.first(), Some(FnArg::Receiver(receiver))
                    if receiver.reference.is_some() && receiver.mutability.is_none());
            if !borrows_self || !result(&signature.output).is_some_and(refusal) {
                return Err(syn::Error::new(
                    validator.span,
                    format!("#[validate] marks {}", Marker::Validate.shape()),
                ));
            }
            state.validator = Some(signature.ident.clone());
        }
    }
    // An Orb exports services, declares hooks or contributes to them; a module doing none of
    // those would be an empty world.
    if services.is_empty() && contributions.is_empty() && declared.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "nothing to export: mark a unit struct with #[service] and give it an impl block of pub fn, or mark a hook with #[hook]",
        ));
    }
    if let Some(service) = services
        .iter()
        .find(|service| service.implements.is_none() && service.functions.is_empty())
    {
        return Err(syn::Error::new(
            service.ident.span(),
            "a service needs at least one pub fn in an impl block",
        ));
    }
    // The module names the world; WIT refuses a world and an interface with one name.
    if let Some(service) = services
        .iter()
        .find(|service| service.implements.is_none() && wit::kebab(&service.ident) == world)
    {
        return Err(syn::Error::new(
            service.ident.span(),
            format!(
                "`{}` is also the name of the world, taken from the module; rename one of them",
                service.ident
            ),
        ));
    }
    // A derived hook is an interface of the package too: its name is free of the world's and of
    // the derived services'.
    for declaration in declared
        .iter()
        .filter(|declaration| declaration.input.is_some())
    {
        let name = wit::kebab(&declaration.ident);
        if name == world {
            return Err(syn::Error::new(
                declaration.ident.span(),
                format!(
                    "`{}` is also the name of the world, taken from the module; rename one of them",
                    declaration.ident
                ),
            ));
        }
        if let Some(service) = services
            .iter()
            .find(|service| service.implements.is_none() && wit::kebab(&service.ident) == name)
        {
            return Err(syn::Error::new(
                declaration.ident.span(),
                format!(
                    "`{}` names both a hook and the service `{}`; rename one of them",
                    declaration.ident, service.ident
                ),
            ));
        }
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
    // Each state belongs to the objects of one service, the one named, the only one of its
    // module or the only one of the Orb, and an object declares exactly one state.
    let mut owners = BTreeMap::new();
    for state in &mut states {
        let owner = owner_of(
            &services,
            state.service.as_ref(),
            state.scope,
            state.ident.span(),
            "name the service whose objects own this state, #[state(service = Name)], or put the state in the service's sub-module",
        )?;
        if let Some(previous) = owners.insert(owner.ident.to_string(), state.ident.to_string()) {
            return Err(syn::Error::new(
                state.ident.span(),
                format!(
                    "`{}` already owns the state `{previous}`; an object declares exactly one state, so group the fields in one struct",
                    owner.ident
                ),
            ));
        }
        state.identity = owner.identity(&package);
        state.service = Some(owner.ident.clone());
    }
    // Each WIT resource belongs to the interface of one service, found the same way.
    for resource in &mut wit_resources {
        let owner = owner_of(
            &services,
            resource.service.as_ref(),
            resource.scope,
            resource.ident.span(),
            "name the service whose interface defines this resource, #[wit_resource(service = Name)], or put the resource in the service's sub-module",
        )?;
        resource.service = Some(owner.ident.clone());
    }
    // A resource is handed out, never taken: no parameter of a derived service, of a resource or
    // of a derived hook names one.
    let resource_names: BTreeSet<String> = wit_resources
        .iter()
        .map(|resource| resource.ident.to_string())
        .collect();
    let derived_functions = services
        .iter()
        .filter(|service| service.implements.is_none())
        .flat_map(|service| &service.functions)
        .chain(wit_resources.iter().flat_map(|resource| &resource.methods))
        .chain(wit_resources.iter().flat_map(|resource| &resource.statics));
    let constructor_params = wit_resources
        .iter()
        .filter_map(|resource| resource.constructor.as_ref())
        .flat_map(|constructor| &constructor.params);
    let input_ident = format_ident!("input");
    let hook_inputs: Vec<(&Type, usize)> = declared
        .iter()
        .filter_map(|declaration| {
            let (ty, _) = declaration.input.as_ref()?;
            Some((ty, declaration.scope))
        })
        .collect();
    for (ident, ty) in derived_functions
        .flat_map(|function| &function.params)
        .chain(constructor_params)
        .map(|(ident, ty, _)| (ident, ty))
        .chain(hook_inputs.iter().map(|(ty, _)| (&input_ident, *ty)))
    {
        let mut mentioned = Vec::new();
        wit::references(ty, &mut mentioned);
        if let Some(resource) = mentioned
            .iter()
            .find(|mention| resource_names.contains(&mention.to_string()))
        {
            return Err(syn::Error::new(
                ty.span(),
                format!(
                    "`{ident}` takes the resource `{resource}`: a resource is handed out, never taken"
                ),
            ));
        }
    }

    // Only the types a service method, a resource or a hook uses, directly or through another
    // type, enter the WIT; the others go back to their modules as ordinary Rust, a state with
    // serde derives.
    let mut unreached: Vec<(usize, Item)> = Vec::new();
    let shapes = reachable(
        candidates,
        &services,
        &wit_resources,
        &hook_inputs,
        &mut unreached,
    )?;
    for (scope, mut item) in unreached {
        if let Item::Struct(item) = &mut item
            && states
                .iter()
                .any(|state| state.ident == item.ident && state.scope == scope)
        {
            item.attrs
                .push(syn::parse_quote!(#[derive(::serde::Serialize, ::serde::Deserialize)]));
        }
        tree.at(&modules[scope]).items.push(item);
    }
    // A name that enters the WIT is one across the Orb module: the types, the services, the
    // derived hooks and the resources of every module share the package's names.
    {
        let mut names: BTreeMap<String, usize> = BTreeMap::new();
        let entries = shapes
            .iter()
            .map(|shape| (&shape.ident, shape.scope))
            .chain(
                services
                    .iter()
                    .map(|service| (&service.ident, service.scope)),
            )
            .chain(
                declared
                    .iter()
                    .filter(|declaration| declaration.input.is_some())
                    .map(|declaration| (&declaration.ident, declaration.scope)),
            )
            .chain(
                wit_resources
                    .iter()
                    .map(|resource| (&resource.ident, resource.scope)),
            )
            .chain(
                resources
                    .iter()
                    .map(|resource| (&resource.ident, resource.scope)),
            );
        for (ident, scope) in entries {
            if let Some(other) = names.insert(ident.to_string(), scope)
                && other != scope
            {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "`{ident}` is declared in {} and in {}; a name that enters the WIT is one across the Orb module",
                        describe(&modules[other]),
                        describe(&modules[scope])
                    ),
                ));
            }
        }
    }

    // Consumed packages: the callback of every hook, contributed or declared, and every interface
    // implemented or imported are read from their WIT packages.
    let wit_dir = options.wit.as_deref().map(wit::absolute).transpose()?;
    let implemented: Vec<String> = services
        .iter()
        .filter_map(|service| service.implements.clone())
        .collect();
    let consumed = match (
        &wit_dir,
        implemented.is_empty() && options.imports.is_empty(),
    ) {
        (_, true) => BTreeMap::new(),
        (Some(dir), false) => wit::consumed(dir, implemented.iter().chain(options.imports.iter()))?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "implementing or importing an interface needs wit = \"<dir>\", the directory with its WIT package",
            ));
        }
    };
    // An implemented interface is fixed by its WIT: the impl has exactly its functions, and each
    // of its resources is a #[wit_resource] struct of the same name with the resource's functions.
    for service in services
        .iter()
        .filter(|service| service.implements.is_some())
    {
        let identity = service.implements.as_deref().unwrap_or_default();
        let interface = &consumed[identity];
        for function in &interface.functions {
            if !service
                .functions
                .iter()
                .any(|own| wit::kebab(&own.ident) == function.name)
            {
                return Err(syn::Error::new(
                    service.ident.span(),
                    format!(
                        "`{}` implements {identity} but has no `pub fn {}`",
                        service.ident,
                        function.name.to_snake_case()
                    ),
                ));
            }
        }
        for own in &service.functions {
            if !interface
                .functions
                .iter()
                .any(|function| function.name == wit::kebab(&own.ident))
            {
                return Err(syn::Error::new(
                    own.ident.span(),
                    format!(
                        "`{}::{}` is not a function of {identity}; the interface is fixed by its WIT",
                        service.ident, own.ident
                    ),
                ));
            }
        }
        for resource in &interface.resources {
            let ident = resource.name.to_upper_camel_case();
            let Some(implementation) = wit_resources.iter().find(|candidate| {
                candidate.ident == ident && candidate.service.as_ref() == Some(&service.ident)
            }) else {
                return Err(syn::Error::new(
                    service.ident.span(),
                    format!(
                        "{identity} defines the resource `{}`: mark a struct `{ident}` with #[wit_resource(service = {})] and give it the resource's functions",
                        resource.name, service.ident
                    ),
                ));
            };
            match (&resource.constructor, &implementation.constructor) {
                (Some(_), None) => {
                    return Err(syn::Error::new(
                        implementation.ident.span(),
                        format!(
                            "`{ident}` implements the resource `{}` of {identity} but none of its functions is marked #[constructor]",
                            resource.name
                        ),
                    ));
                }
                (
                    None,
                    Some(Constructor {
                        ident: constructor, ..
                    }),
                ) => {
                    return Err(syn::Error::new(
                        constructor.span(),
                        format!(
                            "`{ident}::{constructor}` is marked #[constructor], but the resource `{}` of {identity} has no constructor; the interface is fixed by its WIT",
                            resource.name
                        ),
                    ));
                }
                _ => {}
            }
            let expected = resource
                .methods
                .iter()
                .chain(&resource.statics)
                .map(|function| function.name.to_snake_case());
            for name in expected {
                let defined = implementation
                    .methods
                    .iter()
                    .chain(&implementation.statics)
                    .any(|function| function.ident == name);
                if !defined {
                    return Err(syn::Error::new(
                        implementation.ident.span(),
                        format!(
                            "`{ident}` implements the resource `{}` of {identity} but has no `pub fn {name}`",
                            resource.name
                        ),
                    ));
                }
            }
        }
    }
    // The callback of a contributed hook, or of a declared one named from a consumed package, is
    // read from that package; a hook derived from `input = Type` needs no directory.
    let consumed_hooks: Vec<&String> = hooks
        .keys()
        .chain(
            declarations
                .iter()
                .filter(|(_, declaration)| declaration.input.is_none())
                .map(|(hook, _)| hook),
        )
        .collect();
    let callbacks = match (&wit_dir, consumed_hooks.is_empty()) {
        (_, true) => Default::default(),
        (Some(dir), false) => wit::callbacks(dir, consumed_hooks.iter().copied())?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "hooks need wit = \"<dir>\", the directory with the hook's WIT package",
            ));
        }
    };
    for declaration in declarations
        .values()
        .filter(|declaration| declaration.input.is_none())
    {
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
        wit::Items {
            shapes: &shapes,
            services: &services,
            resources: &wit_resources,
            declared: &declared,
        },
        hooks.keys(),
        &options.imports,
    );
    // What `lifecycle.describe` reports: the services as the host's contract check names them,
    // read back from the WIT just written, and the Orb's registration.
    let hook_identities: BTreeSet<String> = hooks
        .keys()
        .chain(declared.iter().map(|declaration| &declaration.hook))
        .cloned()
        .collect();
    let described = wit::described(&wit, wit_dir.as_deref(), &world, &hook_identities)?;
    let registration = registration()?;

    // Generate the Rust side: the bindings from the inline WIT, the author's types replaced by
    // the generated ones, the bridges to the author's functions, each in the module of the item
    // it serves, and the no_std prelude.
    let module_ident = module.ident;
    let vis = module.vis;
    let attrs = module.attrs;
    let ns_ident = format_ident!("{}", namespace.to_snake_case());
    let name_ident = format_ident!("{}", name.to_snake_case());
    // The generated types carry the author's names and public fields, so the author's functions
    // and the generated traits speak about one type: each is re-exported in the module that
    // declared it. A type a derived hook reaches is defined in the first derived hook's
    // interface, any other in the first derived service's.
    let hook_shapes = wit::hook_shapes(&shapes, &declared);
    let interface_module = |ident: &Ident| format_ident!("{}", ident.to_string().to_snake_case());
    let first_hook = declared
        .iter()
        .find(|declaration| declaration.input.is_some())
        .map(|declaration| interface_module(&declaration.ident));
    let first_service = services
        .iter()
        .find(|service| service.implements.is_none())
        .map(|service| interface_module(&service.ident));
    let mut grouped: Vec<((Ident, usize), Vec<&Ident>)> = Vec::new();
    for shape in &shapes {
        let interface = if hook_shapes.contains(&shape.ident.to_string()) {
            &first_hook
        } else {
            &first_service
        };
        let interface = interface
            .clone()
            .expect("a shape is reached from a hook or a derived service");
        let key = (interface, shape.scope);
        match grouped.iter_mut().find(|(known, _)| *known == key) {
            Some((_, idents)) => idents.push(&shape.ident),
            None => grouped.push((key, vec![&shape.ident])),
        }
    }
    for ((interface, scope), idents) in &grouped {
        let root = supers(modules[*scope].len());
        tree.at(&modules[*scope]).generated.push(quote! {
            pub use #root exports::#ns_ident::#name_ident::#interface::{#(#idents),*};
        });
    }
    // A derived service's bridge forwards each function to the author's; a result that is a
    // resource is wrapped into the bindings' handle type, and the resources of the service's
    // interface are the author's structs, each bridged in its own module.
    for service in services
        .iter()
        .filter(|service| service.implements.is_none())
    {
        let depth = modules[service.scope].len();
        let root = supers(depth);
        let interface = interface_module(&service.ident);
        let module = quote! { #root exports::#ns_ident::#name_ident::#interface };
        let target = &service.ident;
        let functions = service.functions.iter().map(|function| {
            let ident = &function.ident;
            let params = function
                .params
                .iter()
                .map(|(ident, ty, _)| quote! { #ident: #ty });
            let names = function.params.iter().map(|(ident, _, _)| ident);
            let call = quote! { #target::#ident(#(#names),*) };
            let (result, body) = derived_result(&function.result, &module, &resource_names, call);
            quote! { fn #ident(#(#params),*) #result { #body } }
        });
        let owned = wit_resources
            .iter()
            .filter(|resource| resource.service.as_ref() == Some(&service.ident))
            .map(|resource| {
                let ident = &resource.ident;
                let path = relative(depth, &modules[resource.scope], ident);
                quote! { type #ident = #path; }
            });
        tree.at(&modules[service.scope]).generated.push(quote! {
            impl #module::Guest for #root __TembraExports {
                #(#owned)*
                #(#functions)*
            }
        });
        for resource in wit_resources
            .iter()
            .filter(|resource| resource.service.as_ref() == Some(&service.ident))
        {
            let resource_root = supers(modules[resource.scope].len());
            let resource_module =
                quote! { #resource_root exports::#ns_ident::#name_ident::#interface };
            tree.at(&modules[resource.scope])
                .generated
                .push(derived_resource(
                    resource,
                    &resource_module,
                    &resource_names,
                ));
        }
    }
    // An implemented interface's bridge takes its signatures from the WIT: the functions forward
    // to the author's of the same names, and each resource of the interface is the author's
    // struct of the same name.
    for service in services
        .iter()
        .filter(|service| service.implements.is_some())
    {
        let identity = service.implements.as_deref().unwrap_or_default();
        let bridge = implemented_service(
            service,
            identity,
            &consumed[identity],
            &wit_resources,
            &modules,
        )?;
        tree.at(&modules[service.scope]).generated.push(bridge);
    }
    // Implemented and imported interfaces are re-exported under their own names, like hooks, in
    // the Orb module; a sub-module reaches them through `super::`.
    let implemented_modules = services
        .iter()
        .filter_map(|service| service.implements.as_deref())
        .map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            quote! { pub use exports::#ns::#package::#interface; }
        });
    // An imported interface becomes a module of the same name: its types and resources from the
    // bindings, and each freestanding function wrapped so the call first selects the reference
    // the Orb root declared to the provider's `main` object. The kernel refuses a dependency call
    // without a selection; a resource method targets its handle and needs none.
    let import_modules = options
        .imports
        .iter()
        .map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            let dir = wit_dir.as_deref().ok_or_else(|| {
                syn::Error::new(Span::call_site(), "import = \"...\" needs wit = \"<dir>\"")
            })?;
            let functions =
                wit::imported(dir, identity, &format!("super::{ns}::{package}::{interface}"))?;
            let binding = LitStr::new(identity, Span::call_site());
            let wrappers = functions
                .iter()
                .map(|function| {
                    let name = format_ident!("{}", function.name.to_snake_case());
                    let names: Vec<Ident> = function
                        .params
                        .iter()
                        .map(|(param, _)| format_ident!("{}", param.to_snake_case()))
                        .collect();
                    let types = function
                        .params
                        .iter()
                        .map(|(_, ty)| syn::parse_str::<Type>(ty))
                        .collect::<syn::Result<Vec<_>>>()?;
                    let result = function
                        .result
                        .as_deref()
                        .map(syn::parse_str::<Type>)
                        .transpose()?
                        .map(|ty| quote! { -> #ty });
                    let doc = format!(
                        " Call `{}` of the provider's `main` object, through the reference the Orb root declared.",
                        function.name
                    );
                    Ok(quote! {
                        #[doc = #doc]
                        pub fn #name(#(#names: #types),*) #result {
                            super::__tembra_orb::select_reference(#binding);
                            super::#ns::#package::#interface::#name(#(#names),*)
                        }
                    })
                })
                .collect::<syn::Result<Vec<_>>>()?;
            let doc = format!(" The interface `{identity}`, imported from another Orb.");
            Ok(quote! {
                #[doc = #doc]
                pub mod #interface {
                    extern crate alloc as __tembra_alloc;
                    pub use super::#ns::#package::#interface::*;
                    #(#wrappers)*
                }
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;
    let consumed_modules = implemented_modules.chain(import_modules);
    // The call's context, answered by the kernel's private `host` interface; the author writes
    // `context::principal()`. Every answer but `log` is a `Result`: after a cancellation the
    // kernel refuses `principal` and `cancel`, and the caller decides what that means.
    let context_module = options.context.then(|| {
        quote! {
            /// The context of the call running now, answered by the kernel.
            pub mod context {
                extern crate alloc;
                use alloc::string::String;
                use super::tembra::orb::host;
                pub use super::tembra::orb::host::{Address, Level};

                /// The key and principal of the object serving the current call, as the kernel
                /// gave them when it created the object.
                pub fn object() -> ::core::result::Result<Address, String> {
                    super::__tembra_orb::address()
                }

                /// The principal on whose behalf the current call runs.
                pub fn principal() -> ::core::result::Result<String, String> {
                    host::context_principal()
                }

                /// Whether the current call has been cancelled.
                pub fn cancelled() -> ::core::result::Result<bool, String> {
                    host::signal_cancelled()
                }

                /// Cancel the current call's root.
                pub fn cancel() -> ::core::result::Result<(), String> {
                    host::context_cancel()
                }

                /// Record a diagnostic, attributed by the host to this Orb.
                pub fn log(level: Level, message: &str) {
                    host::log(level, message)
                }
            }
        }
    });
    // Each hook's generated module is re-exported under its interface name: the export module
    // of a contributed hook, whose callback forwards to the author's function, and the import
    // module of a hook declared from a consumed package, whose types `run` takes and returns.
    let hook_modules = hooks
        .keys()
        .map(|hook| {
            let [ns, package, interface] = hook_path(hook);
            quote! { pub use exports::#ns::#package::#interface; }
        })
        .chain(
            declarations
                .iter()
                .filter(|(_, declaration)| declaration.input.is_none())
                .map(|(hook, _)| {
                    let [ns, package, interface] = hook_path(hook);
                    quote! { pub use self::#ns::#package::#interface; }
                }),
        );
    for contribution in &contributions {
        let depth = modules[contribution.scope].len();
        let root = supers(depth);
        let module = export_module(depth, &hook_path(&contribution.hook));
        let callback = format_ident!("{}", callbacks[&contribution.hook].name.to_snake_case());
        let target = &contribution.ident;
        let (param, ty) = &contribution.param;
        let result = contribution
            .result
            .as_ref()
            .map(|ty| quote! { -> #ty })
            .unwrap_or_default();
        tree.at(&modules[contribution.scope])
            .generated
            .push(quote! {
                impl #module::Guest for #root __TembraExports {
                    fn #callback(#param: #ty) #result { #target(#param) }
                }
            });
    }
    // The directory of the consumed packages, as written on the attribute, travels with the
    // component: `cargo tembra build` nests the whole packages from there into the contract,
    // where the component's own types would keep only the functions it calls.
    let consumed_section = options.wit.as_ref().map(|dir| {
        let bytes = LitByteStr::new(dir.as_bytes(), Span::call_site());
        let len = proc_macro2::Literal::usize_unsuffixed(dir.len());
        quote! {
            #[cfg(target_arch = "wasm32")]
            #[unsafe(link_section = "tembra:consumed")]
            #[used]
            static __TEMBRA_CONSUMED: [u8; #len] = *#bytes;
        }
    });
    // Signatures spelled from a consumed WIT name strings and lists from `alloc`, which a module
    // may not name: the alias goes in every module that holds such a signature.
    let alloc_scopes: BTreeSet<usize> = declared
        .iter()
        .filter(|declaration| declaration.input.is_none())
        .map(|declaration| declaration.scope)
        .chain(
            services
                .iter()
                .filter(|service| service.implements.is_some())
                .map(|service| service.scope),
        )
        .collect();
    for scope in &alloc_scopes {
        tree.at(&modules[*scope])
            .generated
            .push(quote! { extern crate alloc as __tembra_alloc; });
    }
    // The hooks: the export the contract requires and `run`, in the declaring module; the
    // conversions between the two sides of the derived hooks' types, once, in the Orb module.
    let defining = declared
        .iter()
        .find(|declaration| declaration.input.is_some())
        .map(|declaration| hook_path(&declaration.hook));
    for declaration in &declared {
        let depth = modules[declaration.scope].len();
        let code = declaration_code(
            declaration,
            &callbacks,
            &hook_shapes,
            defining.as_ref(),
            depth,
        )?;
        tree.at(&modules[declaration.scope]).generated.push(code);
    }
    let conversions = defining.as_ref().map(|path| {
        conversions(
            &shapes,
            &hook_shapes,
            &export_module(0, path),
            &import_module(0, path),
        )
    });
    let state_code = state_code(&states, &modules);
    let resource_code = resource_code(&resources, &modules);
    let lifecycle_code = lifecycle_code(Lifecycle {
        registration: &registration,
        described: &described,
        services: &services,
        package: &package,
        declared: &declared,
        contributions: &hooks,
        imports: &options.imports,
        context: options.context,
        states: &states,
        resources: &resources,
        modules: &modules,
    });
    let runtime_code = runtime_code(!states.is_empty(), !resources.is_empty(), options.context);
    let body = tree.tokens();
    let wit_literal = LitStr::new(&wit, Span::call_site());
    let wit_bytes = LitByteStr::new(wit.as_bytes(), Span::call_site());
    let wit_len = proc_macro2::Literal::usize_unsuffixed(wit.len());
    // The bindings read the author's WIT, the build world that adds the kernel's private
    // `tembra:orb` package and the declared hooks' imports, and that package nested after them;
    // the consumed packages come from their directory. Foreign interfaces need `generate_all`.
    let inline = LitStr::new(
        &format!(
            "{wit}{}\n{}",
            wit::build_world(&world, &declared),
            wit::private_package()
        ),
        Span::call_site(),
    );
    let build_world = LitStr::new(&format!("{world}-orb"), Span::call_site());
    let path = wit_dir.as_ref().map(|dir| {
        let dir = LitStr::new(&dir.display().to_string(), Span::call_site());
        quote! { path: #dir, }
    });
    // A state's record that a method mentions is a generated type, which the kernel's JSON needs
    // to serialize; the derives reach every generated type.
    let derives = (!states.is_empty())
        .then(|| quote! { additional_derives: [::serde::Serialize, ::serde::Deserialize], });
    let generate = quote! {
        ::wit_bindgen::generate!({
            #path
            inline: #inline,
            world: #build_world,
            generate_all,
            #derives
        });
    };
    // The registration lives in the crate manifest: including it makes a change to it rebuild
    // the component, as a change to the module does.
    let manifest = LitStr::new(
        &registration.manifest.display().to_string(),
        Span::call_site(),
    );
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

            // A panic traps the component; its message reaches the host's log first.
            #[cfg(target_arch = "wasm32")]
            #[panic_handler]
            fn __tembra_panic(info: &::core::panic::PanicInfo<'_>) -> ! {
                let message = __tembra_orb::format!("{info}");
                self::tembra::orb::host::log(self::tembra::orb::host::Level::Error, &message);
                ::core::arch::wasm32::unreachable()
            }

            const _: &[u8] = include_bytes!(#manifest);

            // The exact WIT of this module travels inside the component as a custom section, so
            // `cargo tembra build` packages the contract as written here: hook imports the
            // linker drops and `///` comments the binary type section loses.
            #[cfg(target_arch = "wasm32")]
            #[unsafe(link_section = "tembra:wit")]
            #[used]
            static __TEMBRA_WIT: [u8; #wit_len] = *#wit_bytes;

            #consumed_section

            #generate

            #context_module
            #(#consumed_modules)*
            #(#hook_modules)*

            #body

            struct __TembraExports;
            #conversions
            #lifecycle_code
            #runtime_code
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

/// Read a `#[service]` attribute: the identity of an interface the service implements, if it
/// names one first, then the ownership policy of its objects, if any: `lifetime = host` pins
/// the objects until the host closes, `linger_ms = n` keeps an unreferenced object for `n`
/// milliseconds; a host-lifetime object cannot linger, as the kernel rules.
fn service_attribute(attribute: &Attribute) -> syn::Result<(Option<String>, Option<Policy>)> {
    if matches!(attribute.meta, syn::Meta::Path(_)) {
        return Ok((None, None));
    }
    let span = attribute.span();
    attribute.parse_args_with(|input: syn::parse::ParseStream| {
        let mut implements = None;
        if input.peek(LitStr) {
            let identity: LitStr = input.parse()?;
            let value = identity.value();
            if !value.contains(':') || !value.contains('/') || value.contains(['"', '\\']) {
                return Err(syn::Error::new(
                    identity.span(),
                    "an implemented interface is named ns:name/interface@version",
                ));
            }
            implements = Some(value);
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        let (mut host, mut linger_ms, mut chosen) = (false, 0, false);
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "lifetime" {
                let value: Ident = input.parse()?;
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
                chosen = true;
            } else if key == "linger_ms" {
                let literal: LitInt = input.parse()?;
                linger_ms = literal.base10_parse::<u32>()?;
                chosen = true;
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "unknown option; use #[service(\"ns:name/interface@version\", lifetime = host, linger_ms = n)]",
                ));
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        if host && linger_ms != 0 {
            return Err(syn::Error::new(
                span,
                "host-lifetime objects cannot linger: they live until the host closes",
            ));
        }
        Ok((implements, chosen.then_some(Policy { host, linger_ms })))
    })
}

/// Read a WIT resource: a struct, with the service whose interface defines it.
fn wit_resource(item: &syn::ItemStruct, attribute: &Attribute) -> syn::Result<WitResource> {
    let mut service = None;
    if !matches!(attribute.meta, syn::Meta::Path(_)) {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("service") {
                service = Some(meta.value()?.parse::<Ident>()?);
            } else {
                return Err(meta.error("unknown option; use #[wit_resource(service = Name)]"));
            }
            Ok(())
        })?;
    }
    Ok(WitResource {
        ident: item.ident.clone(),
        docs: wit::docs(&item.attrs),
        service,
        constructor: None,
        methods: Vec::new(),
        statics: Vec::new(),
        scope: 0,
    })
}

/// The `pub fn` of a type's `impl` blocks, in the module `scope`.
fn public_functions<'a>(
    impls: &'a [(usize, syn::ItemImpl)],
    ident: &Ident,
    scope: usize,
) -> Vec<&'a syn::ImplItemFn> {
    impls
        .iter()
        .filter(|(module, item)| {
            *module == scope
                && matches!(&*item.self_ty, Type::Path(path) if path.path.is_ident(ident))
        })
        .flat_map(|(_, item)| &item.items)
        .filter_map(|item| match item {
            syn::ImplItem::Fn(function) if matches!(function.vis, Visibility::Public(_)) => {
                Some(function)
            }
            _ => None,
        })
        .collect()
}

/// One `pub fn` of a WIT resource, by its role: a function taking `&self` is a method, any other
/// a static function. The constructor is the function marked `#[constructor]`, read apart.
enum ResourceFunction {
    Method(Function),
    Static(Function),
}

/// Read one `pub fn` of a WIT resource; `&mut self` and `self` are refused.
fn resource_function(function: &syn::ImplItemFn) -> syn::Result<ResourceFunction> {
    let receiver = function.sig.inputs.first().and_then(|input| match input {
        FnArg::Receiver(receiver) => Some(receiver),
        FnArg::Typed(_) => None,
    });
    if let Some(receiver) = receiver
        && (receiver.reference.is_none() || receiver.mutability.is_some())
    {
        return Err(syn::Error::new(
            receiver.span(),
            "a WIT method borrows `&self`; change a resource through Cell or RefCell",
        ));
    }
    let read = Function {
        ident: function.sig.ident.clone(),
        docs: wit::docs(&function.attrs),
        params: typed_params(function)?,
        result: match result(&function.sig.output) {
            Some(ty) => Some((ty.clone(), wit::wit_type(ty)?)),
            None => None,
        },
    };
    Ok(match receiver {
        Some(_) => ResourceFunction::Method(read),
        None => ResourceFunction::Static(read),
    })
}

/// The typed parameters of a function, the receiver left out: name, Rust type and WIT spelling.
fn typed_params(function: &syn::ImplItemFn) -> syn::Result<Vec<(Ident, Type, String)>> {
    let mut params = Vec::new();
    for input in &function.sig.inputs {
        let FnArg::Typed(typed) = input else {
            continue;
        };
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
    Ok(params)
}

/// The result of a derived function's bridge: a resource is wrapped into the bindings' handle
/// type of the interface's module, any other value is returned as it is.
fn derived_result(
    result: &Option<(Type, String)>,
    module: &proc_macro2::TokenStream,
    resources: &BTreeSet<String>,
    call: proc_macro2::TokenStream,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let Some((ty, _)) = result else {
        return (quote! {}, call);
    };
    let resource = match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .filter(|segment| resources.contains(&segment.ident.to_string()))
            .map(|segment| segment.ident.clone()),
        _ => None,
    };
    match resource {
        Some(resource) => (
            quote! { -> #module::#resource },
            quote! { #module::#resource::new(#call) },
        ),
        None => (quote! { -> #ty }, call),
    }
}

/// The bindings' trait of a derived resource, implemented by the author's struct: the constructor,
/// the methods and the static functions forward to the author's `pub fn`.
fn derived_resource(
    resource: &WitResource,
    module: &proc_macro2::TokenStream,
    resources: &BTreeSet<String>,
) -> proc_macro2::TokenStream {
    let ident = &resource.ident;
    let guest = format_ident!("Guest{}", ident);
    let constructor = resource.constructor.as_ref().map(|constructor| {
        let names = constructor.params.iter().map(|(ident, _, _)| ident);
        let params = constructor
            .params
            .iter()
            .map(|(ident, ty, _)| quote! { #ident: #ty });
        let function = &constructor.ident;
        quote! { fn new(#(#params),*) -> Self { #ident::#function(#(#names),*) } }
    });
    let methods = resource.methods.iter().map(|function| {
        let name = &function.ident;
        let params = function
            .params
            .iter()
            .map(|(ident, ty, _)| quote! { #ident: #ty });
        let names = function.params.iter().map(|(ident, _, _)| ident);
        let call = quote! { #ident::#name(self, #(#names),*) };
        let (result, body) = derived_result(&function.result, module, resources, call);
        quote! { fn #name(&self, #(#params),*) #result { #body } }
    });
    let statics = resource.statics.iter().map(|function| {
        let name = &function.ident;
        let params = function
            .params
            .iter()
            .map(|(ident, ty, _)| quote! { #ident: #ty });
        let names = function.params.iter().map(|(ident, _, _)| ident);
        let call = quote! { #ident::#name(#(#names),*) };
        let (result, body) = derived_result(&function.result, module, resources, call);
        quote! { fn #name(#(#params),*) #result { #body } }
    });
    quote! {
        impl #module::#guest for #ident {
            #constructor
            #(#methods)*
            #(#statics)*
        }
    }
}

/// One function of an implemented interface as its bridge spells it: parameter names and
/// declarations, the result, and the resource a returned value is wrapped into, if any.
struct Signature {
    names: Vec<Ident>,
    params: Vec<proc_macro2::TokenStream>,
    result: proc_macro2::TokenStream,
    wrap: Option<Ident>,
}

/// The bridge of an implemented interface, with signatures spelled from its WIT: the functions
/// forward to the author's of the same names, and each resource of the interface is the author's
/// struct of the same name, whose constructor, methods and static functions forward likewise.
fn implemented_service(
    service: &Service,
    identity: &str,
    interface: &wit::Consumed,
    resources: &[WitResource],
    modules: &[Vec<Ident>],
) -> syn::Result<proc_macro2::TokenStream> {
    let depth = modules[service.scope].len();
    let root = supers(depth);
    let module = export_module(depth, &hook_path(identity));
    let target = &service.ident;
    let spelled =
        |spelling: &str| -> syn::Result<Type> { syn::parse_str(&rooted(spelling, depth)) };
    let signature = |function: &wit::ConsumedFunction| -> syn::Result<Signature> {
        let mut names = Vec::new();
        let mut params = Vec::new();
        for (param, spelling) in &function.params {
            let ident = format_ident!("{}", param.to_snake_case());
            let ty = spelled(&spelling.export)?;
            params.push(quote! { #ident: #ty });
            names.push(ident);
        }
        let result = match &function.result {
            Some(spelling) => {
                let ty = spelled(&spelling.export)?;
                quote! { -> #ty }
            }
            None => quote! {},
        };
        let wrap = function
            .result
            .as_ref()
            .and_then(|spelling| spelling.resource.as_deref())
            .map(|resource| format_ident!("{resource}"));
        Ok(Signature {
            names,
            params,
            result,
            wrap,
        })
    };
    let wrapped = |wrap: &Option<Ident>, call: proc_macro2::TokenStream| match wrap {
        Some(resource) => quote! { #module::#resource::new(#call) },
        None => call,
    };
    let mut functions = Vec::new();
    for function in &interface.functions {
        let ident = format_ident!("{}", function.name.to_snake_case());
        let Signature {
            names,
            params,
            result,
            wrap,
        } = signature(function)?;
        let body = wrapped(&wrap, quote! { #target::#ident(#(#names),*) });
        functions.push(quote! { fn #ident(#(#params),*) #result { #body } });
    }
    let mut owned = Vec::new();
    let mut implementations = Vec::new();
    for resource in &interface.resources {
        let ident = format_ident!("{}", resource.name.to_upper_camel_case());
        let Some(implementation) = resources.iter().find(|candidate| {
            candidate.ident == ident && candidate.service.as_ref() == Some(&service.ident)
        }) else {
            continue;
        };
        let struct_ident = relative(depth, &modules[implementation.scope], &implementation.ident);
        owned.push(quote! { type #ident = #struct_ident; });
        let guest = format_ident!("Guest{}", ident);
        let constructor = match (&resource.constructor, &implementation.constructor) {
            (
                Some(params),
                Some(Constructor {
                    ident: constructor, ..
                }),
            ) => {
                let Signature { names, params, .. } = signature(&wit::ConsumedFunction {
                    name: "new".into(),
                    params: params
                        .iter()
                        .map(|(name, spelling)| {
                            (
                                name.clone(),
                                wit::Spelled {
                                    import: spelling.import.clone(),
                                    export: spelling.export.clone(),
                                    resource: spelling.resource.clone(),
                                },
                            )
                        })
                        .collect(),
                    result: None,
                })?;
                quote! { fn new(#(#params),*) -> Self { #struct_ident::#constructor(#(#names),*) } }
            }
            _ => quote! {},
        };
        let mut members = Vec::new();
        for method in &resource.methods {
            let name = format_ident!("{}", method.name.to_snake_case());
            let Signature {
                names,
                params,
                result,
                wrap,
            } = signature(method)?;
            let body = wrapped(&wrap, quote! { #struct_ident::#name(self, #(#names),*) });
            members.push(quote! { fn #name(&self, #(#params),*) #result { #body } });
        }
        for function in &resource.statics {
            let name = format_ident!("{}", function.name.to_snake_case());
            let Signature {
                names,
                params,
                result,
                wrap,
            } = signature(function)?;
            let body = wrapped(&wrap, quote! { #struct_ident::#name(#(#names),*) });
            members.push(quote! { fn #name(#(#params),*) #result { #body } });
        }
        implementations.push(quote! {
            impl #module::#guest for #struct_ident {
                #constructor
                #(#members)*
            }
        });
    }
    Ok(quote! {
        impl #module::Guest for #root __TembraExports {
            #(#owned)*
            #(#functions)*
        }
        #(#implementations)*
    })
}

/// Keep, in module order, the candidate types that a derived service's method, a resource's
/// function or a derived hook's input reaches; the others go back to their modules as ordinary
/// Rust. A name resolves to the type of the module naming it, else to the one type of that name
/// in the Orb; a type the module does not declare is an error, and a resource is a type of its own.
fn reachable(
    candidates: Vec<(Item, Shape)>,
    services: &[Service],
    resources: &[WitResource],
    hook_inputs: &[(&Type, usize)],
    unreached: &mut Vec<(usize, Item)>,
) -> syn::Result<Vec<Shape>> {
    let mut declared: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, (_, shape)) in candidates.iter().enumerate() {
        declared
            .entry(shape.ident.to_string())
            .or_default()
            .push(index);
    }
    let mut pending: Vec<(Ident, usize)> = Vec::new();
    let mention = |ty: &Type, scope: usize, pending: &mut Vec<(Ident, usize)>| {
        let mut idents = Vec::new();
        wit::references(ty, &mut idents);
        pending.extend(idents.into_iter().map(|ident| (ident, scope)));
    };
    let mention_function =
        |function: &Function, scope: usize, pending: &mut Vec<(Ident, usize)>| {
            for (_, ty, _) in &function.params {
                mention(ty, scope, pending);
            }
            if let Some((ty, _)) = &function.result {
                mention(ty, scope, pending);
            }
        };
    for service in services
        .iter()
        .filter(|service| service.implements.is_none())
    {
        for function in &service.functions {
            mention_function(function, service.scope, &mut pending);
        }
    }
    for resource in resources {
        for (_, ty, _) in resource
            .constructor
            .iter()
            .flat_map(|constructor| &constructor.params)
        {
            mention(ty, resource.scope, &mut pending);
        }
        for function in resource.methods.iter().chain(&resource.statics) {
            mention_function(function, resource.scope, &mut pending);
        }
    }
    for (input, scope) in hook_inputs {
        mention(input, *scope, &mut pending);
    }
    let mut kept = BTreeSet::new();
    while let Some((ident, scope)) = pending.pop() {
        if resources.iter().any(|resource| resource.ident == ident) {
            continue;
        }
        let Some(indexes) = declared.get(&ident.to_string()) else {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{ident}` is used by a service method or a hook but is not a struct or enum of this module"
                ),
            ));
        };
        let index = match indexes
            .iter()
            .find(|&&index| candidates[index].1.scope == scope)
        {
            Some(&index) => index,
            None if indexes.len() == 1 => indexes[0],
            None => {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "`{ident}` is declared in several modules of the Orb; a module uses its own `{ident}`, or the types take different names"
                    ),
                ));
            }
        };
        if kept.insert(index) {
            let shape = &candidates[index].1;
            pending.extend(
                shape
                    .references
                    .iter()
                    .cloned()
                    .map(|reference| (reference, shape.scope)),
            );
        }
    }
    let mut shapes = Vec::new();
    for (index, (item, shape)) in candidates.into_iter().enumerate() {
        if kept.contains(&index) {
            shapes.push(shape);
        } else {
            unreached.push((shape.scope, item));
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
        scope: 0,
    })
}

/// Read a hook declaration: a unit struct, with the hook's name and its composition on the
/// attribute.
fn declaration(
    item: &syn::ItemStruct,
    attribute: &Attribute,
    package: &str,
) -> syn::Result<Declared> {
    const FORMS: &str = "#[hook(composition = series | waterfall | bail, input = Type)] derives the hook from the module, #[hook(\"ns:name/hook@version\", composition = ...)] names an existing one";
    if !matches!(item.fields, Fields::Unit) {
        return Err(syn::Error::new(
            attribute.span(),
            format!("a hook declaration is a unit struct: {FORMS}"),
        ));
    }
    let mut hook: Option<LitStr> = None;
    let mut composition = None;
    let mut input: Option<Type> = None;
    attribute.parse_args_with(|stream: syn::parse::ParseStream| {
        if stream.peek(LitStr) {
            hook = Some(stream.parse()?);
            if !stream.is_empty() {
                stream.parse::<Token![,]>()?;
            }
        }
        while !stream.is_empty() {
            let key: Ident = stream.parse()?;
            stream.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "composition" => {
                    let value: Ident = stream.parse()?;
                    composition = Some(match value.to_string().as_str() {
                        "series" => Composition::Series,
                        "waterfall" => Composition::Waterfall,
                        "bail" => Composition::Bail,
                        _ => {
                            return Err(syn::Error::new(
                                value.span(),
                                "the composition is series, waterfall or bail",
                            ));
                        }
                    });
                }
                "input" => input = Some(stream.parse()?),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown option; {FORMS}"),
                    ));
                }
            }
            if !stream.is_empty() {
                stream.parse::<Token![,]>()?;
            }
        }
        Ok(())
    })?;
    let Some(composition) = composition else {
        return Err(syn::Error::new(
            attribute.span(),
            format!("a declared hook names its composition; {FORMS}"),
        ));
    };
    let (hook, input) = match (hook, input) {
        (Some(hook), None) => {
            let id = hook.value();
            if !id.contains(':') || !id.contains('/') || id.contains(['"', '\\']) {
                return Err(syn::Error::new(
                    hook.span(),
                    "a hook is named ns:name/interface@version",
                ));
            }
            (id, None)
        }
        (None, Some(input)) => {
            // The input crosses to the import by value, or by reference for a string or a list;
            // the bindings take other containers in borrowed forms the macro does not spell.
            let plain =
                match &input {
                    Type::Path(path) => !path.path.segments.last().is_some_and(|segment| {
                        segment.ident == "Option" || segment.ident == "Result"
                    }),
                    _ => false,
                };
            if !plain {
                return Err(syn::Error::new(
                    input.span(),
                    "a hook's input is a record, an enum, a variant, a scalar, a string or a list",
                ));
            }
            let spelling = wit::wit_type(&input)?;
            (
                identity(package, &wit::kebab(&item.ident)),
                Some((input, spelling)),
            )
        }
        (Some(hook), Some(_)) => {
            return Err(syn::Error::new(
                hook.span(),
                format!(
                    "a declared hook either names an existing interface or derives one from its input, not both; {FORMS}"
                ),
            ));
        }
        (None, None) => {
            return Err(syn::Error::new(
                attribute.span(),
                format!("a declared hook names an existing interface or its input type; {FORMS}"),
            ));
        }
    };
    Ok(Declared {
        hook,
        composition,
        ident: item.ident.clone(),
        docs: wit::docs(&item.attrs),
        input,
        scope: 0,
    })
}

/// The code of one declared hook, for the module declaring it: the export the contract requires,
/// which traps because the contributions answer the hook, and `run` on the author's struct, which
/// calls the matching import so the kernel runs the plan. A derived hook's `run` speaks the
/// module's own types and converts them to and from the import side of the bindings.
fn declaration_code(
    declaration: &Declared,
    callbacks: &BTreeMap<String, wit::Callback>,
    hook_shapes: &BTreeSet<String>,
    defining: Option<&[Ident; 3]>,
    depth: usize,
) -> syn::Result<proc_macro2::TokenStream> {
    let path = hook_path(&declaration.hook);
    let root = supers(depth);
    let export = export_module(depth, &path);
    let import = import_module(depth, &path);
    if let Some((input, _)) = &declaration.input {
        let import_types = import_module(depth, defining.expect("a derived hook defines types"));
        return Ok(derived_hook_code(
            declaration,
            input,
            hook_shapes,
            &import_types,
            &export,
            &import,
            &root,
        ));
    }
    let callback = &callbacks[&declaration.hook];
    let name = format_ident!("{}", callback.name.to_snake_case());
    let export_input: Type = syn::parse_str(&rooted(&callback.input.export, depth))?;
    let import_input: Type = syn::parse_str(&rooted(&callback.input.import, depth))?;
    let output = |spelling: &str| -> syn::Result<proc_macro2::TokenStream> {
        let ty: Type = syn::parse_str(&rooted(spelling, depth))?;
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
    let alias = alias_of(&declaration.hook);
    let message = format!(
        "the hook {} is declared by this Orb: its contributions answer it, this export never does",
        declaration.hook
    );
    let doc = format!(
        " Run the hook `{}`: the kernel calls its contributions in plan order and returns the composed result.",
        declaration.hook
    );
    Ok(quote! {
        impl #export::Guest for #root __TembraExports {
            fn #name(_input: #export_input) #export_result {
                ::core::panic!(#message)
            }
        }
        impl #ident {
            #[doc = #doc]
            pub fn run(input: #import_input) #import_result {
                #root __tembra_orb::select_hook(#alias);
                #import::#name(input)
            }
        }
    })
}

/// The code of a hook derived from `input = Type`: the export the contract requires, which traps
/// because the contributions answer the hook, and `run` on the author's struct, which converts
/// the module's types to the import side of the bindings, calls the hook's import so the kernel
/// runs the plan, and converts the composed result back.
fn derived_hook_code(
    declaration: &Declared,
    input: &Type,
    hook_shapes: &BTreeSet<String>,
    import_types: &proc_macro2::TokenStream,
    export: &proc_macro2::TokenStream,
    import: &proc_macro2::TokenStream,
    root: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let ident = &declaration.ident;
    let import_input = respell(input, hook_shapes, import_types);
    let (result, settle) = match declaration.composition {
        Composition::Series => (quote! {}, None),
        Composition::Waterfall => (
            quote! { -> #input },
            Some(convert(input, quote! { answer })),
        ),
        Composition::Bail => {
            let item = convert(input, quote! { item });
            (
                quote! { -> ::core::option::Option<#input> },
                Some(quote! { answer.map(|item| #item) }),
            )
        }
    };
    let message = format!(
        "the hook {} is declared by this Orb: its contributions answer it, this export never does",
        declaration.hook
    );
    let doc = format!(
        " Run the hook `{}`: the kernel calls its contributions in plan order and returns the composed result.",
        declaration.hook
    );
    let converted = convert(input, quote! { input });
    // A string or a list crosses to an import by reference, as the bindings take them.
    let by_reference = matches!(input, Type::Path(path)
        if path.path.segments.last().is_some_and(|segment| segment.ident == "String" || segment.ident == "Vec"));
    let argument = if by_reference {
        quote! { &input }
    } else {
        quote! { input }
    };
    let call = quote! { #import::apply(#argument) };
    let body = match settle {
        Some(settle) => quote! { let answer = #call; #settle },
        None => quote! { #call; },
    };
    let alias = alias_of(&declaration.hook);
    quote! {
        impl #export::Guest for #root __TembraExports {
            fn apply(_input: #input) #result {
                ::core::panic!(#message)
            }
        }
        impl #ident {
            #[doc = #doc]
            pub fn run(input: #input) #result {
                let input: #import_input = #converted;
                #root __tembra_orb::select_hook(#alias);
                #body
            }
        }
    }
}

/// `From` in both directions between the two sides of the bindings for each type a derived hook
/// reaches: the export side, which the module's functions use, and the import side, which the
/// typed entry to the plan uses. The two are distinct Rust types of one WIT definition.
fn conversions(
    shapes: &[Shape],
    hook_shapes: &BTreeSet<String>,
    export: &proc_macro2::TokenStream,
    import: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let impls = shapes
        .iter()
        .filter(|shape| hook_shapes.contains(&shape.ident.to_string()))
        .map(|shape| {
            let ident = &shape.ident;
            let body = |from: &proc_macro2::TokenStream| match &shape.kind {
                ShapeKind::Record(fields) => {
                    let fields = fields.iter().map(|(field, ty)| {
                        let value = convert(ty, quote! { value.#field });
                        quote! { #field: #value }
                    });
                    quote! { Self { #(#fields),* } }
                }
                ShapeKind::Enum(cases) => {
                    let arms = cases
                        .iter()
                        .map(|case| quote! { #from::#ident::#case => Self::#case });
                    quote! { match value { #(#arms,)* } }
                }
                ShapeKind::Variant(cases) => {
                    let arms = cases.iter().map(|(case, payload)| match payload {
                        Some(ty) => {
                            let item = convert(ty, quote! { item });
                            quote! { #from::#ident::#case(item) => Self::#case(#item) }
                        }
                        None => quote! { #from::#ident::#case => Self::#case },
                    });
                    quote! { match value { #(#arms,)* } }
                }
            };
            let to_import = body(export);
            let to_export = body(import);
            quote! {
                impl ::core::convert::From<#export::#ident> for #import::#ident {
                    fn from(value: #export::#ident) -> Self { #to_import }
                }
                impl ::core::convert::From<#import::#ident> for #export::#ident {
                    fn from(value: #import::#ident) -> Self { #to_export }
                }
            }
        });
    quote! { #(#impls)* }
}

/// An expression converting `value`, of the author's type `ty` on one side of the bindings, to the
/// other side: containers are rebuilt element by element, everything else goes through `Into`,
/// the identity for scalars and strings, the generated `From` for the module's types.
fn convert(ty: &Type, value: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    match ty {
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return quote! { ::core::convert::Into::into(#value) };
            };
            let arguments: Vec<&Type> = match &segment.arguments {
                syn::PathArguments::AngleBracketed(arguments) => arguments
                    .args
                    .iter()
                    .filter_map(|argument| match argument {
                        syn::GenericArgument::Type(ty) => Some(ty),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            match (segment.ident.to_string().as_str(), arguments.as_slice()) {
                ("Vec", [item]) => {
                    let item = convert(item, quote! { item });
                    quote! { #value.into_iter().map(|item| #item).collect() }
                }
                ("Option", [item]) => {
                    let item = convert(item, quote! { item });
                    quote! { #value.map(|item| #item) }
                }
                ("Result", [ok, err]) => {
                    let ok = convert(ok, quote! { item });
                    let err = convert(err, quote! { item });
                    quote! { match #value { Ok(item) => Ok(#ok), Err(item) => Err(#err) } }
                }
                _ => quote! { ::core::convert::Into::into(#value) },
            }
        }
        Type::Tuple(tuple) if tuple.elems.is_empty() => value,
        Type::Tuple(tuple) => {
            let names: Vec<Ident> = (0..tuple.elems.len())
                .map(|index| format_ident!("item{index}"))
                .collect();
            let converted = tuple
                .elems
                .iter()
                .zip(&names)
                .map(|(ty, name)| convert(ty, quote! { #name }));
            quote! { { let (#(#names),*) = #value; (#(#converted),*) } }
        }
        _ => quote! { ::core::convert::Into::into(#value) },
    }
}

/// The author's type as the import side of the bindings spells it: a type of the module that a
/// hook reaches lives in `module` there, everything else keeps its spelling.
fn respell(
    ty: &Type,
    hook_shapes: &BTreeSet<String>,
    module: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    match ty {
        Type::Path(path) => {
            let segments: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
            let Some((last, prefix)) = segments.split_last() else {
                return quote! { #ty };
            };
            let ident = &last.ident;
            if prefix.is_empty() && hook_shapes.contains(&ident.to_string()) {
                return quote! { #module::#ident };
            }
            let syn::PathArguments::AngleBracketed(arguments) = &last.arguments else {
                return quote! { #ty };
            };
            let leading = &path.path.leading_colon;
            let prefix = prefix.iter().map(|segment| quote! { #segment:: });
            let arguments = arguments.args.iter().map(|argument| match argument {
                syn::GenericArgument::Type(ty) => respell(ty, hook_shapes, module),
                other => quote! { #other },
            });
            quote! { #leading #(#prefix)* #ident<#(#arguments),*> }
        }
        Type::Tuple(tuple) => {
            let elements = tuple
                .elems
                .iter()
                .map(|ty| respell(ty, hook_shapes, module));
            quote! { (#(#elements,)*) }
        }
        _ => quote! { #ty },
    }
}

/// Whether a type is `Result<(), String>`, the answer of a state's validator.
fn refusal(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return false;
    };
    let mut arguments = arguments.args.iter();
    let (Some(ok), Some(err), None) = (arguments.next(), arguments.next(), arguments.next()) else {
        return false;
    };
    segment.ident == "Result"
        && matches!(ok, syn::GenericArgument::Type(Type::Tuple(unit)) if unit.elems.is_empty())
        && matches!(err, syn::GenericArgument::Type(Type::Path(path))
            if path.path.segments.last().is_some_and(|segment| segment.ident == "String"))
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
        validator: None,
        identity: String::new(),
        scope: 0,
    })
}

/// The accessors of each state type: `read` and `update` on the state of the object running the
/// current call, through the kernel's `host.state-read` and `host.state-update`. The declaration
/// and the validator live in [`lifecycle_code`].
fn state_code(states: &[State], modules: &[Vec<Ident>]) -> proc_macro2::TokenStream {
    let accessors = states.iter().map(|state| {
        let path = from_root(&modules[state.scope], &state.ident);
        let fields = &state.fields;
        let owner = alias_of(&state.identity);
        let name = state.ident.to_string();
        let service = state.service.as_ref().expect("resolved owner").to_string();
        let read_doc = format!(
            " The state of the `{service}` object running the current call, read from the kernel."
        );
        let (update_doc, check) = match &state.validator {
            Some(validator) => (
                format!(
                    " Change the state of the `{service}` object running the current call, validate it and store it in the kernel; returns the stored value, or the refusal (the validator's or the kernel's), in which case the kernel keeps the previous value."
                ),
                quote! { value.#validator()?; },
            ),
            None => (
                format!(
                    " Change the state of the `{service}` object running the current call and store it in the kernel; returns the stored value, or the kernel's refusal, in which case it keeps the previous value."
                ),
                quote! {},
            ),
        };
        quote! {
            impl ::core::default::Default for #path {
                fn default() -> Self {
                    Self { #(#fields: ::core::default::Default::default()),* }
                }
            }
            impl #path {
                #[doc = #read_doc]
                pub fn read() -> Self {
                    __tembra_orb::read(#owner, #name)
                }

                #[doc = #update_doc]
                pub fn update(
                    change: impl ::core::ops::FnOnce(&mut Self),
                ) -> ::core::result::Result<Self, __tembra_orb::String> {
                    let mut value = Self::read();
                    change(&mut value);
                    #check
                    __tembra_orb::update(#owner, #name, &value)?;
                    ::core::result::Result::Ok(value)
                }
            }
        }
    });
    quote! { #(#accessors)* }
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
        open: None,
        close: None,
        scope: 0,
    })
}

/// The static slot holding a root resource's value while the kernel keeps it open.
fn slot_ident(resource: &Resource) -> Ident {
    format_ident!(
        "__TEMBRA_RESOURCE_{}",
        resource.ident.to_string().to_shouty_snake_case()
    )
}

/// The accessor of each root resource: `with` on its value, after the kernel's
/// `host.resource-read` confirms the resource is open. The declaration, the opener and the closer
/// live in [`lifecycle_code`].
fn resource_code(resources: &[Resource], modules: &[Vec<Ident>]) -> proc_macro2::TokenStream {
    let slots = resources.iter().map(|resource| {
        let path = from_root(&modules[resource.scope], &resource.ident);
        let slot = slot_ident(resource);
        let name = wit::kebab(&resource.ident);
        let doc = format!(
            " Use the resource `{name}` of this Orb's root, opened by the kernel at activation; the kernel confirms it is open, the value lives in this component."
        );
        quote! {
            static #slot: __tembra_orb::Slot<#path> = __tembra_orb::Slot::new();
            impl #path {
                #[doc = #doc]
                pub fn with<R>(use_: impl ::core::ops::FnOnce(&mut Self) -> R) -> R {
                    __tembra_orb::read_resource(#name);
                    #slot.with(use_)
                }
            }
        }
    });
    quote! { #(#slots)* }
}

/// The JavaScript alias the host gives an interface, read from its identity
/// `ns:package/name@version`: the lower camel case of `name`, as `heck` spells it for the host.
fn alias_of(identity: &str) -> String {
    let name = identity.rsplit_once('/').map_or(identity, |(_, name)| name);
    let name = name.split_once('@').map_or(name, |(name, _)| name);
    name.to_lower_camel_case()
}

/// The Orb's name and API version, read where `cargo tembra` reads them: the crate manifest's
/// `[package.metadata.tembra]`, defaulting to the package name and 1. `lifecycle.describe`
/// reports them, and the host refuses an artifact registered under other ones.
struct Registration {
    name: String,
    api_version: u32,
    /// The crate manifest, which the expansion includes so a change to it rebuilds the component.
    manifest: PathBuf,
}

fn registration() -> syn::Result<Registration> {
    let invalid = |message: String| syn::Error::new(Span::call_site(), message);
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| invalid("CARGO_MANIFEST_DIR is not set".into()))?;
    let manifest = PathBuf::from(dir).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|cause| invalid(format!("cannot read {}: {cause}", manifest.display())))?;
    let table: toml::Table = text
        .parse()
        .map_err(|cause| invalid(format!("cannot parse {}: {cause}", manifest.display())))?;
    let package = table.get("package").and_then(toml::Value::as_table);
    let metadata = package
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("tembra"));
    let name = metadata
        .and_then(|metadata| metadata.get("name"))
        .or_else(|| package.and_then(|package| package.get("name")))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| invalid(format!("{} names no package", manifest.display())))?
        .to_owned();
    let api_version = match metadata.and_then(|metadata| metadata.get("api-version")) {
        None => 1,
        Some(value) => value
            .as_integer()
            .and_then(|version| u32::try_from(version).ok())
            .filter(|version| *version > 0)
            .ok_or_else(|| {
                invalid("[package.metadata.tembra] api-version is a positive integer".into())
            })?,
    };
    Ok(Registration {
        name,
        api_version,
        manifest,
    })
}

/// What [`lifecycle_code`] reads from the expansion.
struct Lifecycle<'a> {
    registration: &'a Registration,
    described: &'a [wit::Described],
    services: &'a [Service],
    package: &'a str,
    declared: &'a [Declared],
    contributions: &'a BTreeMap<String, i32>,
    imports: &'a [String],
    context: bool,
    states: &'a [State],
    resources: &'a [Resource],
    modules: &'a [Vec<Ident>],
}

/// The kernel's `tembra:orb/lifecycle`, which the host calls around the services.
///
/// `describe` reports the registration, the services with their lifetime and linger, the hooks
/// the module declares and the hooks it contributes to; the host checks it against the contract
/// before anything runs. `create` runs once for the Orb root, which declares its hooks and root
/// resources, and once for each service object, which declares its state with a validator.
/// `validate` is that validator: the retained JSON must parse as the state type, then pass the
/// author's `#[validate]` function if there is one, so a reload whose new code cannot read the
/// state is refused instead of trapping on the first read. `open` and `close` run the root
/// resources' `#[open]` and `#[close]` functions. Callback numbers are the item's position, from 1.
fn lifecycle_code(lifecycle: Lifecycle<'_>) -> proc_macro2::TokenStream {
    let Lifecycle {
        registration,
        described,
        services,
        package,
        declared,
        contributions,
        imports,
        context,
        states,
        resources,
        modules,
    } = lifecycle;
    let name = LitStr::new(&registration.name, Span::call_site());
    let api_version = registration.api_version;
    let number = |index: usize| u32::try_from(index + 1).expect("fewer than 2^32 items");
    let described_services = described.iter().map(|service| {
        let alias = LitStr::new(&service.alias, Span::call_site());
        let methods = service
            .methods
            .iter()
            .map(|method| LitStr::new(method, Span::call_site()));
        let policy = services
            .iter()
            .find(|candidate| candidate.identity(package) == service.identity)
            .and_then(|service| service.policy.as_ref());
        let lifetime = match policy {
            Some(policy) if policy.host => {
                quote! { ::core::option::Option::Some(lifecycle::Lifetime::Host) }
            }
            _ => quote! { ::core::option::Option::None },
        };
        let linger = match policy.map(|policy| policy.linger_ms).filter(|ms| *ms != 0) {
            Some(linger) => quote! { ::core::option::Option::Some(#linger) },
            None => quote! { ::core::option::Option::None },
        };
        quote! {
            lifecycle::Service {
                alias: #alias.into(),
                methods: __tembra_orb::vec![#(#methods.into()),*],
                lifetime: #lifetime,
                linger: #linger,
                access: false,
            }
        }
    });
    let described_hooks = declared.iter().map(|declaration| {
        let alias = alias_of(&declaration.hook);
        let composition = format_ident!("{}", declaration.composition.name().to_upper_camel_case());
        quote! {
            lifecycle::Hook {
                alias: #alias.into(),
                composition: lifecycle::Composition::#composition,
            }
        }
    });
    let described_contributions = contributions.iter().map(|(hook, priority)| {
        quote! {
            lifecycle::Contribution { binding: #hook.into(), priority: #priority }
        }
    });
    let hook_declarations = declared.iter().map(|declaration| {
        let alias = alias_of(&declaration.hook);
        quote! {
            root.hooks.insert(#alias, self::tembra::orb::host::hook_declare(#alias)?);
        }
    });
    let reference_declarations = imports.iter().map(|binding| {
        quote! {
            root.references.insert(
                #binding,
                self::tembra::orb::host::reference_declare(
                    #binding,
                    &[__tembra_orb::String::from("main")],
                )?,
            );
        }
    });
    let resource_declarations = resources.iter().enumerate().map(|(index, resource)| {
        let name = wit::kebab(&resource.ident);
        let version = resource.version;
        let callback = number(index);
        quote! {
            root.resources.insert(
                #name,
                self::tembra::orb::host::resource_declare(#name, #version, #callback, #callback, &[])?,
            );
        }
    });
    let state_declarations = states.iter().enumerate().map(|(index, state)| {
        let owner = alias_of(&state.identity);
        let path = from_root(&modules[state.scope], &state.ident);
        let version = state.version;
        let callback = number(index);
        quote! {
            ::core::option::Option::Some(#owner) => {
                let initial =
                    __tembra_orb::to_json(&<#path as ::core::default::Default>::default());
                let id = self::tembra::orb::host::state_declare(
                    &initial,
                    #version,
                    ::core::option::Option::Some(#callback),
                )?;
                __tembra_orb::STATES.with(|states| states.insert(environment, (#owner, id)));
                ::core::result::Result::Ok(())
            }
        }
    });
    let validators = states.iter().enumerate().map(|(index, state)| {
        let path = from_root(&modules[state.scope], &state.ident);
        let name = state.ident.to_string();
        let callback = number(index);
        let check = match &state.validator {
            Some(validator) => quote! { state.#validator() },
            None => quote! {
                let _ = state;
                ::core::result::Result::Ok(())
            },
        };
        quote! {
            #callback => {
                let state: #path = ::serde_json::from_str(&value).map_err(|error| {
                    __tembra_orb::format!("the retained state is not a `{}`: {error}", #name)
                })?;
                #check
            }
        }
    });
    let openers = resources.iter().enumerate().map(|(index, resource)| {
        let path = from_root(&modules[resource.scope], &resource.ident);
        let open = resource
            .open
            .as_ref()
            .expect("a resource has its #[open] function");
        let slot = slot_ident(resource);
        let callback = number(index);
        quote! {
            #callback => {
                #slot.open(#path::#open());
                ::core::result::Result::Ok(#callback)
            }
        }
    });
    let closers = resources.iter().enumerate().map(|(index, resource)| {
        let slot = slot_ident(resource);
        let callback = number(index);
        let close = match &resource.close {
            Some(close) => quote! {
                if let ::core::option::Option::Some(value) = #slot.close() {
                    value.#close();
                }
            },
            None => quote! { #slot.close(); },
        };
        quote! {
            #callback => {
                #close
                ::core::result::Result::Ok(())
            }
        }
    });
    // The kernel answers an object's address only while it creates the object: kept for
    // `context::object()` in the object's calls.
    let address = context.then(|| {
        quote! {
            let address = self::tembra::orb::host::instance_address()?;
            __tembra_orb::ADDRESSES.with(|addresses| addresses.insert(environment, address));
        }
    });
    quote! {
        impl exports::tembra::orb::lifecycle::Guest for __TembraExports {
            fn describe() -> exports::tembra::orb::lifecycle::Implementation {
                use exports::tembra::orb::lifecycle;
                lifecycle::Implementation {
                    name: #name.into(),
                    api_version: #api_version,
                    services: __tembra_orb::vec![#(#described_services),*],
                    hooks: __tembra_orb::vec![#(#described_hooks),*],
                    contributes: __tembra_orb::vec![#(#described_contributions),*],
                }
            }

            fn create(
                environment: u64,
                service: ::core::option::Option<__tembra_orb::String>,
            ) -> ::core::result::Result<(), __tembra_orb::String> {
                #address
                match service.as_deref() {
                    ::core::option::Option::None => {
                        #[allow(unused_mut)]
                        let mut root = __tembra_orb::Root::new(environment);
                        #(#hook_declarations)*
                        #(#reference_declarations)*
                        #(#resource_declarations)*
                        __tembra_orb::ROOT.with(|slot| *slot = ::core::option::Option::Some(root));
                        ::core::result::Result::Ok(())
                    }
                    #(#state_declarations)*
                    ::core::option::Option::Some(_) => ::core::result::Result::Ok(()),
                }
            }

            fn release(environment: u64) {
                __tembra_orb::release(environment)
            }

            fn validate(
                callback: u32,
                value: __tembra_orb::String,
            ) -> ::core::result::Result<(), __tembra_orb::String> {
                let _ = &value;
                match callback {
                    #(#validators)*
                    _ => ::core::result::Result::Err(
                        __tembra_orb::format!("unknown state validator {callback}"),
                    ),
                }
            }

            async fn open(callback: u32) -> ::core::result::Result<u32, __tembra_orb::String> {
                match callback {
                    #(#openers)*
                    _ => ::core::result::Result::Err(
                        __tembra_orb::format!("unknown resource opener {callback}"),
                    ),
                }
            }

            async fn close(
                callback: u32,
                _value: u32,
            ) -> ::core::result::Result<(), __tembra_orb::String> {
                match callback {
                    #(#closers)*
                    _ => ::core::result::Result::Err(
                        __tembra_orb::format!("unknown resource closer {callback}"),
                    ),
                }
            }
        }
    }
}

/// The runtime the lifecycle and the accessors share: the Orb root's handles, each service
/// object's state handle, and the slots of the root resources.
///
/// A component instance serves one revision of one Orb, so it has one root. A state belongs to
/// the objects of one service: an accessor called while another service's object runs refuses,
/// instead of reading or writing that object's state.
fn runtime_code(states: bool, resources: bool, context: bool) -> proc_macro2::TokenStream {
    let state = states.then(|| {
        quote! {
            /// Environment of each service object with a state -> its service's alias and the
            /// kernel's handle of the state.
            pub static STATES: Global<BTreeMap<u64, (&'static str, u32)>> =
                Global::new(BTreeMap::new());

            fn state(owner: &str, name: &str) -> Handle {
                let environment = host::environment();
                let Some((service, id)) =
                    STATES.with(|states| states.get(&environment).copied())
                else {
                    ::core::panic!(
                        "the `{name}` state belongs to the `{owner}` service; this call serves an object without a state"
                    )
                };
                if service != owner {
                    ::core::panic!(
                        "the `{name}` state belongs to the `{owner}` service; this call serves `{service}`"
                    )
                }
                Handle { scope: Some(environment), id }
            }

            pub fn read<T: ::serde::de::DeserializeOwned>(owner: &str, name: &str) -> T {
                let snapshot = host::state_read(state(owner, name), None)
                    .unwrap_or_else(|error| ::core::panic!("the `{name}` state cannot be read: {error}"));
                let text = snapshot
                    .value
                    .unwrap_or_else(|| ::core::panic!("the kernel sent no `{name}` value"));
                ::serde_json::from_str(&text).unwrap_or_else(|error| {
                    ::core::panic!("the kernel's state is not a `{name}`: {error}")
                })
            }

            pub fn update<T: ::serde::Serialize>(
                owner: &str,
                name: &str,
                value: &T,
            ) -> Result<(), String> {
                host::state_update(state(owner, name), &to_json(value)).map(drop)
            }

            pub fn to_json<T: ::serde::Serialize>(value: &T) -> String {
                ::serde_json::to_string(value).expect("a state serializes to JSON")
            }
        }
    });
    let release_state =
        states.then(|| quote! { STATES.with(|states| states.remove(&environment)); });
    let address = context.then(|| {
        quote! {
            /// Environment of each object -> its address, as the kernel gave it at creation.
            pub static ADDRESSES: Global<BTreeMap<u64, super::tembra::orb::host::Address>> =
                Global::new(BTreeMap::new());

            /// The address of the object serving the current call.
            pub fn address() -> Result<super::tembra::orb::host::Address, String> {
                let environment = host::environment();
                ADDRESSES
                    .with(|addresses| addresses.get(&environment).cloned())
                    .ok_or_else(|| format!("no address is known for environment {environment}"))
            }
        }
    });
    let release_address =
        context.then(|| quote! { ADDRESSES.with(|addresses| addresses.remove(&environment)); });
    let resource = resources.then(|| {
        quote! {
            /// One root resource's value, absent until the kernel opens it and after it closes it.
            pub struct Slot<T>(Global<Option<T>>);

            impl<T> Slot<T> {
                pub const fn new() -> Self {
                    Self(Global::new(None))
                }

                pub fn open(&self, value: T) {
                    self.0.with(|slot| *slot = Some(value));
                }

                pub fn close(&self) -> Option<T> {
                    self.0.with(Option::take)
                }

                pub fn with<R>(&self, use_: impl FnOnce(&mut T) -> R) -> R {
                    self.0.with(|slot| use_(slot.as_mut().expect("the resource is not open")))
                }
            }

            /// Ask the kernel whether the root resource `name` is open.
            pub fn read_resource(name: &str) {
                let resource = root_handle("resource", name, |root| root.resources.get(name).copied());
                if let Err(error) = host::resource_read(resource) {
                    ::core::panic!("the resource {name} is not open: {error}")
                }
            }
        }
    });
    quote! {
        #[doc(hidden)]
        pub mod __tembra_orb {
            extern crate alloc;
            pub use alloc::{collections::BTreeMap, format, string::String, vec};
            use super::tembra::orb::host::{self, Handle};

            /// A component global: one export runs at a time, on one thread.
            pub struct Global<T>(::core::cell::RefCell<T>);

            // SAFETY: a component instance runs one export at a time on one thread, so a global
            // is never reached from two threads; the RefCell refuses a reentrant use at run time.
            unsafe impl<T> Sync for Global<T> {}

            impl<T> Global<T> {
                pub const fn new(value: T) -> Self {
                    Self(::core::cell::RefCell::new(value))
                }

                pub fn with<R>(&self, use_: impl FnOnce(&mut T) -> R) -> R {
                    use_(&mut self.0.borrow_mut())
                }
            }

            /// The Orb root's environment and the handles its `create` declared: its hooks, its
            /// references to the providers of its imports, and its resources.
            pub struct Root {
                pub environment: u64,
                pub hooks: BTreeMap<&'static str, u32>,
                pub references: BTreeMap<&'static str, u32>,
                pub resources: BTreeMap<&'static str, u32>,
            }

            impl Root {
                pub fn new(environment: u64) -> Self {
                    Self {
                        environment,
                        hooks: BTreeMap::new(),
                        references: BTreeMap::new(),
                        resources: BTreeMap::new(),
                    }
                }
            }

            pub static ROOT: Global<Option<Root>> = Global::new(None);

            pub fn release(environment: u64) {
                #release_state
                #release_address
                ROOT.with(|root| {
                    if root.as_ref().is_some_and(|root| root.environment == environment) {
                        *root = None;
                    }
                });
            }

            /// A handle the root declared, scoped to the root: any object's call may use it.
            fn root_handle(kind: &str, name: &str, pick: impl FnOnce(&Root) -> Option<u32>) -> Handle {
                ROOT.with(|root| {
                    let Some(root) = root.as_ref() else {
                        ::core::panic!("the Orb root of this instance is not created")
                    };
                    let Some(id) = pick(root) else {
                        ::core::panic!("the Orb root declared no {kind} {name}")
                    };
                    Handle { scope: Some(root.environment), id }
                })
            }

            /// Select the declared hook `alias`: the next call of its imported interface, in this
            /// turn, runs the hook's plan.
            pub fn select_hook(alias: &str) {
                let hook = root_handle("hook", alias, |root| root.hooks.get(alias).copied());
                if let Err(error) = host::select_hook(Some(hook)) {
                    ::core::panic!("the hook {alias} cannot run: {error}")
                }
            }

            /// Select the root's reference to the provider of `binding`: the next call of one of
            /// the interface's functions, in this turn, goes to that object.
            pub fn select_reference(binding: &str) {
                let reference =
                    root_handle("reference to", binding, |root| root.references.get(binding).copied());
                if let Err(error) = host::select(Some(reference)) {
                    ::core::panic!("the provider of {binding} cannot be called: {error}")
                }
            }

            #state
            #resource
            #address
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
