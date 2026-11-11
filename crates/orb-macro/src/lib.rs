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

use heck::{ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
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
    /// The module imports the kernel's `tembra:guest/context` interface.
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
}

/// The one function of `ident`'s `impl` blocks carrying `marker`, if any; two are an error.
fn marked_function<'a>(
    marked: &'a [Marked],
    marker: Marker,
    ident: &Ident,
) -> syn::Result<Option<&'a Marked>> {
    let mut found = marked
        .iter()
        .filter(|candidate| candidate.marker == marker && &candidate.self_ty == ident);
    let first = found.next();
    if let Some(second) = found.next() {
        return Err(syn::Error::new(
            second.span,
            format!("`{ident}` has two functions marked #[{}]", marker.name()),
        ));
    }
    Ok(first)
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
    let mut wit_resources: Vec<WitResource> = Vec::new();
    let mut passthrough = Vec::new();
    let mut impls = Vec::new();
    let mut marked: Vec<Marked> = Vec::new();
    for item in items {
        match item {
            Item::Struct(mut s) => {
                if let Some(attribute) = take(&mut s.attrs, "hook") {
                    declared.push(declaration(&s, &attribute, &package)?);
                    passthrough.push(Item::Struct(s));
                } else if let Some(attribute) = take(&mut s.attrs, "wit_resource") {
                    wit_resources.push(wit_resource(&s, &attribute)?);
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
                    let (implements, policy) = service_attribute(&attribute)?;
                    services.push(Service {
                        ident: s.ident.clone(),
                        docs: wit::docs(&s.attrs),
                        functions: Vec::new(),
                        policy,
                        implements,
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
            Item::Impl(mut i) if i.trait_.is_none() => {
                if let Some(attribute) = i.attrs.iter().find(|a| a.path().is_ident("service")) {
                    return Err(syn::Error::new(
                        attribute.span(),
                        "put #[service] on the unit struct, not on its impl block",
                    ));
                }
                // The marked functions are read here and their markers removed: the compiler
                // knows no attribute of those names.
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
                    marked.push(Marked {
                        marker,
                        self_ty,
                        function: function.clone(),
                        span,
                    });
                }
                impls.push(i.clone());
                passthrough.push(Item::Impl(i));
            }
            Item::Impl(i) => {
                // A trait impl is copied through unread; a marker belongs to an inherent block.
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
    // Every marker goes on a struct of its kind; the function's shape is checked below.
    for marked in &marked {
        let fits = match marked.marker {
            Marker::Validate => states.iter().any(|state| state.ident == marked.self_ty),
            Marker::Open | Marker::Close => resources
                .iter()
                .any(|resource| resource.ident == marked.self_ty),
            Marker::Constructor => wit_resources
                .iter()
                .any(|resource| resource.ident == marked.self_ty),
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
        let constructor = marked_function(&marked, Marker::Constructor, &resource.ident)?;
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
        for function in public_functions(&impls, &resource.ident) {
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
        let Some(open) = marked_function(&marked, Marker::Open, &resource.ident)? else {
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
        if let Some(close) = marked_function(&marked, Marker::Close, &resource.ident)? {
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
        if let Some(validator) = marked_function(&marked, Marker::Validate, &state.ident)? {
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
        state.identity = owner.identity(&package);
        state.service = Some(owner.ident.clone());
    }
    // Each WIT resource belongs to the interface of one service.
    for resource in &mut wit_resources {
        let owner = match &resource.service {
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
                    resource.ident.span(),
                    "name the service whose interface defines this resource: #[wit_resource(service = Name)]",
                ));
            }
        };
        resource.service = Some(owner.ident.clone());
    }
    // A resource is handed out, never taken: no parameter of a derived service or of a
    // resource names one.
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
    let hook_inputs: Vec<&Type> = declared
        .iter()
        .filter_map(|declaration| declaration.input.as_ref())
        .map(|(ty, _)| ty)
        .collect();
    for (ident, ty) in derived_functions
        .flat_map(|function| &function.params)
        .chain(constructor_params)
        .map(|(ident, ty, _)| (ident, ty))
        .chain(hook_inputs.iter().map(|ty| (&input_ident, *ty)))
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

    // Only the types a service method or a resource uses, directly or through another type,
    // enter the WIT; a state no method mentions stays an ordinary struct, with serde derives.
    let shapes = reachable(
        candidates,
        &services,
        &wit_resources,
        &hook_inputs,
        &mut passthrough,
    )?;
    for item in &mut passthrough {
        if let Item::Struct(item) = item
            && states.iter().any(|state| state.ident == item.ident)
        {
            item.attrs
                .push(syn::parse_quote!(#[derive(::serde::Serialize, ::serde::Deserialize)]));
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
        consumed_hooks.iter().copied(),
        &options.imports,
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
    // The generated types carry the author's names and public fields, so the author's functions
    // and the generated traits speak about one type. A type a derived hook reaches is defined in
    // the first derived hook's interface, any other in the first derived service's.
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
    let mut grouped: Vec<(Ident, Vec<&Ident>)> = Vec::new();
    for shape in &shapes {
        let interface = if hook_shapes.contains(&shape.ident.to_string()) {
            &first_hook
        } else {
            &first_service
        };
        let interface = interface
            .clone()
            .expect("a shape is reached from a hook or a derived service");
        match grouped.iter_mut().find(|(known, _)| *known == interface) {
            Some((_, idents)) => idents.push(&shape.ident),
            None => grouped.push((interface, vec![&shape.ident])),
        }
    }
    let reexports = grouped.iter().map(|(interface, idents)| {
        quote! { pub use exports::#ns_ident::#name_ident::#interface::{#(#idents),*}; }
    });
    // A derived service's bridge forwards each function to the author's; a result that is a
    // resource is wrapped into the bindings' handle type, and the resources of the service's
    // interface are the author's structs.
    let bridges = services
        .iter()
        .filter(|service| service.implements.is_none())
        .map(|service| {
            let interface = format_ident!("{}", service.ident.to_string().to_snake_case());
            let module = quote! { exports::#ns_ident::#name_ident::#interface };
            let target = &service.ident;
            let functions = service.functions.iter().map(|function| {
                let ident = &function.ident;
                let params = function
                    .params
                    .iter()
                    .map(|(ident, ty, _)| quote! { #ident: #ty });
                let names = function.params.iter().map(|(ident, _, _)| ident);
                let call = quote! { #target::#ident(#(#names),*) };
                let (result, body) =
                    derived_result(&function.result, &module, &resource_names, call);
                quote! { fn #ident(#(#params),*) #result { #body } }
            });
            let owned = wit_resources
                .iter()
                .filter(|resource| resource.service.as_ref() == Some(&service.ident))
                .map(|resource| {
                    let ident = &resource.ident;
                    quote! { type #ident = #ident; }
                });
            let resource_impls = wit_resources
                .iter()
                .filter(|resource| resource.service.as_ref() == Some(&service.ident))
                .map(|resource| derived_resource(resource, &module, &resource_names));
            quote! {
                impl #module::Guest for __TembraExports {
                    #(#owned)*
                    #(#functions)*
                }
                #(#resource_impls)*
            }
        });
    // An implemented interface's bridge takes its signatures from the WIT: the functions forward
    // to the author's of the same names, and each resource of the interface is the author's
    // struct of the same name.
    let implemented_bridges = services
        .iter()
        .filter_map(|service| {
            let identity = service.implements.as_deref()?;
            Some(implemented_service(
                service,
                identity,
                &consumed[identity],
                &wit_resources,
            ))
        })
        .collect::<syn::Result<Vec<_>>>()?;
    // Implemented and imported interfaces are re-exported under their own names, like hooks.
    let consumed_modules = services
        .iter()
        .filter_map(|service| service.implements.as_deref())
        .map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            quote! { pub use exports::#ns::#package::#interface; }
        })
        .chain(options.imports.iter().map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            quote! { pub use self::#ns::#package::#interface; }
        }));
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
        .chain(
            declarations
                .iter()
                .filter(|(_, declaration)| declaration.input.is_none())
                .map(|(hook, _)| {
                    let [ns, package, interface] = hook_path(hook);
                    quote! { pub use self::#ns::#package::#interface; }
                }),
        );
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
                service.identity(&package),
                policy.linger_ms
            ))
        })
        .collect::<Vec<_>>();
    let manifest_section = if hooks.is_empty()
        && declarations.is_empty()
        && policies.is_empty()
        && options.wit.is_none()
    {
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
        // The directory of the consumed packages, as written on the attribute: `cargo tembra
        // build` nests the whole packages from there, where the component's own types would
        // keep only the functions it calls.
        let wit_dir = match &options.wit {
            Some(dir) => format!(",\"wit\":{}", json_string(dir)),
            None => String::new(),
        };
        let json = format!(
            "{{\"contributes\":{{{contributes}}},\"declares\":{{{declares}}},\"services\":{{{services}}}{wit_dir}}}"
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
    // Signatures spelled from a consumed WIT name strings and lists from `alloc`, which the
    // module may not name.
    let alloc_alias = (!declared.is_empty() || !implemented.is_empty())
        .then(|| quote! { extern crate alloc as __tembra_alloc; });
    let state_code = state_code(&states);
    let resource_code = resource_code(&resources);
    let declaration_code = declaration_code(&declared, &callbacks, &shapes, &hook_shapes)?;
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
    // A state's record that a method mentions is a generated type, which the kernel's JSON needs
    // to serialize; the derives reach every generated type.
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

            #(#reexports)*
            #context_module
            #(#consumed_modules)*
            #(#hook_modules)*

            #(#passthrough)*

            #alloc_alias
            struct __TembraExports;
            #(#bridges)*
            #(#implemented_bridges)*
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

/// A JSON string literal of a path or name: quotes, backslashes and control characters escaped.
fn json_string(text: &str) -> String {
    let mut json = String::from('"');
    for character in text.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            character if character.is_control() => {
                json.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => json.push(character),
        }
    }
    json.push('"');
    json
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
    })
}

/// The `pub fn` of a type's `impl` blocks.
fn public_functions<'a>(impls: &'a [syn::ItemImpl], ident: &Ident) -> Vec<&'a syn::ImplItemFn> {
    impls
        .iter()
        .filter(|item| matches!(&*item.self_ty, Type::Path(path) if path.path.is_ident(ident)))
        .flat_map(|item| &item.items)
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
) -> syn::Result<proc_macro2::TokenStream> {
    let [ns, package, name] = hook_path(identity);
    let module = quote! { exports::#ns::#package::#name };
    let target = &service.ident;
    let spelled = |spelling: &str| -> syn::Result<Type> { syn::parse_str(spelling) };
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
        let struct_ident = &implementation.ident;
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
        impl #module::Guest for __TembraExports {
            #(#owned)*
            #(#functions)*
        }
        #(#implementations)*
    })
}

/// Keep, in module order, the candidate types that a derived service's method, a resource's
/// function or a derived hook's input reaches; the others go back to the module as ordinary Rust.
/// A reference to a type the module does not declare is an error; a resource is a type of its own.
fn reachable(
    candidates: Vec<(Item, Shape)>,
    services: &[Service],
    resources: &[WitResource],
    hook_inputs: &[&Type],
    passthrough: &mut Vec<Item>,
) -> syn::Result<Vec<Shape>> {
    let declared: BTreeMap<String, usize> = candidates
        .iter()
        .enumerate()
        .map(|(index, (_, shape))| (shape.ident.to_string(), index))
        .collect();
    let mut pending: Vec<Ident> = Vec::new();
    let mention = |function: &Function, pending: &mut Vec<Ident>| {
        for (_, ty, _) in &function.params {
            wit::references(ty, pending);
        }
        if let Some((ty, _)) = &function.result {
            wit::references(ty, pending);
        }
    };
    for service in services
        .iter()
        .filter(|service| service.implements.is_none())
    {
        for function in &service.functions {
            mention(function, &mut pending);
        }
    }
    for resource in resources {
        for (_, ty, _) in resource
            .constructor
            .iter()
            .flat_map(|constructor| &constructor.params)
        {
            wit::references(ty, &mut pending);
        }
        for function in resource.methods.iter().chain(&resource.statics) {
            mention(function, &mut pending);
        }
    }
    for input in hook_inputs {
        wit::references(input, &mut pending);
    }
    let mut kept = BTreeSet::new();
    while let Some(ident) = pending.pop() {
        if resources.iter().any(|resource| resource.ident == ident) {
            continue;
        }
        let Some(&index) = declared.get(&ident.to_string()) else {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{ident}` is used by a service method or a hook but is not a struct or enum of this module"
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
    })
}

/// The code of the declared hooks: for each, the export the contract requires, which traps
/// because the contributions answer the hook, and `run` on the author's struct, which calls the
/// matching import so the kernel runs the plan. A derived hook's `run` speaks the module's own
/// types and converts them to and from the import side of the bindings.
fn declaration_code(
    declared: &[Declared],
    callbacks: &BTreeMap<String, wit::Callback>,
    shapes: &[Shape],
    hook_shapes: &BTreeSet<String>,
) -> syn::Result<proc_macro2::TokenStream> {
    if declared.is_empty() {
        return Ok(quote! {});
    }
    // The types the derived hooks reach are defined in the first derived hook's interface, on
    // both sides of the bindings.
    let defining = declared
        .iter()
        .find(|declaration| declaration.input.is_some())
        .map(|declaration| hook_path(&declaration.hook));
    let (import_types, export_types) = match &defining {
        Some([ns, package, interface]) => (
            quote! { self::#ns::#package::#interface },
            quote! { exports::#ns::#package::#interface },
        ),
        None => (quote! {}, quote! {}),
    };
    let conversions = defining
        .is_some()
        .then(|| conversions(shapes, hook_shapes, &export_types, &import_types));
    let items = declared
        .iter()
        .map(|declaration| {
            if let Some((input, _)) = &declaration.input {
                return Ok(derived_hook_code(
                    declaration,
                    input,
                    hook_shapes,
                    &import_types,
                ));
            }
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
    Ok(quote! { #(#items)* #conversions })
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
) -> proc_macro2::TokenStream {
    let [ns, package, interface] = hook_path(&declaration.hook);
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
    let call = quote! { self::#ns::#package::#interface::apply(#argument) };
    let body = match settle {
        Some(settle) => quote! { let answer = #call; #settle },
        None => quote! { #call; },
    };
    quote! {
        impl exports::#ns::#package::#interface::Guest for __TembraExports {
            fn apply(_input: #input) #result {
                ::core::panic!(#message)
            }
        }
        impl #ident {
            #[doc = #doc]
            pub fn run(input: #input) #result {
                let input: #import_input = #converted;
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
    })
}

/// The Rust side of the states: the kernel's `state-init` callback answering one declaration per
/// owning service, the accessors on each state type, and the JSON helpers they share.
fn state_code(states: &[State]) -> proc_macro2::TokenStream {
    if states.is_empty() {
        return quote! {};
    }
    let declarations = states.iter().map(|state| {
        let identity = LitStr::new(&state.identity, Span::call_site());
        let ident = &state.ident;
        let version = state.version;
        let validated = state.validator.is_some();
        quote! {
            #identity => Some(exports::tembra::guest::state_init::Declaration {
                initial: __tembra_state::to_json(&<#ident as ::core::default::Default>::default()),
                version: #version,
                validated: #validated,
            }),
        }
    });
    // On reload the kernel hands the new instance the value it inherits; a validated state
    // answers with its validator, any other state accepts.
    let validators = states
        .iter()
        .filter_map(|state| state.validator.as_ref().map(|validator| (state, validator)))
        .map(|(state, validator)| {
            let identity = LitStr::new(&state.identity, Span::call_site());
            let ident = &state.ident;
            let name = LitStr::new(&state.ident.to_string(), Span::call_site());
            quote! {
                #identity => match ::serde_json::from_str::<#ident>(&value) {
                    Ok(state) => state.#validator(),
                    Err(error) => Err(__tembra_state::format!(
                        "the retained state is not a `{}`: {error}", #name
                    )),
                },
            }
        });
    let accessors = states.iter().map(|state| {
        let ident = &state.ident;
        let fields = &state.fields;
        let service = state.service.as_ref().expect("resolved owner").to_string();
        let read_doc = format!(
            " The state of the `{service}` object running the current call, read from the kernel."
        );
        let update = if let Some(validator) = &state.validator {
            let doc = format!(
                " Change the state of the `{service}` object running the current call, validate it and store it in the kernel; returns the stored value, or the validator's refusal, in which case the kernel keeps the previous value."
            );
            quote! {
                #[doc = #doc]
                pub fn update(
                    change: impl FnOnce(&mut Self),
                ) -> Result<Self, __tembra_state::String> {
                    let mut value = Self::read();
                    change(&mut value);
                    value.#validator()?;
                    Ok(__tembra_state::update(&value))
                }
            }
        } else {
            let doc = format!(
                " Change the state of the `{service}` object running the current call and store it in the kernel; returns the stored value."
            );
            quote! {
                #[doc = #doc]
                pub fn update(change: impl FnOnce(&mut Self)) -> Self {
                    let mut value = Self::read();
                    change(&mut value);
                    __tembra_state::update(&value)
                }
            }
        };
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
                #update
            }
        }
    });
    quote! {
        // The kernel asks for each service object's state when it creates the object, and asks
        // a new instance to validate the state it inherits on reload.
        impl exports::tembra::guest::state_init::Guest for __TembraExports {
            fn declare(
                service: __tembra_state::String,
            ) -> Option<exports::tembra::guest::state_init::Declaration> {
                match service.as_str() {
                    #(#declarations)*
                    _ => None,
                }
            }
            fn validate(
                service: __tembra_state::String,
                value: __tembra_state::String,
            ) -> Result<(), __tembra_state::String> {
                match service.as_str() {
                    #(#validators)*
                    _ => Ok(()),
                }
            }
        }
        #(#accessors)*
        /// The kernel keeps a state as JSON; serde maps it to the state type and back.
        #[doc(hidden)]
        pub mod __tembra_state {
            extern crate alloc;
            pub use alloc::{format, string::String};

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
        open: None,
        close: None,
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
        let open = resource
            .open
            .as_ref()
            .expect("a resource has its #[open] function");
        let slot = slot(resource);
        quote! { #name => { #slot.open(#ident::#open()); 1 } }
    });
    let closers = resources.iter().map(|resource| {
        let name = wit::kebab(&resource.ident);
        let slot = slot(resource);
        match &resource.close {
            Some(close) => {
                quote! { #name => { if let Some(value) = #slot.close() { value.#close(); } } }
            }
            None => quote! { #name => { #slot.close(); } },
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
