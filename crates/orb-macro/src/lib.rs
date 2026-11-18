//! `#[orb(package = "ns:name@1.0.0")]` turns a Rust module into a compiled Orb.
//!
//! The module is the source of truth, and every exported thing is marked. A unit struct with
//! `#[service]` and an `impl` block of `pub fn` becomes an exported interface whose functions are
//! the Orb's methods. A free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`
//! participates in another Orb's hook. A struct with named fields and `#[state]` is the
//! kernel-managed state of a service's objects. A struct with `#[resource]` and a `pub fn open()`
//! is a resource of the Orb's root, opened by the kernel at activation and usable by every
//! method and hook callback. Structs with named fields and enums become `record`s, `enum`s and
//! `variant`s only when a service method, a WIT resource's function or a hook's input uses them,
//! directly or through another type; a state alone does not. Such a type stays the author's, with
//! its derives, attributes and impls as written: the macro converts it to and from the type the
//! bindings generate at the boundary, with `From` in both directions written in the type's module.
//! The rest of the module stays ordinary Rust, with the derives the author writes: a state's
//! `Default` and serde's derives, and those of the module types its fields reach, are the
//! author's.
//!
//! The macro derives the WIT contract from those items and generates what a WIT-first crate
//! writes by hand: the `generate!` invocation of the `wit-bindgen` the facade re-exports, with
//! the WIT inline, the `Guest` implementations forwarding to the author's functions and, for the
//! Wasm target, `export!`. The module uses the standard library by default; `no_std` on the
//! attribute adds the freestanding Wasm prelude.
//! The hook interfaces come from the provider's WIT, found in the
//! directory named by `wit = "dir"` on the attribute, relative to the crate manifest; their
//! generated modules are re-exported under their own names, so the author writes
//! `guard::Admission`. A derived signature names a consumed interface's types the same way, and
//! the derived interface `use`s them, so they cross as the consumed package's own, with no
//! conversion. The kernel's `tembra:guest` package is bundled with the macro; with
//! `context` on the attribute the module imports its `context` interface and reads the call's
//! principal and cancellation, the object's address and the host's log as `context::...`.
//! `import = "wasi:..."` imports an interface of the WASI 0.3 packages `tembra-contracts` bundles,
//! which the host serves under the installation's grants, with no directory to name.
//!
//! The crate using the macro depends on `tembra` with the `macros` feature, which brings the
//! `wit-bindgen` the generated bindings use, at the version whose rules the macro follows, and
//! on `dlmalloc` (feature `global`) only in `no_std` mode; a crate with a state
//! also depends on `serde` (`derive`, `alloc`) and `serde_json` (`alloc`). A crate that depends on
//! the facade under another name gives that name with `crate = ".."` on the attribute.
//! `cargo tembra build` compiles the crate, reads the same WIT and the hook priorities from the
//! component and packages the artifact.
mod cfg;
mod foreign;
mod keyed;
mod metadata;
mod native;
mod resource_params;
mod wasi;
mod wit;

use heck::{ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote, quote_spanned};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use syn::{
    Attribute, Fields, FnArg, Ident, Item, ItemFn, ItemMod, LitByteStr, LitInt, LitStr, Pat,
    ReturnType, Token, Type, Visibility, spanned::Spanned,
};
use tembra_contracts::{
    ComponentRoles, EmbeddedManifest, HookComposition, Lifetime, ServicePolicy,
};

/// Declare a compiled Orb from a module; the crate documentation lists the rules.
#[proc_macro_attribute]
pub fn orb(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expansion(attribute.into(), item.into()).into()
}

/// The module's expansion, or the macro's error next to the runtime prelude the module would have
/// brought: without its allocator and panic handler, a `no_std` crate would report their absence
/// after the error that matters. A std crate has its own runtime and gets none. A module whose
/// services, methods, root or hooks carry `#[cfg]`s expands into one copy per combination of
/// their predicates instead, which the macro expands again, see `cfg`.
fn expansion(
    attribute: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    // Read from the tokens, so that an attribute the macro refuses still says it.
    let freestanding = attribute
        .clone()
        .into_iter()
        .any(|token| matches!(&token, proc_macro2::TokenTree::Ident(ident) if ident == "no_std"));
    let crate_dir = primary_crate_dir(|name| std::env::var_os(name));
    // The copies go through the macro again by the name the crate depends on the facade under.
    let expanded = options(attribute.clone()).and_then(|options| {
        match cfg::combinations(&attribute, &item, &options.facade())? {
            Some(copies) => Ok(copies),
            None => expand(
                options,
                syn::parse2::<ItemMod>(item)?,
                metadata::Metadata::read(),
                crate_dir.as_deref(),
            ),
        }
    });
    match expanded {
        Ok(tokens) => tokens,
        Err(error) => {
            let error = error.to_compile_error();
            let prelude = freestanding.then(|| prelude(facade_of(&attribute).as_ref()));
            quote! { #error #prelude }
        }
    }
}

/// The directory the contract is written into, from Cargo's variables as `variable` reads them:
/// the crate's own, `CARGO_MANIFEST_DIR`, when Cargo compiles the crate's library for itself,
/// which sets `CARGO_PRIMARY_PACKAGE`; none for a dependency, whose contract is never written
/// into. Cargo compiles the crate's other targets for itself too, whose Orb modules are not the
/// library's and write nothing: it also sets `CARGO_TARGET_TMPDIR` for an integration test or a
/// bench, and `CARGO_BIN_NAME` for a binary or an example built as one. It sets neither for an
/// example built as a library, which this does not tell from the library.
fn primary_crate_dir(variable: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if variable("CARGO_TARGET_TMPDIR").is_some() || variable("CARGO_BIN_NAME").is_some() {
        return None;
    }
    variable("CARGO_PRIMARY_PACKAGE")
        .and(variable("CARGO_MANIFEST_DIR"))
        .map(PathBuf::from)
}

/// Read the attribute's options.
fn options(attribute: proc_macro2::TokenStream) -> syn::Result<Options> {
    let mut options = Options::default();
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("package") {
            options.package = Some(meta.value()?.parse::<LitStr>()?);
        } else if meta.path.is_ident("wit") {
            options.wit = Some(meta.value()?.parse::<LitStr>()?.value());
        } else if meta.path.is_ident("import") && meta.input.peek(syn::token::Paren) {
            options.keyed.push(keyed::option(&meta)?);
        } else if meta.path.is_ident("import") {
            options.imports.push(meta.value()?.parse::<LitStr>()?);
        } else if meta.path.is_ident("context") {
            options.context = true;
        } else if meta.path.is_ident("no_std") {
            options.no_std = true;
        } else if meta.path.is_ident("crate") {
            let name = meta.value()?.parse::<LitStr>()?;
            options.facade = Some(name.parse::<Ident>().map_err(|_| {
                syn::Error::new(
                    name.span(),
                    "`crate` names the facade dependency as the crate's code writes it, a Rust identifier: #[sdk::orb(crate = \"sdk\", ..)] for `sdk = { package = \"tembra\", .. }`",
                )
            })?);
        } else {
            return Err(meta.error(
                "unknown option; use #[orb(package = \"ns:name@1.0.0\", wit = \"dir\", import = \"ns:name/interface@version\", import(\"ns:name/interface@version\", keyed), context, no_std, crate = \"name\")]",
            ));
        }
        Ok(())
    });
    syn::parse::Parser::parse2(parser, attribute)?;
    Ok(options)
}

/// The runtime prelude of a `no_std` component: the allocator and a panic handler that traps, for
/// the Wasm target. On the host the crate builds with std, so `cargo check` and rust-analyzer
/// need no Wasm target. The panic handler goes through the facade, named `facade`, which drops it
/// when the facade links std, with `host` or `contracts`: std's would then be a second. The
/// invocation is for wasm32 only, so a native build never resolves the facade's path. Without a
/// facade, for an attribute whose `crate = ".."` the macro refused, the handler is written as it
/// is, since no path to the facade resolves: the build reports the refusal alone, unless a
/// feature that links std also reports the second handler.
fn prelude(facade: Option<&Ident>) -> proc_macro2::TokenStream {
    let handler = quote! {
        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __tembra_panic(_: &::core::panic::PanicInfo<'_>) -> ! {
            ::core::arch::wasm32::unreachable()
        }
    };
    let handler = match facade {
        Some(facade) => quote! {
            #[cfg(target_arch = "wasm32")]
            ::#facade::__freestanding! { #handler }
        },
        None => handler,
    };
    quote! {
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static __TEMBRA_ALLOCATOR: ::dlmalloc::GlobalDlmalloc = ::dlmalloc::GlobalDlmalloc;

        #handler
    }
}

/// The facade's name that the attribute's `crate = ".."` gives, read from the tokens, so that an
/// attribute the macro refuses still has it; `tembra` without one, and none for a `crate = ".."`
/// that names no Rust identifier, which the macro refuses.
fn facade_of(attribute: &proc_macro2::TokenStream) -> Option<Ident> {
    let tokens: Vec<_> = attribute.clone().into_iter().collect();
    let named = tokens.windows(3).find_map(|window| match window {
        [
            proc_macro2::TokenTree::Ident(key),
            proc_macro2::TokenTree::Punct(equals),
            proc_macro2::TokenTree::Literal(literal),
        ] if key == "crate" && equals.as_char() == '=' => Some(
            syn::parse_str::<LitStr>(&literal.to_string())
                .ok()
                .and_then(|name| name.parse::<Ident>().ok()),
        ),
        _ => None,
    });
    match named {
        Some(facade) => facade,
        None => Some(Ident::new("tembra", Span::call_site())),
    }
}

/// The attribute options.
#[derive(Default)]
struct Options {
    /// The WIT package identity of this Orb, as written on the attribute.
    package: Option<LitStr>,
    /// A directory with the WIT packages this Orb consumes, relative to the crate manifest.
    wit: Option<String>,
    /// The module imports the kernel's `tembra:guest/context` interface.
    context: bool,
    /// Supply the allocator and panic handler for a freestanding Wasm guest.
    no_std: bool,
    /// The name the crate depends on the facade under, as its code writes it, from `crate = ".."`
    /// on the attribute; `tembra` without it.
    facade: Option<Ident>,
    /// Interfaces of other Orbs the module imports, from the consumed WIT packages, as written on
    /// the attribute; the installer binds each to a provider.
    imports: Vec<LitStr>,
    /// Interfaces of other Orbs the module imports by key, as written on the attribute: the world
    /// imports a view of each, whose `get` selects an object of the provider by key.
    keyed: Vec<LitStr>,
}

impl Options {
    /// The name the generated code reaches the facade under, `tembra` without `crate = ".."`.
    fn facade(&self) -> Ident {
        self.facade
            .clone()
            .unwrap_or_else(|| Ident::new("tembra", Span::call_site()))
    }
}

/// A type declared in the module, with its WIT definition and the module types it refers to, each
/// with the module of the Orb the field's path names it in, by scope (`Candidate::references`).
struct Shape {
    ident: Ident,
    wit: String,
    references: Vec<(Ident, usize)>,
    /// How the type is built, for the conversions between the two sides of a derived hook, and
    /// between the author's type and the bindings' where the author's stays.
    kind: ShapeKind,
    /// The module declaring the type, by scope; 0 is the Orb module.
    scope: usize,
    /// The author's item, as written, for a type that stays the author's.
    item: Item,
}

/// A struct with named fields or an enum of the module, before reachability decides whether it
/// enters the WIT: the item, its name, the module types its fields name, followed by reachability
/// and serde's derives, and its module. Only a candidate that enters the WIT is translated, so a
/// type no method mentions may hold what WIT has no spelling for.
struct Candidate {
    item: Item,
    ident: Ident,
    /// The module types the fields name, read once the scan knows the consumed interfaces, whose
    /// modules name none, each with the module of the Orb the field's path names it in, by scope:
    /// the one a path leads into, `super::model::Entry`, or this one for a bare name
    /// (`foreign::Modules::item_references`).
    references: Vec<(Ident, usize)>,
    /// The module declaring the type, by scope; 0 is the Orb module.
    scope: usize,
}

impl Candidate {
    /// A struct with named fields, a record once it enters the WIT.
    fn record(item: syn::ItemStruct, scope: usize) -> Self {
        Self {
            ident: item.ident.clone(),
            item: Item::Struct(item),
            references: Vec::new(),
            scope,
        }
    }

    /// An enum, an `enum` or a `variant` once it enters the WIT; until then its variants may
    /// carry fields of any shape.
    fn enumeration(item: syn::ItemEnum, scope: usize) -> Self {
        Self {
            ident: item.ident.clone(),
            item: Item::Enum(item),
            references: Vec::new(),
            scope,
        }
    }

    /// The type's WIT definition, once it enters the WIT, with the newtypes `modules` reads spelled
    /// as their fields. What the macro cannot evaluate in it is refused once every shape is known
    /// (`unconditional`).
    fn translate(&self, modules: &foreign::Modules) -> syn::Result<Shape> {
        let spelled = |ty: &Type| modules.spelled(ty, self.scope);
        let mut shape = match &self.item {
            Item::Struct(item) => wit::record(item, &spelled)?,
            Item::Enum(item) => wit::enumeration(item, &spelled)?,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "a type entering the WIT is a struct with named fields or an enum",
                ));
            }
        };
        shape.scope = self.scope;
        shape.references = self.references.clone();
        Ok(shape)
    }
}

/// The construction of a shape: the named fields of a record, the cases of an enum, the cases of
/// a variant with their payload.
enum ShapeKind {
    Record(Vec<(Ident, Type)>),
    Enum(Vec<Ident>),
    Variant(Vec<(Ident, Payload)>),
}

/// What a case of a variant carries: nothing, one unnamed payload, or named fields, which enter
/// the WIT as a record of their own, `<enum>-<case>`, the case's payload there.
enum Payload {
    None,
    Unnamed(Type),
    Named(Vec<(Ident, Type)>),
}

impl Payload {
    /// The types the payload holds.
    fn types(&self) -> Vec<&Type> {
        match self {
            Payload::None => Vec::new(),
            Payload::Unnamed(ty) => vec![ty],
            Payload::Named(fields) => fields.iter().map(|(_, ty)| ty).collect(),
        }
    }
}

/// An exported interface: a struct marked `#[service]` with an `impl` block of `pub fn`, and
/// the ownership policy of its objects when the attribute names one. A plain `#[service]`
/// derives the interface from the `impl`; `#[service("ns:name/interface@version")]` implements an
/// interface of a consumed WIT package. The struct is a unit struct, unless its function marked
/// `#[create]` builds it, once per object.
struct Service {
    ident: Ident,
    /// The struct's visibility, which its native client takes.
    vis: Visibility,
    docs: Vec<String>,
    functions: Vec<Function>,
    /// The `pub fn` marked `#[local]`: methods for native callers only, with the Rust types as
    /// written, which the WIT leaves out.
    locals: Vec<Function>,
    policy: Option<Policy>,
    implements: Option<String>,
    /// The module declaring the service, by scope; 0 is the Orb module.
    scope: usize,
    /// The struct has no fields.
    unit: bool,
    /// The function marked `#[create]`, `fn(&Inputs, &Declaration) -> tembra::Result<Self>`, once
    /// found: the native factory of the service's objects, whose methods may borrow `&self`.
    create: Option<Ident>,
    /// The address its objects are refused at, natively.
    guard: Guard,
}

/// The address rules of a service's objects, from `#[service(key = main)]` and
/// `#[service(principal = "name")]`, each with where it is written: the native factory refuses an
/// object at another address.
#[derive(Default)]
struct Guard {
    /// The object's key is `["main"]`.
    main: Option<Span>,
    /// The object's principal is this one.
    principal: Option<(String, Span)>,
}

impl Guard {
    /// The rules, which make the module native only.
    fn markers(&self) -> impl Iterator<Item = NativeOnly> + '_ {
        let main = self.main.map(|span| NativeOnly {
            name: "key = main".to_owned(),
            span,
        });
        let principal = self.principal.as_ref().map(|(principal, span)| NativeOnly {
            name: format!("principal = \"{principal}\""),
            span: *span,
        });
        main.into_iter().chain(principal)
    }
}

impl Service {
    /// The ownership policy of the service's objects as the component's roles name it, when the
    /// service chose a lifetime or a linger.
    fn role(&self) -> Option<ServicePolicy> {
        let policy = self.policy.as_ref()?;
        Some(ServicePolicy {
            lifetime: if policy.host {
                Lifetime::Host
            } else {
                Lifetime::Referenced
            },
            linger_ms: policy.linger_ms,
        })
    }

    /// The versioned identity of the service's interface.
    fn identity(&self, package: &str) -> String {
        self.implements
            .clone()
            .unwrap_or_else(|| identity(package, &wit::kebab(&self.ident)))
    }

    /// Whether a contract plans a service for the interface, which then has a signature: its
    /// methods, the implemented interface's, which its WIT gives, or the functions of a WIT
    /// resource of its own, among `resources`. A contract plans none for an empty interface, which
    /// a native-only service without methods or resources exports.
    fn planned(&self, resources: &[WitResource]) -> bool {
        self.implements.is_some()
            || !self.functions.is_empty()
            || resources
                .iter()
                .any(|resource| resource.service.as_ref() == Some(&self.ident))
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

impl WitResource {
    /// Whether the resource belongs to an implemented interface, whose WIT defines it: its
    /// functions take and return the consumed package's types, never the module's.
    fn implemented(&self, services: &[Service]) -> bool {
        services.iter().any(|service| {
            service.implements.is_some() && self.service.as_ref() == Some(&service.ident)
        })
    }
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

/// One exported function, with the WIT spelling of its parameter and result types: empty for a
/// `#[local]` method, which stays out of the WIT.
struct Function {
    ident: Ident,
    asynchronous: bool,
    /// The function borrows `&self`, a receiver the WIT leaves out.
    receiver: bool,
    docs: Vec<String>,
    params: Vec<(Ident, Type, String)>,
    result: Option<(Type, String)>,
    /// The service method returns `tembra::Result<T>`, its error channel: a failure is the call's
    /// error, natively, and `result` is `T`, nothing for `()`.
    fallible: bool,
    /// Where a service method takes the call's `&Context`, which a native host passes and the
    /// WIT leaves out: the number of `params` written before it.
    context: Option<usize>,
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
    /// The composition as the component's roles name it.
    fn role(self) -> HookComposition {
        match self {
            Self::Series => HookComposition::Series,
            Self::Waterfall => HookComposition::Waterfall,
            Self::Bail => HookComposition::Bail,
        }
    }
}

/// A kernel-managed state: a struct with named fields marked `#[state]`, owned by the objects
/// of one service.
struct State {
    ident: Ident,
    /// The owning service, as named by `service = ...` or resolved to the module's only one.
    service: Option<Ident>,
    version: u32,
    /// The function marked `#[validate]`, `fn(&self) -> Result<(), String>`: every update runs it
    /// before the kernel stores the value, and a reload runs it on the value the new instance
    /// inherits, once that value reads as the state's type.
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

/// The struct marked `#[inputs]`: the argument of the native `orb(..)`, the values a host lends
/// every service of the Orb. It stays the author's struct, out of the WIT.
struct Inputs {
    ident: Ident,
    /// The module declaring the struct, by scope; 0 is the Orb module.
    scope: usize,
}

/// A newtype of the module: a tuple struct of one field, without generic parameters or a marker,
/// `struct Id(String)`. It stays the author's struct, and the WIT spells it as its field's type
/// wherever a type that enters the WIT names it: a value crosses as its field, written from it and
/// read back natively through `TryFrom` of the field's type, which may refuse it, and in a
/// component through `From`, which refuses none.
#[derive(Clone)]
struct Newtype {
    ident: Ident,
    /// The field's type, as the newtype's module names it.
    inner: Type,
    /// The module declaring the struct, by scope; 0 is the Orb module.
    scope: usize,
}

/// The newtype a struct is, if it is one: a tuple struct of one field, without generic parameters.
fn newtype(item: &syn::ItemStruct, scope: usize) -> Option<Newtype> {
    let Fields::Unnamed(fields) = &item.fields else {
        return None;
    };
    let [field] = fields.unnamed.iter().collect::<Vec<_>>()[..] else {
        return None;
    };
    item.generics.params.is_empty().then(|| Newtype {
        ident: item.ident.clone(),
        inner: field.ty.clone(),
        scope,
    })
}

/// The free function marked `#[root]`, `fn(&Inputs, &Declaration) -> tembra::Result<T>`: the
/// Orb's create, which the native `orb(..)` hands `Orb::create`, and whose value every object of
/// the Orb shares. It stays the author's function, out of the WIT.
struct Root {
    ident: Ident,
    /// The module declaring the function, by scope; 0 is the Orb module.
    scope: usize,
    /// The types of its parameters, the inputs' and the declaration's, and its result, as written,
    /// which the function of its module that the native side calls it through repeats.
    params: Vec<Type>,
    output: ReturnType,
}

/// A marker that makes the module native only, where it is written: something only a native host
/// gives an Orb, which a component could not serve. A component build refuses the module at each.
struct NativeOnly {
    /// The marker as the guide names it: `#[inputs]`, `#[create]`, `#[root]`, `key = main`,
    /// `principal = "name"`, a method's `&Context` or `tembra::Result`, or `#[local]`.
    name: String,
    span: Span,
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
    /// `#[create]`: a service's native factory, `fn(&Inputs, &Declaration) -> tembra::Result<Self>`.
    Create,
    /// `#[local]`: a service method for native callers only, kept out of the WIT.
    Local,
}

impl Marker {
    const ALL: [Marker; 6] = [
        Marker::Validate,
        Marker::Open,
        Marker::Close,
        Marker::Constructor,
        Marker::Create,
        Marker::Local,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Open => "open",
            Self::Close => "close",
            Self::Constructor => "constructor",
            Self::Create => "create",
            Self::Local => "local",
        }
    }

    /// The shape of the function the marker goes on, for the errors.
    fn shape(self) -> &'static str {
        match self {
            Self::Validate => "`fn(&self) -> Result<(), String>` of a #[state] struct",
            Self::Open => "`fn() -> Self` of a #[resource] struct",
            Self::Close => "`fn(self)` of a #[resource] struct",
            Self::Constructor => "`fn(..) -> Self`, without self, of a #[wit_resource] struct",
            Self::Create => {
                "`fn(&Inputs, &Declaration) -> tembra::Result<Self>` of a #[service] struct"
            }
            Self::Local => "a `pub fn` of a #[service] struct",
        }
    }

    /// The kind of struct whose `impl` block the marker goes in, for the errors.
    fn place(self) -> &'static str {
        match self {
            Self::Validate => "#[state]",
            Self::Open | Self::Close => "#[resource]",
            Self::Constructor => "#[wit_resource]",
            Self::Create | Self::Local => "#[service]",
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
#[derive(Clone)]
struct Module {
    ident: Option<Ident>,
    /// The module's scope, as the scan numbers the modules: each before those it holds, which come
    /// before its next sibling. Two modules of one name, which `#[cfg(..)]`s select, are two.
    scope: usize,
    attrs: Vec<Attribute>,
    vis: Visibility,
    unsafety: Option<syn::token::Unsafe>,
    items: Vec<Item>,
    generated: Vec<proc_macro2::TokenStream>,
    children: Vec<Module>,
}

impl Module {
    /// The module of scope `scope`, which the scan registered below this one.
    fn at(&mut self, scope: usize) -> &mut Module {
        let mut module = self;
        while module.scope != scope {
            module = module
                .children
                .iter_mut()
                .rev()
                .find(|child| child.scope <= scope)
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
                let (outer, inner) = placed(&self.attrs);
                let vis = &self.vis;
                let unsafety = &self.unsafety;
                // The author's span, so that rustc's error on a module the generated code cannot
                // see shows where the author declares it.
                quote_spanned! { ident.span() =>
                    #(#outer)* #vis #unsafety mod #ident { #(#inner)* #body }
                }
            }
            None => body,
        }
    }
}

/// The attributes of a module, split by where they go: the outer ones before `mod`, the inner
/// ones, `#![..]` and `//!`, first inside its braces.
fn placed(attrs: &[Attribute]) -> (Vec<&Attribute>, Vec<&Attribute>) {
    attrs
        .iter()
        .partition(|attribute| matches!(attribute.style, syn::AttrStyle::Outer))
}

/// What the scan of the Orb module collects, across its sub-modules.
struct Scan {
    package: String,
    /// The path of each module from the Orb module, by scope; scope 0 is the Orb module itself.
    modules: Vec<Vec<Ident>>,
    /// Where each module of each module's path is visible, by scope: the depth of the module of
    /// the path it is visible in, and so in every module below that one. The macro names an item
    /// through every module of its path.
    seen_from: Vec<Vec<usize>>,
    candidates: Vec<Candidate>,
    services: Vec<Service>,
    contributions: Vec<Contribution>,
    declared: Vec<Declared>,
    states: Vec<State>,
    resources: Vec<Resource>,
    wit_resources: Vec<WitResource>,
    inputs: Option<Inputs>,
    root: Option<Root>,
    /// The markers that make the module native only, in the order the scan finds them.
    native_only: Vec<NativeOnly>,
    /// The tuple structs of one field, which the WIT spells as their field's type.
    newtypes: Vec<Newtype>,
    impls: Vec<(usize, syn::ItemImpl)>,
    marked: Vec<Marked>,
    /// What each module imports, by scope.
    uses: Vec<Uses>,
    /// Every type each module declares, with its scope: a struct of any shape, an enum, a type
    /// alias or a union, candidate for the WIT or not, but the aliases of a namesake
    /// (`aliases_namesake`), which stand for the type they name, as a `use` does. A name the
    /// module writes resolves to one of them before a resource or a record of another module, and
    /// they tell the type an `impl` names apart from a namesake a `use` brings in.
    own_types: Vec<(usize, Ident)>,
}

/// The names a module binds with `use` and `extern crate`, each with the path it stands for, and
/// the paths it imports by glob, `use path::*;`: what the macro follows to name a derive the
/// author writes in that module from another module, the bindings'. With the names of the items
/// the module declares, which the bindings' module does not reach, and its type aliases, by name,
/// which a contract type's name is followed through (`foreign::Modules`).
#[derive(Clone, Default)]
struct Uses {
    names: BTreeMap<String, Named>,
    globs: Vec<Named>,
    items: BTreeSet<String>,
    aliases: BTreeMap<String, syn::ItemType>,
}

/// A path as a list of names, from the crates when `global`, as `::serde::Serialize` is, and
/// otherwise from the module that writes it.
#[derive(Clone)]
struct Named {
    global: bool,
    segments: Vec<Ident>,
}

impl Uses {
    /// Record the names a `use` declaration binds.
    fn add_use(&mut self, item: &syn::ItemUse) {
        self.add_tree(&item.tree, item.leading_colon.is_some(), &mut Vec::new());
    }

    fn add_tree(&mut self, tree: &syn::UseTree, global: bool, prefix: &mut Vec<Ident>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.clone());
                self.add_tree(&path.tree, global, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(name) => self.bind(&name.ident, &name.ident, global, prefix),
            syn::UseTree::Rename(rename) => {
                self.bind(&rename.ident, &rename.rename, global, prefix);
            }
            syn::UseTree::Glob(_) => self.globs.push(Named {
                global,
                segments: prefix.clone(),
            }),
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    self.add_tree(tree, global, prefix);
                }
            }
        }
    }

    /// Bind `name` to the item `ident` after `prefix`: `self` stands for the prefix itself, under
    /// its own last name unless renamed, and `_` binds nothing.
    fn bind(&mut self, ident: &Ident, name: &Ident, global: bool, prefix: &[Ident]) {
        let mut segments = prefix.to_vec();
        let name = if ident == "self" {
            match (name == "self", prefix.last()) {
                (true, Some(last)) => last.clone(),
                (true, None) => return,
                (false, _) => name.clone(),
            }
        } else {
            segments.push(ident.clone());
            name.clone()
        };
        if name != "_" {
            self.names
                .insert(name.to_string(), Named { global, segments });
        }
    }

    /// Record the name an `extern crate` declaration binds: the crate, or this crate for `self`.
    fn add_extern_crate(&mut self, item: &syn::ItemExternCrate) {
        let name = item
            .rename
            .as_ref()
            .map_or(&item.ident, |(_, rename)| rename);
        if name == "_" {
            return;
        }
        let named = if item.ident == "self" {
            Named {
                global: false,
                segments: vec![Ident::new("crate", item.ident.span())],
            }
        } else {
            Named {
                global: true,
                segments: vec![item.ident.clone()],
            }
        };
        self.names.insert(name.to_string(), named);
    }

    /// Record the name of an item the module declares.
    fn add_item(&mut self, item: &Item) {
        let ident = match item {
            Item::Const(item) => &item.ident,
            Item::Enum(item) => &item.ident,
            Item::Fn(item) => &item.sig.ident,
            Item::Macro(syn::ItemMacro {
                ident: Some(ident), ..
            }) => ident,
            Item::Mod(item) => &item.ident,
            Item::Static(item) => &item.ident,
            Item::Struct(item) => &item.ident,
            Item::Trait(item) => &item.ident,
            Item::TraitAlias(item) => &item.ident,
            Item::Type(item) => {
                self.aliases.insert(item.ident.to_string(), item.clone());
                &item.ident
            }
            Item::Union(item) => &item.ident,
            _ => return,
        };
        self.items.insert(ident.to_string());
    }
}

/// How many modules above the one declaring it an item of this visibility is visible in: none
/// when private, one for `pub(super)`, as many as the `super`s of `pub(in ..)`, and all of them for
/// `pub`, `pub(crate)` and a path from `crate`, which the macro cannot place and leaves to the
/// compiler.
fn reach(vis: &Visibility) -> usize {
    match vis {
        Visibility::Public(_) => usize::MAX,
        Visibility::Restricted(restricted) => {
            let segments = &restricted.path.segments;
            if segments
                .first()
                .is_some_and(|segment| segment.ident == "crate")
            {
                usize::MAX
            } else {
                segments
                    .iter()
                    .filter(|segment| segment.ident == "super")
                    .count()
            }
        }
        Visibility::Inherited => 0,
    }
}

/// Why the native side cannot build an Orb whose WIT resource `resource`, with a constructor or a
/// static function, sits in the module at `from`, when that module cannot name the native client
/// of its service, `service`, declared in the module at `path`, where `seen_from` says each module
/// of the path is visible: the native side writes those functions on the client, in the resource's
/// module. The message names the module of the path and the service to make visible to it; the
/// Orb's `orb()` answers it, and a component build, which needs no client, is never refused for
/// it. Writing the calls in the service's module instead would name the parameter types from
/// there, and raising a visibility would change the author's code. A `pub(in crate::..)` module or
/// service counts as visible everywhere, as `reach` leaves it to the compiler, which reports one
/// the resource's module cannot see.
fn client_seen(
    service: &Service,
    path: &[Ident],
    seen_from: &[usize],
    from: &[Ident],
    resource: &Ident,
) -> Result<(), String> {
    let module = seen_from
        .iter()
        .position(|&depth| !from.starts_with(&path[..depth]))
        .map(|position| format!("the module `{}`", path[position]));
    let declared = path.len().saturating_sub(reach(&service.vis));
    let hidden =
        (!from.starts_with(&path[..declared])).then(|| format!("the service `{}`", service.ident));
    let hidden: Vec<String> = module.into_iter().chain(hidden).collect();
    if hidden.is_empty() {
        return Ok(());
    }
    let viewer = if from.is_empty() {
        "the Orb module".to_owned()
    } else {
        format!("the module {}", describe(from))
    };
    Err(format!(
        "`{resource}` has a constructor or a static function, which a native build writes on the client of its service `{}` in {viewer}, which cannot see it: make {} visible to {viewer}, at least `pub(crate)`",
        service.ident,
        hidden.join(" and ")
    ))
}

/// Whether a type alias names a namesake, `pub type Token = super::sessions::Token;`: a path
/// ending in the alias's own name, with generics on neither, which the alias stands for as
/// `use super::sessions::Token;` would.
fn aliases_namesake(alias: &syn::ItemType) -> bool {
    let Type::Path(path) = &*alias.ty else {
        return false;
    };
    alias.generics.params.is_empty()
        && path.qself.is_none()
        && path
            .path
            .segments
            .last()
            .is_some_and(|last| last.ident == alias.ident && last.arguments.is_none())
}

impl Scan {
    /// Read the items of one module: the marked ones are collected with their scope, the
    /// sub-modules are read in turn, and everything else is copied through.
    fn scan(&mut self, items: Vec<Item>, scope: usize) -> syn::Result<(Vec<Item>, Vec<Module>)> {
        let mut passthrough = Vec::new();
        let mut children = Vec::new();
        let depth = self.modules[scope].len();
        for item in items {
            self.uses[scope].add_item(&item);
            let declared_type = match &item {
                Item::Struct(item) => Some(&item.ident),
                Item::Enum(item) => Some(&item.ident),
                Item::Type(item) => Some(&item.ident),
                Item::Union(item) => Some(&item.ident),
                _ => None,
            };
            if let Some(ident) = declared_type
                && !matches!(&item, Item::Type(alias) if aliases_namesake(alias))
            {
                self.own_types.push((scope, ident.clone()));
            }
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
                    let mut seen_from = self.seen_from[scope].clone();
                    seen_from.push(depth.saturating_sub(reach(&module.vis)));
                    self.seen_from.push(seen_from);
                    self.uses.push(Uses::default());
                    let (items, grandchildren) = self.scan(inner, child)?;
                    // The code the macro writes for an item it marks names the item in every
                    // configuration, also where the module's own `#[cfg(..)]` drops it.
                    if let Some((what, ident, does)) = self.marked_from(child) {
                        unconditional_marked(
                            &module.attrs,
                            &format!("module `{}` holding the {what}", module.ident),
                            ident,
                            does,
                            self.evaluated_from(child),
                        )?;
                    }
                    children.push(Module {
                        ident: Some(module.ident),
                        scope: child,
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
                        // The macro evaluates a plain `#[cfg(..)]` on a hook itself (`cfg`).
                        unconditional_marked(&s.attrs, "hook", &s.ident, "runs it", true)?;
                        let mut declaration = declaration(&s, &attribute, &self.package)?;
                        declaration.scope = scope;
                        self.declared.push(declaration);
                        passthrough.push(Item::Struct(s));
                    } else if let Some(attribute) = take(&mut s.attrs, "wit_resource") {
                        unconditional_marked(
                            &s.attrs,
                            "resource",
                            &s.ident,
                            "calls its functions",
                            false,
                        )?;
                        let mut resource = wit_resource(&s, &attribute)?;
                        resource.scope = scope;
                        self.wit_resources.push(resource);
                        passthrough.push(Item::Struct(s));
                    } else if let Some(attribute) = take(&mut s.attrs, "resource") {
                        unconditional_marked(
                            &s.attrs,
                            "resource",
                            &s.ident,
                            "opens and closes it",
                            false,
                        )?;
                        let mut resource = resource(&s, &attribute)?;
                        resource.scope = scope;
                        self.resources.push(resource);
                        // A resource enters the WIT only when a service method uses it.
                        if matches!(s.fields, Fields::Named(_)) {
                            self.candidates.push(Candidate::record(s, scope));
                        } else {
                            passthrough.push(Item::Struct(s));
                        }
                    } else if let Some(attribute) = take(&mut s.attrs, "state") {
                        unconditional_marked(
                            &s.attrs,
                            "state",
                            &s.ident,
                            "builds and reads it",
                            false,
                        )?;
                        let mut state = state(&s, &attribute)?;
                        state.scope = scope;
                        self.states.push(state);
                        self.candidates.push(Candidate::record(s, scope));
                    } else if let Some(attribute) = take(&mut s.attrs, "inputs") {
                        self.inputs(&s, &attribute, scope)?;
                        passthrough.push(Item::Struct(s));
                    } else if let Some(attribute) = take(&mut s.attrs, "service") {
                        // The macro evaluates a plain `#[cfg(..)]` on a service itself (`cfg`).
                        unconditional_marked(
                            &s.attrs,
                            "service",
                            &s.ident,
                            "calls its functions",
                            true,
                        )?;
                        let (implements, policy, guard) = service_attribute(&attribute)?;
                        self.native_only.extend(guard.markers());
                        self.services.push(Service {
                            ident: s.ident.clone(),
                            vis: s.vis.clone(),
                            docs: wit::docs(&s.attrs)?,
                            functions: Vec::new(),
                            locals: Vec::new(),
                            policy,
                            implements,
                            scope,
                            unit: matches!(s.fields, Fields::Unit),
                            create: None,
                            guard,
                        });
                        passthrough.push(Item::Struct(s));
                    } else if matches!(s.fields, Fields::Named(_)) {
                        self.candidates.push(Candidate::record(s, scope));
                    } else {
                        // A newtype stays the author's, whether a type that enters the WIT names
                        // it or not.
                        self.newtypes.extend(newtype(&s, scope));
                        passthrough.push(Item::Struct(s));
                    }
                }
                Item::Enum(e) => self.candidates.push(Candidate::enumeration(e, scope)),
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
                        no_root(&function.attrs)?;
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
                        // The code the macro writes calls the marked function; a plain `#[cfg(..)]`
                        // on a service's create or local method, or on the `impl` block holding
                        // it, the macro evaluates itself (`cfg`).
                        let names = format!("{self_ty}::{}", function.sig.ident);
                        let evaluated = matches!(marker, Marker::Create | Marker::Local);
                        unconditional_marked(
                            &i.attrs,
                            "`impl` block holding",
                            &names,
                            "calls it",
                            evaluated,
                        )?;
                        unconditional_marked(
                            &function.attrs,
                            "function",
                            &names,
                            "calls it",
                            evaluated,
                        )?;
                        unconditional_params(&function.sig, "function", &names)?;
                        if marker == Marker::Create {
                            self.native_only.push(NativeOnly {
                                name: "#[create]".to_owned(),
                                span,
                            });
                        }
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
                        no_root(&function.attrs)?;
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
                    if let Some(attribute) = take(&mut function.attrs, "root") {
                        self.root(&function, &attribute, scope)?;
                    }
                    if let Some(attribute) = take(&mut function.attrs, "hook") {
                        // The macro evaluates a plain `#[cfg(..)]` on a contribution itself
                        // (`cfg`).
                        unconditional_marked(
                            &function.attrs,
                            "contribution",
                            &function.sig.ident,
                            "calls it",
                            true,
                        )?;
                        unconditional_params(&function.sig, "contribution", &function.sig.ident)?;
                        let mut contribution = contribution(&function, &attribute)?;
                        contribution.scope = scope;
                        self.contributions.push(contribution);
                    }
                    passthrough.push(Item::Fn(function));
                }
                Item::Use(used) => {
                    self.uses[scope].add_use(&used);
                    passthrough.push(Item::Use(used));
                }
                Item::ExternCrate(declared) => {
                    self.uses[scope].add_extern_crate(&declared);
                    passthrough.push(Item::ExternCrate(declared));
                }
                other => passthrough.push(other),
            }
        }
        Ok((passthrough, children))
    }

    /// Read the struct marked `#[inputs]`, the module's one, which the Orb module's `orb(..)`
    /// takes as it is.
    fn inputs(
        &mut self,
        item: &syn::ItemStruct,
        attribute: &Attribute,
        scope: usize,
    ) -> syn::Result<()> {
        let ident = &item.ident;
        if !matches!(attribute.meta, syn::Meta::Path(_)) {
            return Err(syn::Error::new(
                attribute.span(),
                "#[inputs] takes no options",
            ));
        }
        if let Some(first) = &self.inputs {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{}` and `{ident}` are both marked #[inputs]; an Orb has one, the argument of `orb(..)`",
                    first.ident
                ),
            ));
        }
        if !item.generics.params.is_empty() {
            return Err(syn::Error::new(
                item.generics.span(),
                format!("`{ident}` is the argument of `orb(..)`: it takes no generic parameter"),
            ));
        }
        self.inputs = Some(Inputs {
            ident: ident.clone(),
            scope,
        });
        self.native_only.push(NativeOnly {
            name: "#[inputs]".to_owned(),
            span: attribute.span(),
        });
        Ok(())
    }

    /// Read the function marked `#[root]`, the module's one, which the Orb module's `orb(..)`
    /// calls with the Orb's inputs and its declaration, both borrowed.
    fn root(&mut self, function: &ItemFn, attribute: &Attribute, scope: usize) -> syn::Result<()> {
        let signature = &function.sig;
        let ident = &signature.ident;
        if !matches!(attribute.meta, syn::Meta::Path(_)) {
            return Err(syn::Error::new(
                attribute.span(),
                "#[root] takes no options",
            ));
        }
        if let Some(first) = &self.root {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{}` and `{ident}` are both marked #[root]; an Orb has one, its create",
                    first.ident
                ),
            ));
        }
        let borrowed = |input: &FnArg| {
            matches!(input, FnArg::Typed(typed)
                if matches!(&*typed.ty, Type::Reference(reference) if reference.mutability.is_none()))
        };
        if signature.asyncness.is_some()
            || signature.inputs.len() != 2
            || !signature.inputs.iter().all(borrowed)
            || matches!(signature.output, ReturnType::Default)
        {
            return Err(syn::Error::new(
                attribute.span(),
                "#[root] marks `fn(&Inputs, &Declaration) -> tembra::Result<T>`, a free function of the module",
            ));
        }
        self.root = Some(Root {
            ident: ident.clone(),
            scope,
            params: signature
                .inputs
                .iter()
                .filter_map(|input| match input {
                    FnArg::Typed(typed) => Some((*typed.ty).clone()),
                    FnArg::Receiver(_) => None,
                })
                .collect(),
            output: signature.output.clone(),
        });
        self.native_only.push(NativeOnly {
            name: "#[root]".to_owned(),
            span: attribute.span(),
        });
        Ok(())
    }
    /// Whether the macro evaluates a plain `#[cfg(..)]` on the module `scope` itself (`cfg`): it
    /// does on a sub-module holding a service, a hook or the root, in the module or below it, and
    /// leaves one on a sub-module holding none of these to the compiler.
    fn evaluated_from(&self, scope: usize) -> bool {
        let below = |of: usize| of >= scope;
        self.services.iter().any(|item| below(item.scope))
            || self.declared.iter().any(|item| below(item.scope))
            || self.contributions.iter().any(|item| below(item.scope))
            || self.root.as_ref().is_some_and(|root| below(root.scope))
    }

    /// The first item the scan has marked in the module `scope` or below it, which are the modules
    /// scanned since that one, with what it is and what the code the macro writes for it does, for
    /// the errors: a struct marked `#[service]`, `#[state]`, `#[resource]`, `#[wit_resource]` or
    /// `#[hook(..)]`, or a contribution. The functions the code calls by role are those of a
    /// marked struct's `impl` blocks, in the struct's module.
    fn marked_from(&self, scope: usize) -> Option<(&'static str, &Ident, &'static str)> {
        let below = |of: usize| of >= scope;
        let services = self.services.iter().filter(|item| below(item.scope));
        let states = self.states.iter().filter(|item| below(item.scope));
        let resources = self.resources.iter().filter(|item| below(item.scope));
        let wit_resources = self.wit_resources.iter().filter(|item| below(item.scope));
        let declared = self.declared.iter().filter(|item| below(item.scope));
        let contributions = self.contributions.iter().filter(|item| below(item.scope));
        services
            .map(|item| ("service", &item.ident, "calls its functions"))
            .chain(states.map(|item| ("state", &item.ident, "builds and reads it")))
            .chain(resources.map(|item| ("resource", &item.ident, "opens and closes it")))
            .chain(wit_resources.map(|item| ("resource", &item.ident, "calls its functions")))
            .chain(declared.map(|item| ("hook", &item.ident, "runs it")))
            .chain(contributions.map(|item| ("contribution", &item.ident, "calls it")))
            .next()
    }
}

/// Refuse `#[root]` on a function of an impl block: it marks a free function.
fn no_root(attrs: &[Attribute]) -> syn::Result<()> {
    match attrs
        .iter()
        .find(|attribute| attribute.path().is_ident("root"))
    {
        Some(attribute) => Err(syn::Error::new(
            attribute.span(),
            "#[root] goes on a free function of the module, not in an impl block",
        )),
        None => Ok(()),
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

/// The module as the analysis leaves it for the emitters: the head of the Orb module and the
/// items that stay the author's, each marked item resolved to what it belongs to, what the
/// consumed packages define, the types that enter the WIT and the WIT itself. An emitter writes
/// its side of the output from this and nothing else.
struct Analysis {
    /// The attribute options, as read.
    options: Options,
    /// The package identity of the Orb, `ns:name@version`.
    package: String,
    /// The WIT name of the world: the module's, in kebab-case.
    world: String,
    /// The interfaces imported whole, `ns:name/interface@version`, as checked.
    imports: Vec<String>,
    /// The Orb module's attributes, visibility and name.
    attrs: Vec<Attribute>,
    vis: Visibility,
    ident: Ident,
    /// The path of each module from the Orb module, by scope; scope 0 is the Orb module itself.
    modules: Vec<Vec<Ident>>,
    /// Where each module of each module's path is visible, by scope, as the scan found it: the
    /// depth of the module of the path it is visible in, and so in every module below that one.
    seen_from: Vec<Vec<usize>>,
    /// The Orb module and its sub-modules with the author's items that stay theirs: the types
    /// that stay out of the WIT, with the serde derives a state needs of them, and the types the
    /// macro converts, `converted`, with the serde derives a state needs of them. Every other type
    /// that enters the WIT is generated by the bindings from `shapes`.
    tree: Module,
    /// The exported services, each with its methods, their parameters and results carrying the
    /// WIT spelling of their types, and the ownership policy of its objects.
    services: Vec<Service>,
    /// The WIT resources, each resolved to the service whose interface defines it.
    wit_resources: Vec<WitResource>,
    /// The resources of the Orb's root, with their `#[open]` and `#[close]` functions.
    resources: Vec<Resource>,
    /// The struct marked `#[inputs]`, if any: the argument of the native `orb(..)`.
    inputs: Option<Inputs>,
    /// The function marked `#[root]`, if any: the Orb's create, natively.
    root: Option<Root>,
    /// The markers that make the module native only, which a component build refuses: in the
    /// order the scan finds them, its `#[inputs]`, its creates and its root, which take the
    /// inputs, and the address guards; then, in the order written, what a service's method takes
    /// from a native host alone: the call's `&Context`, the runtime's `tembra::Result`, `#[local]`.
    native_only: Vec<NativeOnly>,
    /// The states, each resolved to the service whose objects own it.
    states: Vec<State>,
    /// The hook contributions, and the priority of each hook contributed to, by hook identity.
    contributions: Vec<Contribution>,
    hooks: BTreeMap<String, i32>,
    /// The hook declarations, as written; `declarations` keys them by hook.
    declared: Vec<Declared>,
    /// The callback of each consumed hook: a contributed one, or a declared one named from a
    /// consumed package.
    callbacks: BTreeMap<String, wit::Callback>,
    /// Each implemented or imported interface of a consumed package, as the bindings spell it.
    consumed: BTreeMap<String, wit::Consumed>,
    /// The modules the Orb module re-exports for the consumed interfaces.
    consumed_modules: foreign::Modules,
    /// The data types of the implemented interfaces, as their bindings generate them, which the
    /// native codecs read and write.
    consumed_types: wit::ConsumedTypes,
    /// The view of each interface imported by key.
    views: Vec<keyed::View>,
    /// The directory of the consumed WIT packages, absolute, when the attribute names one.
    wit_dir: Option<std::path::PathBuf>,
    /// The WIT packages the expansion parsed, each once: the consumed packages, the bundled WASI
    /// packages and the kernel's, which the bindings read again.
    parsed: wit::Parsed,
    /// The consumed records, variants and enums a state holds, directly or through the types of
    /// the module, and those they hold, by full name: the bindings derive serde's traits on these.
    serialized: BTreeSet<String>,
    /// The module types that enter the WIT, with their definitions.
    shapes: Vec<Shape>,
    /// The interface defining each type that enters the WIT, by the type's name.
    placement: BTreeMap<String, Ident>,
    /// The types that enter the WIT and stay the author's, in `tree`, converted to and from the
    /// bindings' types at the boundary: every enum with a case with named fields and every type
    /// holding one, free JSON or a newtype, by name, each with the bindings' name of its type.
    converted: BTreeMap<String, Ident>,
    /// The newtypes a type that enters the WIT names, which the WIT spells as their fields, each
    /// with the service whose interface names it, through which the bindings name what its field
    /// holds.
    newtypes: Vec<(Newtype, Ident)>,
    /// The WIT derived from the module.
    wit: String,
}

impl Analysis {
    /// The hook declarations by hook identity: one each, as the checks established.
    fn declarations(&self) -> BTreeMap<String, &Declared> {
        self.declared
            .iter()
            .map(|declaration| (declaration.hook.clone(), declaration))
            .collect()
    }
}

/// The module's expansion: one analysis of the module, then each side of the output written from
/// it into the author's module, after the WIT both sides share. The native side names the Orb
/// after the crate manifest's `[package.metadata.tembra]`, read by the caller, and alone reports
/// a manifest the caller could not read: a component build never needs it. With `crate_dir`,
/// the directory of the crate whose library is compiled for itself, its `contract/` is written
/// too, as `cargo tembra build` writes it; the token tests pass none.
fn expand(
    options: Options,
    module: ItemMod,
    metadata: syn::Result<metadata::Metadata>,
    crate_dir: Option<&Path>,
) -> syn::Result<proc_macro2::TokenStream> {
    let analysis = analyze(options, module)?;
    let native = native::emit(&analysis, metadata.as_ref());
    let component = component(&analysis, &native.nested, &native.served)?;
    check_profile(&analysis)?;
    check_roles(&analysis)?;
    let native = native.root;
    // Last, once both sides accepted the module: the signature of each service, from the WIT.
    let signatures = signatures(&analysis)?;
    // The contract follows the module: `contract/<world>.wit` with the consumed packages nested
    // and `contract/deps/` with the standard packages, the files `cargo tembra build` writes,
    // rewritten only when their content changes. A compile error names what went wrong.
    if let Some(crate_dir) = crate_dir {
        tembra_contracts::ModuleContract::new(&analysis.wit, analysis.wit_dir.as_deref())
            .and_then(|contract| contract.write(crate_dir))
            .map_err(|error| {
                syn::Error::new(
                    analysis.ident.span(),
                    format!(
                        "writing the contract into {}: {error}",
                        crate_dir.join("contract").display()
                    ),
                )
            })?;
    }
    let (outer_attrs, inner_attrs) = placed(&analysis.attrs);
    let vis = &analysis.vis;
    let ident = &analysis.ident;
    let wit_literal = LitStr::new(&analysis.wit, Span::call_site());
    Ok(quote! {
        #(#outer_attrs)*
        #vis mod #ident {
            #(#inner_attrs)*

            /// The WIT derived from this module.
            pub const WIT: &str = #wit_literal;

            #(#signatures)*

            #component
            #native
        }
    })
}

/// Read and check the module: scan its items, resolve each marked item to what it belongs to,
/// read the consumed packages, decide which types enter the WIT and render it. What a side of the
/// output alone can refuse, its emitter refuses.
fn analyze(options: Options, mut module: ItemMod) -> syn::Result<Analysis> {
    // The combination of `#[cfg]` predicates the module is the copy for, if it is one.
    let configuration = cfg::configuration(&mut module.attrs);
    let package = options
        .package
        .as_ref()
        .ok_or_else(|| syn::Error::new(module.span(), "missing package = \"ns:name@1.0.0\""))?;
    let package = wit_identity(package, "the package", false)?;
    let imports = options
        .imports
        .iter()
        .map(|import| wit_identity(import, "an imported interface", true))
        .collect::<syn::Result<Vec<String>>>()?;
    let world = wit::kebab(&module.ident);
    if let Some(problem) = wit::name_problem(&world) {
        return Err(syn::Error::new(
            module.ident.span(),
            format!(
                "the module `{}` has no WIT name for the world: its kebab-case form `{world}` {problem}",
                module.ident
            ),
        ));
    }
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
        seen_from: vec![Vec::new()],
        candidates: Vec::new(),
        services: Vec::new(),
        contributions: Vec::new(),
        declared: Vec::new(),
        states: Vec::new(),
        resources: Vec::new(),
        wit_resources: Vec::new(),
        inputs: None,
        root: None,
        native_only: Vec::new(),
        newtypes: Vec::new(),
        impls: Vec::new(),
        marked: Vec::new(),
        uses: vec![Uses::default()],
        own_types: Vec::new(),
    };
    let (items, children) = scan.scan(items, 0)?;
    let mut tree = Module {
        ident: None,
        scope: 0,
        attrs: Vec::new(),
        vis: Visibility::Inherited,
        unsafety: None,
        items,
        generated: Vec::new(),
        children,
    };
    let Scan {
        modules,
        seen_from,
        mut candidates,
        mut services,
        contributions,
        mut declared,
        mut states,
        mut resources,
        mut wit_resources,
        inputs,
        root,
        mut native_only,
        newtypes,
        impls,
        marked,
        uses,
        own_types,
        ..
    } = scan;
    // A method's error channel is spelled through the name the crate depends on the facade under.
    let facade = options.facade();
    // A service's methods are the pub fn of the impl blocks of its own module, but its create;
    // those marked #[local] are for native callers only, and stay out of the WIT.
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
            let marked_as = |marker: Marker| {
                marked.iter().find(|marked| {
                    marked.marker == marker
                        && marked.scope == *scope
                        && marked.self_ty == service.ident
                        && marked.function.sig.ident == function.sig.ident
                })
            };
            let create = marked_as(Marker::Create).is_some();
            let local = marked_as(Marker::Local);
            if matches!(function.vis, Visibility::Public(_)) && !create {
                // The macro evaluates a plain `#[cfg(..)]` on a service's `pub fn` and on the
                // `impl` block holding it itself (`cfg`).
                let names = format!("{}::{}", service.ident, function.sig.ident);
                unconditional_marked(
                    &item.attrs,
                    "`impl` block holding",
                    &names,
                    "calls it",
                    true,
                )?;
                unconditional_marked(&function.attrs, "function", &names, "calls it", true)?;
                unconditional_params(&function.sig, "function", &names)?;
                let read = method(function, local.is_some(), &facade)?;
                // In a module with a `Context` of its own, `&Context` names that type, which no
                // WIT parameter takes by reference, not the call's context.
                if read.context.is_some() && declares(&mut tree, &candidates, *scope, "Context") {
                    return Err(syn::Error::new(
                        read.ident.span(),
                        format!(
                            "`{}::{}` takes `&Context`, and this module declares a `Context` of its own: pass that type owned, as WIT parameters are, or rename it for the method to take the call's `&Context`",
                            service.ident, read.ident
                        ),
                    ));
                }
                // An implemented interface's names are fixed by its WIT, checked with it below; a
                // local method's are the service's own.
                if (service.implements.is_none() || local.is_some())
                    && let Some(member) = reserved_member(&wit::kebab(&read.ident))
                {
                    return Err(syn::Error::new(
                        read.ident.span(),
                        format!(
                            "`{}::{}` is a method named `{member}`, a member every reference to a service object has ({}): the kernel refuses such a method, so rename it",
                            service.ident,
                            read.ident,
                            tembra_contracts::RESERVED_MEMBERS.join(", ")
                        ),
                    ));
                }
                // What only a native host gives a method makes the module native only.
                if let Some(local) = local {
                    native_only.push(NativeOnly {
                        name: "#[local]".to_owned(),
                        span: local.span,
                    });
                }
                for input in &function.sig.inputs {
                    if let FnArg::Typed(typed) = input
                        && is_context(&typed.ty)
                    {
                        native_only.push(NativeOnly {
                            name: "&Context".to_owned(),
                            span: typed.ty.span(),
                        });
                    }
                }
                if let (true, ReturnType::Type(_, ty)) = (read.fallible, &function.sig.output) {
                    native_only.push(NativeOnly {
                        name: format!("{facade}::Result"),
                        span: ty.span(),
                    });
                }
                match local {
                    Some(_) => service.locals.push(read),
                    None => service.functions.push(read),
                }
            } else if let Some(local) = local {
                return Err(syn::Error::new(
                    local.span,
                    format!("#[local] marks {}", Marker::Local.shape()),
                ));
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
            Marker::Create | Marker::Local => services
                .iter()
                .any(|service| service.ident == marked.self_ty && service.scope == marked.scope),
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
            if let Some(asynchronous) = function.sig.asyncness {
                return Err(syn::Error::new(
                    asynchronous.span(),
                    "WIT constructors are synchronous; use an async static factory instead",
                ));
            }
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
        for (block, function) in public_functions(&impls, &resource.ident, resource.scope) {
            let names = format!("{}::{}", resource.ident, function.sig.ident);
            unconditional_marked(
                &block.attrs,
                "`impl` block holding",
                &names,
                "calls it",
                false,
            )?;
            unconditional_marked(&function.attrs, "function", &names, "calls it", false)?;
            unconditional_params(&function.sig, "function", &names)?;
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
    // A service's create builds each of its objects natively, from the Orb's inputs and the
    // object's declaration, both borrowed. Only a service with one has fields, or a method
    // borrowing `&self`.
    for service in &mut services {
        let Some(create) = marked_function(&marked, Marker::Create, &service.ident, service.scope)?
        else {
            if !service.unit {
                return Err(syn::Error::new(
                    service.ident.span(),
                    format!(
                        "`{}` has fields, which a #[create] builds: give it a function marked #[create], `fn(&Inputs, &Declaration) -> tembra::Result<Self>`, or make it a unit struct",
                        service.ident
                    ),
                ));
            }
            if let Some(function) = service
                .functions
                .iter()
                .chain(&service.locals)
                .find(|function| function.receiver)
            {
                return Err(syn::Error::new(
                    function.ident.span(),
                    format!(
                        "`{0}::{1}` borrows `&self`, which a #[create] builds: give `{0}` a function marked #[create], or remove self",
                        service.ident, function.ident
                    ),
                ));
            }
            continue;
        };
        let signature = &create.function.sig;
        let borrowed = |input: &FnArg| {
            matches!(input, FnArg::Typed(typed)
                if matches!(&*typed.ty, Type::Reference(reference) if reference.mutability.is_none()))
        };
        if signature.asyncness.is_some()
            || signature.inputs.len() != 2
            || !signature.inputs.iter().all(borrowed)
            || matches!(signature.output, ReturnType::Default)
        {
            return Err(syn::Error::new(
                create.span,
                format!("#[create] marks {}", Marker::Create.shape()),
            ));
        }
        if inputs.is_none() {
            return Err(syn::Error::new(
                create.span,
                format!(
                    "`{}::{}` is marked #[create], which takes the Orb's inputs: mark the struct `orb(..)` takes #[inputs]",
                    service.ident, signature.ident
                ),
            ));
        }
        service.create = Some(signature.ident.clone());
    }
    // The Orb's root is created from its inputs, as a service's object is.
    if let Some(root) = &root
        && inputs.is_none()
    {
        return Err(syn::Error::new(
            root.ident.span(),
            format!(
                "`{}` is marked #[root], which takes the Orb's inputs: mark the struct `orb(..)` takes #[inputs]",
                root.ident
            ),
        ));
    }
    // An Orb exports services, declares hooks or contributes to them; a module doing none of
    // those would be an empty world. A native-only one may instead run only its root, or hold
    // only root resources, while the host keeps it installed: its world is then empty.
    if services.is_empty() && contributions.is_empty() && declared.is_empty() {
        if native_only.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                cfg::under(
                    configuration.as_ref(),
                    "nothing to export: mark a unit struct with #[service] and give it an impl block of pub fn, or mark a hook with #[hook]",
                ),
            ));
        }
        if root.is_none() && resources.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                cfg::under(
                    configuration.as_ref(),
                    "nothing to export or run: mark a unit struct with #[service] and give it an impl block of pub fn, mark a hook with #[hook], or give the Orb a function marked #[root] or a #[resource]",
                ),
            ));
        }
    }
    // A service exports its methods, but its #[local] ones. In a native-only module, one that its
    // create builds may have none: its objects then only run what the create declares, such as
    // tasks, while they are referenced.
    if let Some(service) = services.iter().find(|service| {
        service.implements.is_none() && service.functions.is_empty() && service.create.is_none()
    }) {
        return Err(syn::Error::new(
            service.ident.span(),
            if !service.locals.is_empty() {
                "a service needs at least one portable method, a pub fn without #[local], or a function marked #[create] that declares what its objects run"
            } else if !native_only.is_empty() {
                "a service needs at least one pub fn in an impl block, or a function marked #[create] that declares what its objects run"
            } else {
                "a service needs at least one pub fn in an impl block"
            },
        ));
    }
    // A service object keeps one method of each name, and a local one would replace a portable one
    // of its name: local methods take their names among the portable ones and each other's.
    for service in &services {
        let mut claimed: BTreeMap<String, &Ident> = service
            .functions
            .iter()
            .map(|function| (wit::kebab(&function.ident), &function.ident))
            .collect();
        for local in &service.locals {
            let name = wit::kebab(&local.ident);
            if let Some(previous) = claimed.insert(name.clone(), &local.ident) {
                return Err(syn::Error::new(
                    local.ident.span(),
                    format!(
                        "`{0}::{previous}` and `{0}::{1}` are both `{name}` in the methods of `{0}`, where local and portable methods share one namespace; rename one of them",
                        service.ident, local.ident
                    ),
                ));
            }
        }
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
    // Each derived service and derived hook names an interface of the package after its struct:
    // a WIT name, which no other interface takes.
    let mut interface_names: BTreeMap<String, &Ident> = BTreeMap::new();
    let interface_idents = services
        .iter()
        .filter(|service| service.implements.is_none())
        .map(|service| &service.ident)
        .chain(
            declared
                .iter()
                .filter(|declaration| declaration.input.is_some())
                .map(|declaration| &declaration.ident),
        );
    for ident in interface_idents {
        let name = wit::kebab(ident);
        if let Some(problem) = wit::name_problem(&name) {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{ident}` has no WIT name for its interface: its kebab-case form `{name}` {problem}"
                ),
            ));
        }
        if let Some(previous) = interface_names.insert(name.clone(), ident) {
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{previous}` and `{ident}` are both the interface `{name}` in the WIT; rename one of them"
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
    // Each WIT resource belongs to the interface of one service, found the same way. The bindings
    // name it from that service's module, through every module of its path.
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
    // The modules the Orb module re-exports for the consumed interfaces name the consumed types and
    // handles, which are no types of the module.
    let keyed_imports = options
        .keyed
        .iter()
        .map(|import| wit_identity(import, "an interface imported by key", true))
        .collect::<syn::Result<Vec<String>>>()?;
    // A newtype is read through to its field, as the WIT spells it, by its name, as the types of
    // the module are: one whose name another type of the Orb takes stays ordinary Rust.
    let newtypes: Vec<Newtype> = newtypes
        .iter()
        .filter(|newtype| {
            let ident = &newtype.ident;
            newtypes
                .iter()
                .filter(|other| other.ident == *ident)
                .count()
                == 1
                && !candidates.iter().any(|candidate| candidate.ident == *ident)
                && !wit_resources
                    .iter()
                    .any(|resource| resource.ident == *ident)
                && !resources.iter().any(|resource| resource.ident == *ident)
        })
        .cloned()
        .collect();
    let consumed_modules = foreign::Modules::new(
        options.context,
        &options.imports,
        &keyed_imports,
        &services,
        &contributions,
        &declared,
        &modules,
        &uses,
    )?
    .with_newtypes(&newtypes);
    for candidate in &mut candidates {
        candidate.references = consumed_modules.item_references(&candidate.item, candidate.scope);
    }
    // Consumed packages, parsed once for the expansion: the callback of every hook, contributed or
    // declared, and every interface implemented or imported are read from their WIT packages.
    let parsed = wit::Parsed::new(options.wit.as_deref().map(wit::absolute).transpose()?);
    let wit_dir = parsed.dir().map(|dir| dir.path().to_path_buf());
    let implemented: Vec<String> = services
        .iter()
        .filter_map(|service| service.implements.clone())
        .collect();
    // An imported WASI interface comes from the bundled WASI packages, nested in the inline WIT.
    let imported: Vec<&String> = imports
        .iter()
        .filter(|identity| !wasi::is_wasi(identity))
        .collect();
    let consumed = match (parsed.dir(), implemented.is_empty() && imported.is_empty()) {
        (_, true) => BTreeMap::new(),
        (Some(dir), false) => {
            wit::consumed(dir, implemented.iter().chain(imported.iter().copied()))?
        }
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "implementing or importing an interface needs wit = \"<dir>\", the directory with its WIT package",
            ));
        }
    };
    // The callback of a contributed hook, or of a declared one named from a consumed package, is
    // read from that package; a hook derived from `input = Type` needs no directory.
    let consumed_hooks: Vec<String> = hooks
        .keys()
        .chain(
            declarations
                .iter()
                .filter(|(_, declaration)| declaration.input.is_none())
                .map(|(hook, _)| hook),
        )
        .cloned()
        .collect();
    let callbacks = match (parsed.dir(), consumed_hooks.is_empty()) {
        (_, true) => Default::default(),
        (Some(dir), false) => wit::callbacks(dir, consumed_hooks.iter())?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "hooks need wit = \"<dir>\", the directory with the hook's WIT package",
            ));
        }
    };
    let hook_inputs: Vec<(&Type, usize)> = declared
        .iter()
        .filter_map(|declaration| {
            let (ty, _) = declaration.input.as_ref()?;
            Some((ty, declaration.scope))
        })
        .collect();

    // Only the types a service method, a resource or a hook uses, directly or through another
    // type, enter the WIT; the others go back to their modules as ordinary Rust, with the derives
    // the author writes. A state alone makes no type enter: its `Default` and serde's traits are
    // the author's, which the macro asserts on the state's name (`state_code`).
    let mut unreached: Vec<Candidate> = Vec::new();
    let shapes = reachable(
        candidates,
        &services,
        &wit_resources,
        &own_types,
        &hook_inputs,
        &consumed_modules,
        &mut unreached,
    )?;
    // Every record, variant and enum stays the author's, converted to and from the bindings' type
    // at the boundary, its derives and attributes as written; what the macro cannot evaluate in
    // the type is refused first.
    let converted = converted(&shapes);
    for shape in &shapes {
        unconditional(&shape.item)?;
        wit_named(&shape.item)?;
    }
    // Where each type is defined, computed once for the WIT and for the re-exports below.
    let placement = wit::placement(
        &shapes,
        &services,
        &wit_resources,
        &declared,
        &consumed_modules,
    );
    // The newtypes the WIT spells as their fields, which cross as their fields, converted in both
    // targets: a field holding a resource has no conversion.
    let newtypes = named_newtypes(
        &newtypes,
        (&services, &wit_resources, &declared),
        &shapes,
        &placement,
        &consumed_modules,
    );
    for (newtype, _) in &newtypes {
        let mut held = Vec::new();
        consumed_modules.references(&newtype.inner, newtype.scope, &mut held);
        if let Some(resource) = held.iter().find_map(|name| {
            wit_resources
                .iter()
                .find(|resource| resource.ident == *name)
        }) {
            return Err(syn::Error::new(
                newtype.inner.span(),
                format!(
                    "the newtype `{}` holds the resource `{}`: the macro converts a newtype to and from the type of its field the bindings hold, and converts no resource; take or return the resource itself",
                    newtype.ident, resource.ident
                ),
            ));
        }
    }
    // The view of each interface imported by key, derived from the interface's WIT: its module
    // re-exports the types the view uses, which a derived signature names.
    let views = keyed::views(&options.keyed, parsed.dir(), &world, &interface_names)?;
    // What each derived interface `use`s from the consumed interfaces: the types its functions and
    // the types it defines name, and the handles its functions take, once each is checked where
    // it is named, a state's fields included.
    let foreign = foreign::foreign(foreign::Reach {
        modules: &consumed_modules,
        parsed: &parsed,
        services: &services,
        resources: &wit_resources,
        contributions: &contributions,
        declared: &declared,
        shapes: &shapes,
        placement: &placement,
        states: &states,
        unreached: &unreached,
    })?;
    for candidate in unreached {
        tree.at(candidate.scope).items.push(candidate.item);
    }
    // A type the macro converts stays the author's; the component writes its conversions.
    keep_converted(&shapes, &converted, &mut tree);
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
    // A derived function takes and returns only its own interface's resources, owned, itself or
    // through the records and variants it reaches, and a derived hook's input holds no resource. An
    // implemented interface's functions take what its WIT says, in the consumed package's types.
    resource_params::check_reached(
        &services,
        &wit_resources,
        &declared,
        &shapes,
        &consumed_modules,
    )?;
    // The data types of the implemented interfaces, of the imported ones and of the hooks the
    // module contributes to, for the native codecs: the world exports an implemented interface and
    // a contributed hook, so the bindings generate their types under `exports`, and imports the
    // others, whose types they generate at the root, an interface imported by key too, whose view
    // `use`s them.
    let consumed_types = match parsed.dir() {
        Some(dir)
            if !implemented.is_empty()
                || !imported.is_empty()
                || !views.is_empty()
                || !hooks.is_empty() =>
        {
            wit::ConsumedTypes::read(
                dir,
                implemented
                    .iter()
                    .map(|identity| (identity.as_str(), true))
                    .chain(hooks.keys().map(|hook| (hook.as_str(), true)))
                    .chain(imported.iter().map(|identity| (identity.as_str(), false)))
                    .chain(views.iter().map(|view| (view.target.as_str(), false))),
            )?
        }
        _ => wit::ConsumedTypes::default(),
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
            if let Some(member) = reserved_member(&function.name) {
                return Err(syn::Error::new(
                    service.ident.span(),
                    format!(
                        "{identity} has a function `{}`, a member every reference to a service object has as `{member}` ({}): the kernel refuses such a method, so no Orb can serve this interface",
                        function.name,
                        tembra_contracts::RESERVED_MEMBERS.join(", ")
                    ),
                ));
            }
            // A missing one is left to rustc, which reports the bridge's `impl` lacking it on
            // the service.
            if let Some(own) = implementing(&service.functions, &function.name)? {
                check_async(own, function)?;
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
            let ident = camel_ident(&resource.name);
            let Some(implementation) =
                implementation_of(&wit_resources, &service.ident, &resource.name)
            else {
                return Err(syn::Error::new(
                    service.ident.span(),
                    format!(
                        "{identity} defines the resource `{}`: mark a struct `{ident}` with #[wit_resource(service = {})] and give it the resource's functions",
                        resource.name, service.ident
                    ),
                ));
            };
            let ident = &implementation.ident;
            // A missing constructor or function is left to rustc, which reports the bridge's
            // `impl` lacking it on the struct.
            if let (
                None,
                Some(Constructor {
                    ident: constructor, ..
                }),
            ) = (&resource.constructor, &implementation.constructor)
            {
                return Err(syn::Error::new(
                    constructor.span(),
                    format!(
                        "`{ident}::{constructor}` is marked #[constructor], but the resource `{}` of {identity} has no constructor; the interface is fixed by its WIT",
                        resource.name
                    ),
                ));
            }
            let own = implementation.methods.iter().chain(&implementation.statics);
            for function in resource.methods.iter().chain(&resource.statics) {
                // The WIT fixes whether each one is asynchronous.
                if let Some(authored) = implementing(own.clone(), &function.name)? {
                    check_async(authored, function)?;
                }
            }
        }
    }
    // The functions and a derived hook's input were spelled by the names they write, before the
    // newtypes and the modules were known: each is spelled again, a newtype as its field's type
    // and a name a `use` or an alias stands for by the type's own.
    for declaration in &mut declared {
        if let Some((ty, spelling)) = &mut declaration.input {
            *spelling = consumed_modules.spelled(ty, declaration.scope)?;
        }
    }
    for service in &mut services {
        for function in &mut service.functions {
            spell_names(function, service.scope, &consumed_modules)?;
        }
    }
    for resource in &mut wit_resources {
        for function in resource.methods.iter_mut().chain(&mut resource.statics) {
            spell_names(function, resource.scope, &consumed_modules)?;
        }
        for (_, ty, spelling) in resource
            .constructor
            .iter_mut()
            .flat_map(|constructor| &mut constructor.params)
        {
            *spelling = consumed_modules.spelled(ty, resource.scope)?;
        }
    }
    let wit = wit::render(
        &package,
        &world,
        wit::Items {
            shapes: &shapes,
            placement: &placement,
            services: &services,
            resources: &wit_resources,
            declared: &declared,
            foreign: &foreign.uses,
            modules: &consumed_modules,
        },
        consumed_hooks.iter(),
        &imports,
        &views,
        wit::Kernel {
            state: !states.is_empty(),
            resources: !resources.is_empty(),
            context: options.context,
        },
    )?;
    Ok(Analysis {
        options,
        package,
        world,
        imports,
        attrs: module.attrs,
        vis: module.vis,
        ident: module.ident,
        modules,
        seen_from,
        tree,
        services,
        wit_resources,
        resources,
        inputs,
        root,
        native_only,
        states,
        contributions,
        hooks,
        declared,
        callbacks,
        consumed,
        consumed_modules,
        consumed_types,
        views,
        wit_dir,
        parsed,
        serialized: foreign.serialized,
        shapes,
        placement,
        converted,
        newtypes,
        wit,
    })
}

/// The component side of the module: the bindings from the inline WIT, the conversions between
/// the author's types and the bindings', the bridges to the author's functions, each in the module
/// of the item it serves, the sections `cargo tembra build` reads and the optional freestanding
/// runtime prelude. The sub-modules are rendered here, so the native side's items that go in one,
/// `nested` by scope, are placed in it on the way. The re-export of an import module the native
/// side writes in its place, one `served` names, is a component build's alone.
fn component(
    analysis: &Analysis,
    nested: &[(usize, proc_macro2::TokenStream)],
    served: &BTreeSet<String>,
) -> syn::Result<proc_macro2::TokenStream> {
    let Analysis {
        options,
        package,
        world,
        imports,
        modules,
        services,
        wit_resources,
        resources,
        states,
        contributions,
        hooks,
        declared,
        callbacks,
        consumed,
        consumed_modules,
        views,
        parsed,
        serialized,
        shapes,
        placement,
        converted,
        newtypes,
        wit,
        ..
    } = analysis;
    let declarations = analysis.declarations();
    // The name the crate depends on the facade under, which every path through it takes.
    let facade = options.facade();
    // The generated items go in the module of the item each serves: a copy of the tree takes
    // them, so the analysis stays as it is for the other side.
    let mut tree = analysis.tree.clone();
    let (namespace, rest) = package.split_once(':').unwrap_or_default();
    let name = rest.split_once('@').map_or(rest, |(name, _)| name);
    let ns_ident = rust_ident(namespace);
    let name_ident = package_module(name);
    // Every record, variant and enum is the author's own, which the bindings never replace: the
    // module re-exports no type of the bindings, and converts each at the boundary, below.
    let hook_shapes = wit::hook_shapes(shapes, declared, consumed_modules);
    let interface_module = |ident: &Ident| rust_ident(&wit::kebab(ident));
    // The bindings' handle type of each WIT resource, by the author's name of its struct.
    let handle_types: BTreeMap<String, Ident> = wit_resources
        .iter()
        .map(|resource| {
            let handle = type_ident(&wit::kebab(&resource.ident));
            (resource.ident.to_string(), handle)
        })
        .collect();
    // The hook whose interface defines the types the derived hooks' inputs reach.
    let defining = declared
        .iter()
        .find(|declaration| declaration.input.is_some())
        .map(|declaration| hook_path(&declaration.hook));
    // What a derived hook's `run` converts: the types the macro converts and the newtypes; and the
    // module types holding a string or a list, which the hook's import takes by reference.
    let converting = (converted, consumed_modules);
    let holding = holding_lists(shapes, consumed_modules);
    // A type the macro converts, the author's as the analysis left it, gets its conversions to
    // and from the bindings' type, next to it, and to and from the import's copy too where a
    // derived hook's input reaches it.
    converted_conversions(
        shapes,
        converted,
        (placement, &handle_types),
        (&ns_ident, &name_ident),
        &mut tree,
        (modules, consumed_modules),
        (&hook_shapes, defining.as_ref()),
    );
    // A newtype the WIT spells as its field crosses as that field, through the trait `NEWTYPE`,
    // implemented next to it, where the field is visible. With a derived hook, whose types the
    // bindings hold twice, it crosses to the hook's import side too, through `IMPORT_NEWTYPE`.
    for (newtype, interface) in newtypes {
        let interface = interface_module(interface);
        let depth = modules[newtype.scope].len();
        let root = supers(depth);
        let module = quote! { #root exports::#ns_ident::#name_ident::#interface };
        let export = Side::Export { converted };
        tree.at(newtype.scope).generated.push(newtype_conversions(
            newtype,
            &module,
            export,
            consumed_modules,
        ));
        if let Some(defining) = defining.as_ref() {
            let module = import_module(depth, defining);
            tree.at(newtype.scope).generated.push(newtype_conversions(
                newtype,
                &module,
                Side::Import {
                    hook_shapes: &hook_shapes,
                },
                consumed_modules,
            ));
        }
    }
    let newtype_trait = (!newtypes.is_empty()).then(newtype_trait);
    // A derived service's bridge forwards each function, named as the bindings name it, to the
    // author's; a resource a parameter holds is taken back from the bindings' handle type, one the
    // result holds is wrapped into it, and the resources of the service's interface are the
    // author's structs, each bridged in its own module. A service its `#[create]` builds has no
    // bridge: only the native side builds its objects, which its methods may borrow.
    for service in services
        .iter()
        .filter(|service| service.implements.is_none() && service.create.is_none())
    {
        let depth = modules[service.scope].len();
        let root = supers(depth);
        let interface = interface_module(&service.ident);
        let module = quote! { #root exports::#ns_ident::#name_ident::#interface };
        let target = &service.ident;
        let functions = service.functions.iter().map(|function| {
            let ident = &function.ident;
            let generated = rust_ident(&wit::kebab(ident));
            let (params, names) = resource_params::bridged(
                &function.params,
                service.scope,
                &module,
                &handle_types,
                converted,
                consumed_modules,
            );
            let (asynchronous, await_) = async_bridge(function.asynchronous);
            let call = answered(function, quote! { #target::#ident(#(#names),*) #await_ });
            let (result, body) = derived_result(
                &function.result,
                service.scope,
                &module,
                &handle_types,
                converted,
                consumed_modules,
                call,
            );
            let body = match function.context {
                Some(_) => native_only(
                    target,
                    ident,
                    function.params.iter().map(|(param, _, _)| param),
                ),
                None => body,
            };
            quote! { #asynchronous fn #generated(#(#params),*) #result { #body } }
        });
        let owned = wit_resources
            .iter()
            .filter(|resource| resource.service.as_ref() == Some(&service.ident))
            .map(|resource| {
                let camel = camel_ident(&wit::kebab(&resource.ident));
                let path = relative(depth, &modules[resource.scope], &resource.ident);
                quote! { type #camel = #path; }
            });
        tree.at(service.scope).generated.push(quote! {
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
            tree.at(resource.scope).generated.push(derived_resource(
                resource,
                &resource_module,
                &handle_types,
                converted,
                consumed_modules,
            ));
        }
    }
    // An implemented interface's bridge takes its signatures from the WIT: the functions forward
    // to the author's of the same WIT names, and each resource of the interface is the author's
    // struct of its WIT name. A service its `#[create]` builds has none, as above.
    for service in services
        .iter()
        .filter(|service| service.implements.is_some() && service.create.is_none())
    {
        let identity = service.implements.as_deref().unwrap_or_default();
        let (bridge, implementations) = implemented_service(
            service,
            identity,
            &consumed[identity],
            wit_resources,
            modules,
        )?;
        tree.at(service.scope).generated.push(bridge);
        for (scope, implementation) in implementations {
            tree.at(scope).generated.push(implementation);
        }
    }
    // Implemented and imported interfaces are re-exported under their own names, like hooks, in
    // the Orb module; a sub-module reaches them through `super::`. A native host gets the module
    // the native side writes in place of an import module it serves.
    let guest_only = |module: &Ident, reexport: proc_macro2::TokenStream| {
        if served.contains(&module.to_string()) {
            quote! { ::#facade::__guest! { #reexport } }
        } else {
            reexport
        }
    };
    let consumed_modules = services
        .iter()
        .filter_map(|service| service.implements.as_deref())
        .map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            quote! { pub use exports::#ns::#package::#interface; }
        })
        .chain(imports.iter().map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            guest_only(
                &interface,
                quote! { pub use self::#ns::#package::#interface; },
            )
        }))
        .chain(
            views
                .iter()
                .zip(keyed::reexports(package, views))
                .map(|(view, reexport)| guest_only(&rust_ident(&view.name), reexport)),
        );
    // The kernel's context interface is re-exported as `context`, so the author writes
    // `context::principal()`; only in a component build, as a native host gets the native side's
    // `context` instead.
    let context_module = options
        .context
        .then(|| quote! { ::#facade::__guest! { pub use self::tembra::guest::context; } });
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
    for contribution in contributions {
        let depth = modules[contribution.scope].len();
        let root = supers(depth);
        let module = export_module(depth, &hook_path(&contribution.hook));
        let callback = rust_ident(&callbacks[&contribution.hook].name);
        let target = &contribution.ident;
        let (param, ty) = &contribution.param;
        let result = contribution
            .result
            .as_ref()
            .map(|ty| quote! { -> #ty })
            .unwrap_or_default();
        tree.at(contribution.scope).generated.push(quote! {
            impl #module::Guest for #root __TembraExports {
                fn #callback(#param: #ty) #result { #target(#param) }
            }
        });
    }
    // The manifest travels with the component, like the WIT: the contributions with their
    // priorities, the declarations with their composition, the services that chose a
    // lifetime or a linger, the interfaces imported by key, and the directory of the consumed
    // packages, as written on the attribute: `cargo tembra build` nests the whole packages from
    // there, where the component's own types would keep only the functions it calls.
    let manifest = manifest(analysis);
    let manifest_section = if manifest == EmbeddedManifest::default() {
        quote! {}
    } else {
        let json = serde_json::to_vec(&manifest).expect("the manifest serializes");
        let bytes = LitByteStr::new(&json, Span::call_site());
        let len = proc_macro2::Literal::usize_unsuffixed(json.len());
        quote! {
            #[cfg(target_arch = "wasm32")]
            #[unsafe(link_section = "tembra:manifest")]
            #[used]
            static __TEMBRA_MANIFEST: [u8; #len] = *#bytes;
        }
    };
    // Signatures spelled from a consumed WIT name strings and lists from `alloc`, which a module
    // may not name: the alias goes in every module that holds such a signature, and so do the
    // bridges spelling free JSON as the bindings' string of its text, a derived hook's included.
    // The resources of an implemented interface are bridged in their own modules.
    let holds_json = |function: &Function| {
        function
            .params
            .iter()
            .map(|(_, ty, _)| ty)
            .chain(function.result.as_ref().map(|(ty, _)| ty))
            .any(wit::holds_free_json)
    };
    let implementers: Vec<&Ident> = services
        .iter()
        .filter(|service| service.implements.is_some())
        .map(|service| &service.ident)
        .collect();
    let alloc_scopes: BTreeSet<usize> = declared
        .iter()
        .map(|declaration| declaration.scope)
        .chain(
            services
                .iter()
                .filter(|service| service.implements.is_some())
                .map(|service| service.scope),
        )
        .chain(
            wit_resources
                .iter()
                .filter(|resource| {
                    resource
                        .service
                        .as_ref()
                        .is_some_and(|owner| implementers.contains(&owner))
                })
                .map(|resource| resource.scope),
        )
        .chain(
            services
                .iter()
                .filter(|service| service.functions.iter().any(holds_json))
                .map(|service| service.scope),
        )
        .chain(
            wit_resources
                .iter()
                .filter(|resource| {
                    resource
                        .methods
                        .iter()
                        .chain(&resource.statics)
                        .any(holds_json)
                        || resource.constructor.as_ref().is_some_and(|constructor| {
                            constructor
                                .params
                                .iter()
                                .any(|(_, ty, _)| wit::holds_free_json(ty))
                        })
                })
                .map(|resource| resource.scope),
        )
        .chain(
            newtypes
                .iter()
                .filter(|(newtype, _)| wit::holds_free_json(&newtype.inner))
                .map(|(newtype, _)| newtype.scope),
        )
        .collect();
    for scope in &alloc_scopes {
        tree.at(*scope)
            .generated
            .push(quote! { extern crate alloc as __tembra_alloc; });
    }
    // The hooks: the export the contract requires and `run`, in the declaring module; a derived
    // hook's types convert between the author's and each side of the bindings in their modules.
    for declaration in declared {
        let depth = modules[declaration.scope].len();
        let code = declaration_code(
            declaration,
            callbacks,
            &hook_shapes,
            &holding,
            defining.as_ref(),
            converting,
            depth,
        )?;
        tree.at(declaration.scope).generated.push(code);
    }
    let state_code = state_code(states, modules, &facade);
    let resource_code = resource_code(resources, modules, &facade);
    // The native side's items of each sub-module, after the component's, with the services they
    // register and the types whose JSON they write; a component build expands them to nothing.
    for (scope, items) in nested {
        tree.at(*scope).generated.push(items.clone());
    }
    let body = tree.tokens();
    let wit_bytes = LitByteStr::new(wit.as_bytes(), Span::call_site());
    let wit_len = proc_macro2::Literal::usize_unsuffixed(wit.len());
    let world_literal = LitStr::new(&wit::escaped(world), Span::call_site());
    // The bindings read the author's WIT and, nested in it, the kernel package a state, a
    // resource or the context uses and the WASI packages the imports need, unless the consumed
    // directory holds them already, as a contract with its dependencies does, every copy of a
    // bundled package there checked whether the Orb uses it or not; the hook packages come from
    // the consumed directory. Foreign interfaces need `generate_all`.
    let kernel = !states.is_empty() || !resources.is_empty() || options.context;
    let copies = wit::bundled_copies(parsed, kernel)?;
    let (standard, nested) = wasi::packages(parsed, &options.imports, &copies)?;
    let guest = kernel && wit::guest_package_needed(parsed, &copies)?;
    if let Some(literal) = &options.package {
        wit::claim_package(literal, parsed, &copies, guest, &nested)?;
    }
    let inline = if guest {
        LitStr::new(
            &format!("{wit}\n{}{standard}", wit::guest_package()?),
            Span::call_site(),
        )
    } else {
        LitStr::new(&format!("{wit}{standard}"), Span::call_site())
    };
    let path = parsed.dir().map(|dir| {
        let dir = LitStr::new(&dir.path().display().to_string(), Span::call_site());
        quote! { path: #dir, }
    });
    // A state's JSON holds the consumed types it reaches as the bindings generate them: those
    // types alone, and the types they hold, take serde's derives, each selected by its full name.
    let serialized = serialized
        .iter()
        .map(|name| LitStr::new(name, Span::call_site()));
    let derives = (!analysis.serialized.is_empty()).then(|| {
        quote! {
            additional_type_attributes: {
                #(#serialized: [#[derive(::serde::Serialize, ::serde::Deserialize)]]),*
            },
        }
    });
    // The generated code follows the derive rules and the flags lowering of one wit-bindgen
    // version, so the facade picks it, not the author's crate: the bindings come from its
    // re-export, and their runtime and the bitflags they name are reached there as well, under the
    // name the crate depends on the facade under.
    let runtime = LitStr::new(&format!("::{facade}::__wit_bindgen::rt"), Span::call_site());
    let bitflags = LitStr::new(
        &format!("::{facade}::__wit_bindgen::rt::bitflags"),
        Span::call_site(),
    );
    let generate = quote! {
        ::#facade::__wit_bindgen::generate!({
            #path
            inline: #inline,
            world: #world_literal,
            generate_all,
            runtime_path: #runtime,
            bitflags_path: #bitflags,
            #derives
        });
    };
    let runtime_prelude = options.no_std.then(|| prelude(Some(&options.facade())));
    // A component cannot serve a native-only module, so its build refuses the module at each
    // marker, every one at once; a host build compiles none of these errors.
    let refusals = analysis.native_only.iter().map(|NativeOnly { name, span }| {
        let message = LitStr::new(
            &format!(
                "`{name}` makes this module native only, which a component build refuses: leave it out of a component, or compile the module for native targets only, under `#[cfg(not(target_arch = \"wasm32\"))]`"
            ),
            *span,
        );
        quote_spanned! { *span =>
            #[cfg(target_arch = "wasm32")]
            ::core::compile_error!(#message);
        }
    });
    // A component holds one Orb: one `tembra:wit` section and one export of each of the kernel's
    // interfaces. An expansion cannot see the crate's other modules, nor its dependencies, so each
    // defines this symbol, which rustc refuses to define twice in one crate, on the second module,
    // and the linker across crates, in the words of its name. The name sorts before every other
    // symbol, the kernel's exports among them, so that rustc, which stops at the first symbol
    // defined twice, reports this one.
    let one_orb = quote_spanned! { analysis.ident.span() =>
        #[cfg(target_arch = "wasm32")]
        #[doc(hidden)]
        #[unsafe(export_name = #ONE_ORB)]
        static __TEMBRA_ONE_ORB: u8 = 0;
    };
    Ok(quote! {
        #(#refusals)*

        #one_orb

        #runtime_prelude

        // The exact WIT of this module travels inside the component as a custom section, so
        // `cargo tembra build` packages the contract as written here: hook imports the
        // linker drops and `///` comments the binary type section loses.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(link_section = "tembra:wit")]
        #[used]
        static __TEMBRA_WIT: [u8; #wit_len] = *#wit_bytes;

        #manifest_section

        #generate

        #context_module
        #(#consumed_modules)*
        #(#hook_modules)*

        #body

        // Only `export!` constructs it, in a component build; a native build keeps the bridges
        // and lets it be.
        #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
        struct __TembraExports;
        #newtype_trait
        #state_code
        #resource_code
        // The exports are symbols, which two Orb crates linked into one native binary would
        // both define. A host build keeps the bridges above, so `cargo check` still checks
        // them.
        #[cfg(target_arch = "wasm32")]
        export!(__TembraExports);
    })
}

/// Refuse a derived function whose signature the runtime does not carry, as tembra-contracts'
/// resource profile says it, which `cargo tembra build` checks of the component: the derived WIT,
/// resolved as a contract, checks each function of a derived interface and the callback of each
/// derived hook, and an error falls on the function or the hook. The profile holds the rules on
/// what a signature carries, such as an option of an option, and on the resources it moves: a
/// derived function takes owned resources of its own interface or of an interface the world
/// imports, and returns resources of its own interface only.
fn check_profile(analysis: &Analysis) -> syn::Result<()> {
    // Each derived function, by its interface's identity and its WIT name, with what the errors
    // call it and where: a derived hook's callback, `apply`, is the hook's.
    let functions: Vec<(String, String, String, Span)> =
        resource_params::derived(&analysis.services, &analysis.wit_resources)
            .into_iter()
            .map(|function| {
                (
                    identity(&analysis.package, &wit::kebab(function.interface)),
                    function.wit_name,
                    format!("`{}`", function.name),
                    function.span,
                )
            })
            .chain(
                analysis
                    .declared
                    .iter()
                    .filter(|declaration| declaration.input.is_some())
                    .map(|declaration| {
                        (
                            declaration.hook.clone(),
                            "apply".to_owned(),
                            format!("the callback of the hook `{}`", declaration.ident),
                            declaration.ident.span(),
                        )
                    }),
            )
            .collect();
    if functions.is_empty() {
        return Ok(());
    }
    let contract = resolved(analysis)?;
    for (identity, wit_name, name, span) in functions {
        let refused = |error: tembra_contracts::Error| {
            syn::Error::new(
                span,
                format!("{name} has a signature the runtime refuses: {error}"),
            )
        };
        let slot = contract.bind(&identity, &wit_name, true).map_err(refused)?;
        contract.resource_signature(slot).map_err(refused)?;
    }
    Ok(())
}

/// The manifest the component carries: the contributions with their priorities, the declarations
/// with their composition, the services that chose a lifetime or a linger, the interfaces imported
/// by key, and the directory of the consumed packages, as written on the attribute.
fn manifest(analysis: &Analysis) -> EmbeddedManifest {
    EmbeddedManifest {
        contributes: analysis.hooks.clone(),
        declares: analysis
            .declarations()
            .iter()
            .map(|(hook, declaration)| (hook.clone(), declaration.composition.role()))
            .collect(),
        services: analysis
            .services
            .iter()
            .filter_map(|service| Some((service.identity(&analysis.package), service.role()?)))
            .collect(),
        references: keyed::manifest(&analysis.package, &analysis.views),
        wit: analysis.options.wit.clone(),
    }
}

/// Refuse a hook declaration whose callback does not fit its composition, and a service policy the
/// kernel refuses, a host lifetime with a linger, as tembra-contracts' `check_roles` says, which
/// `cargo tembra build` runs on the roles it packages: each is checked alone, with the error on
/// the struct. A service without a function of the contract, which a native host alone serves,
/// has no role in a component: its linger is the kernel's `admits_linger`'s to check.
fn check_roles(analysis: &Analysis) -> syn::Result<()> {
    // A service with no role in a component, which a native host alone serves, still installs with
    // its policy, which the kernel checks by `admits_linger`.
    for service in &analysis.services {
        if let Some(policy) = service.role()
            && !service.planned(&analysis.wit_resources)
            && !tembra_contracts::admits_linger(policy.lifetime.into(), policy.linger_ms)
        {
            return Err(syn::Error::new(
                service.ident.span(),
                format!(
                    "{}: host-lifetime objects cannot linger",
                    service.identity(&analysis.package)
                ),
            ));
        }
    }
    let declarations = analysis.declarations();
    let services: Vec<&Service> = analysis
        .services
        .iter()
        .filter(|service| service.role().is_some() && service.planned(&analysis.wit_resources))
        .collect();
    if declarations.is_empty() && services.is_empty() {
        return Ok(());
    }
    let contract = resolved(analysis)?;
    let check = |roles: ComponentRoles, span: Span| {
        contract
            .check_roles(&roles)
            .map_err(|error| syn::Error::new(span, error.to_string()))
    };
    for (hook, declaration) in declarations {
        let roles = ComponentRoles {
            hooks: BTreeMap::from([(hook, declaration.composition.role())]),
            ..ComponentRoles::default()
        };
        check(roles, declaration.ident.span())?;
    }
    for service in services {
        let policy = service.role().expect("a policy");
        let roles = ComponentRoles {
            services: BTreeMap::from([(service.identity(&analysis.package), policy)]),
            ..ComponentRoles::default()
        };
        check(roles, service.ident.span())?;
    }
    Ok(())
}

/// The derived WIT, resolved as a contract with the consumed packages the way `cargo tembra build`
/// reads it.
fn resolved(analysis: &Analysis) -> syn::Result<tembra_contracts::Contract> {
    // The world as the WIT text names it, escaped when the module's name is a WIT keyword.
    let file = format!("{}.wit", analysis.world);
    let world = wit::escaped(&analysis.world);
    match &analysis.wit_dir {
        Some(dir) => {
            tembra_contracts::Contract::from_source_in(dir, &file, &analysis.wit, Some(&world))
        }
        None => tembra_contracts::Contract::from_source(&file, &analysis.wit, Some(&world)),
    }
    .map_err(|error| {
        syn::Error::new(
            Span::call_site(),
            format!("the derived WIT does not resolve as a contract: {error}"),
        )
    })
}

/// The signature of each service's interface, as the kernel computes it when it installs a
/// provider: the derived WIT, resolved as a contract with the consumed packages the way
/// `cargo tembra build` reads it, gives a service of the interface the digest `plan` gives it. A
/// native Orb of the module declares it on its `Service`. Each goes in the module, beside the
/// `WIT` both sides share, as a hidden constant named after the service, `__TEMBRA_SIGNATURE_LEDGER`
/// for `Ledger`.
fn signatures(analysis: &Analysis) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let Analysis {
        package,
        services,
        wit_resources,
        ..
    } = analysis;
    if services.is_empty() {
        return Ok(Vec::new());
    }
    let signatures = resolved(analysis)?.service_signatures().map_err(|error| {
        syn::Error::new(
            Span::call_site(),
            format!("the derived WIT does not resolve as a contract: {error}"),
        )
    })?;
    services
        .iter()
        .filter(|service| service.planned(wit_resources))
        .map(|service| {
            let identity = service.identity(package);
            let digest = signatures.get(&identity).ok_or_else(|| {
                syn::Error::new(
                    service.ident.span(),
                    format!("{identity} is no service of the derived contract"),
                )
            })?;
            let ident = signature_ident(service);
            Ok(quote! {
                #[doc(hidden)]
                pub const #ident: &str = #digest;
            })
        })
        .collect()
}

/// The constant holding a service's signature, `__TEMBRA_SIGNATURE_LEDGER` for `Ledger`: written
/// by `signatures` in the Orb module and read by the native side.
fn signature_ident(service: &Service) -> Ident {
    format_ident!(
        "__TEMBRA_SIGNATURE_{}",
        service.ident.to_string().to_shouty_snake_case()
    )
}

/// The symbol every Orb module defines in a build for wasm32, whose name is the error: rustc
/// reports `symbol `..` is already defined` on the second Orb module of one crate, and the linker
/// `duplicate symbol: ..` for an Orb crate whose dependency holds an Orb module too. It starts
/// with a capital letter, which sorts before the kernel's exports and mangled names.
const ONE_ORB: &str = "A component holds one Orb module, of this crate or of a dependency, and this build holds more than one: put each Orb module in a crate of its own, and what another crate uses of it in a crate without one";

/// The reserved member a WIT function would take on a reference, if any, as the kernel's
/// `reserved_member` says when it installs an Orb: it compares the lowerCamelCase spelling of the
/// official JavaScript bindings.
fn reserved_member(wit_name: &str) -> Option<String> {
    let member = wit_name.to_lower_camel_case();
    tembra_contracts::reserved_member(&member).then_some(member)
}

/// Remove and return the marker attribute with the given name, if present.
fn take(attrs: &mut Vec<Attribute>, name: &str) -> Option<Attribute> {
    let index = attrs.iter().position(|attr| attr.path().is_ident(name))?;
    Some(attrs.remove(index))
}

/// The versioned identity of an interface of this package: `ns:name/interface@version`.
fn identity(package: &str, interface: &str) -> String {
    Identity {
        interface: Some(interface),
        ..Identity::of(package)
    }
    .to_string()
}

/// Read a `#[service]` attribute: the identity of an interface the service implements, if it
/// names one first, then the ownership policy of its objects, if any: `lifetime = host` pins
/// the objects until the host closes, `linger_ms = n` keeps an unreferenced object for `n`
/// milliseconds; that a host-lifetime object cannot linger is `check_roles`'s. Then the guard of
/// their address, if any: `key = main` admits the key `["main"]` alone, `principal = "name"` that
/// principal alone, which is `host` for a host-lifetime object, as the kernel rules.
fn service_attribute(
    attribute: &Attribute,
) -> syn::Result<(Option<String>, Option<Policy>, Guard)> {
    if matches!(attribute.meta, syn::Meta::Path(_)) {
        return Ok((None, None, Guard::default()));
    }
    let span = attribute.span();
    attribute.parse_args_with(|input: syn::parse::ParseStream| {
        let mut implements = None;
        if input.peek(LitStr) {
            let identity: LitStr = input.parse()?;
            implements = Some(wit_identity(&identity, "an implemented interface", true)?);
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        let (mut host, mut linger_ms, mut chosen) = (false, 0, false);
        let mut guard = Guard::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if key == "key" {
                let value: Ident = input.parse()?;
                if value != "main" {
                    return Err(syn::Error::new(
                        value.span(),
                        "the key guard is `key = main`: an object's key is then [\"main\"]",
                    ));
                }
                guard.main = Some(key.span());
            } else if key == "principal" {
                guard.principal = Some((input.parse::<LitStr>()?.value(), key.span()));
            } else if key == "lifetime" {
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
                    "unknown option; use #[service(\"ns:name/interface@version\", lifetime = host, linger_ms = n, key = main, principal = \"name\")]",
                ));
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        // The host creates a host-lifetime object as `main` for itself when it installs the Orb.
        if host
            && let Some((principal, _)) = &guard.principal
            && !tembra_contracts::admits_principal(Lifetime::Host.into(), principal)
        {
            return Err(syn::Error::new(
                span,
                format!(
                    "host-lifetime objects belong to the principal host: the host creates them for itself when it installs the Orb, and `principal = \"{principal}\"` would refuse them"
                ),
            ));
        }
        Ok((implements, chosen.then_some(Policy { host, linger_ms }), guard))
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
        docs: wit::docs(&item.attrs)?,
        service,
        constructor: None,
        methods: Vec::new(),
        statics: Vec::new(),
        scope: 0,
    })
}

/// The `pub fn` of a type's `impl` blocks, in the module `scope`, each with the block holding it.
fn public_functions<'a>(
    impls: &'a [(usize, syn::ItemImpl)],
    ident: &Ident,
    scope: usize,
) -> Vec<(&'a syn::ItemImpl, &'a syn::ImplItemFn)> {
    impls
        .iter()
        .filter(|(module, item)| {
            *module == scope
                && matches!(&*item.self_ty, Type::Path(path) if path.path.is_ident(ident))
        })
        .flat_map(|(_, block)| block.items.iter().map(move |item| (block, item)))
        .filter_map(|(block, item)| match item {
            syn::ImplItem::Fn(function) if matches!(function.vis, Visibility::Public(_)) => {
                Some((block, function))
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
        asynchronous: function.sig.asyncness.is_some(),
        receiver: receiver.is_some(),
        docs: wit::docs(&function.attrs)?,
        params: typed_params(function)?,
        result: match result(&function.sig.output) {
            Some(ty) => Some((ty.clone(), wit::wit_type(ty)?)),
            None => None,
        },
        fallible: false,
        context: None,
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

/// Spell again each parameter and the result of a function written in the module `scope` as
/// `modules` resolves their names: a newtype as its field's type, a name a `use` or an alias
/// stands for by the type's own, `entry` for `Item` after `use model::Entry as Item;`.
fn spell_names(
    function: &mut Function,
    scope: usize,
    modules: &foreign::Modules,
) -> syn::Result<()> {
    let result = function
        .result
        .iter_mut()
        .map(|(ty, spelling)| (ty, spelling));
    let params = function
        .params
        .iter_mut()
        .map(|(_, ty, spelling)| (ty, spelling));
    for (ty, spelling) in params.chain(result) {
        *spelling = modules.spelled(ty, scope)?;
    }
    Ok(())
}

/// The call of an author's service method as a bridge answers it: the value of a method returning
/// `tembra::Result<T>`, which only a native host links. Such a module builds no component, so the
/// bridge, which a host build still type-checks, never runs.
fn answered(function: &Function, call: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    if !function.fallible {
        return call;
    }
    quote! {
        match #call {
            ::core::result::Result::Ok(value) => value,
            ::core::result::Result::Err(error) => ::core::panic!("{}", error),
        }
    }
}

/// The body of a component's bridge to a method that takes the call's `&Context`, which only a
/// native host passes: it never calls the author's function. The bridge still compiles on the
/// host with the others, so its parameters are read here.
fn native_only<'a>(
    service: &Ident,
    function: &Ident,
    params: impl IntoIterator<Item = &'a Ident>,
) -> proc_macro2::TokenStream {
    let message = format!(
        "`{service}::{function}` takes the call's `&Context`, which only a native host passes"
    );
    let params = params.into_iter();
    quote! { #(let _ = #params;)* ::core::unreachable!(#message) }
}

/// Preserve async execution in the bindings: poll the author's future instead of blocking it.
fn async_bridge(asynchronous: bool) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    if asynchronous {
        (quote! { async }, quote! { .await })
    } else {
        (quote! {}, quote! {})
    }
}

/// Existing WIT fixes both the result type and the synchronous or asynchronous calling convention.
fn check_async(function: &Function, expected: &wit::ConsumedFunction) -> syn::Result<()> {
    if function.asynchronous == expected.asynchronous {
        return Ok(());
    }
    let required = if expected.asynchronous {
        "async fn"
    } else {
        "fn"
    };
    Err(syn::Error::new(
        function.ident.span(),
        format!(
            "`{}` must be `{required}` to match its WIT declaration",
            function.ident
        ),
    ))
}

/// The result of a derived function's bridge, the function written in the module `scope`: each
/// resource the value holds, itself or inside options, results, lists and tuples, is wrapped into
/// the bindings' handle type of the interface's module, which `resources` names by the author's
/// name of the struct; a value holding none is returned as it is.
fn derived_result(
    result: &Option<(Type, String)>,
    scope: usize,
    module: &proc_macro2::TokenStream,
    resources: &BTreeMap<String, Ident>,
    converted: &BTreeMap<String, Ident>,
    modules: &foreign::Modules,
    call: proc_macro2::TokenStream,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let Some((ty, _)) = result else {
        return (quote! {}, call);
    };
    // A result holding a type the macro converts is spelled with the bindings' type in its
    // place, and each resource beside it with the handle type, and converted part by part, each
    // resource wrapped.
    if mentions(ty, scope, converted, modules) {
        let spelled = with_handles(
            ty,
            scope,
            &bindings_names(resources, converted),
            module,
            modules,
        );
        let at = At {
            modules,
            scope,
            resources,
            module,
            import: false,
        };
        return (
            quote! { -> #spelled },
            convert(ty, call, Towards::Bindings, Some(at)),
        );
    }
    match handles_of(ty, scope, resources, modules) {
        Some(handles) => {
            let spelled = with_handles(ty, scope, resources, module, modules);
            (quote! { -> #spelled }, wrap(&handles, call, module))
        }
        None => (quote! { -> #ty }, call),
    }
}

/// The bindings' name of each type of the module whose place a bridge spells with the bindings'
/// type: each resource's handle type, `resources`, and each type the macro converts, `converted`,
/// by the author's name.
pub(crate) fn bindings_names(
    resources: &BTreeMap<String, Ident>,
    converted: &BTreeMap<String, Ident>,
) -> BTreeMap<String, Ident> {
    resources
        .iter()
        .chain(converted)
        .map(|(name, generated)| (name.clone(), generated.clone()))
        .collect()
}

/// Where a value holds owned resources, which a bridge wraps into the bindings' handle types: a
/// resource itself, by the name of its handle type, or a container rebuilt around what it holds.
/// `None` stands for a part that holds no resource and crosses as it is.
pub(crate) enum Handles {
    Resource(Ident),
    Option(Box<Handles>),
    List(Box<Handles>),
    Result(Option<Box<Handles>>, Option<Box<Handles>>),
    Tuple(Vec<Option<Handles>>),
}

/// The containers, each holding what its parts hold, or `None` when no part holds a resource.
impl Handles {
    /// An option around `inner`.
    pub(crate) fn option(inner: Option<Handles>) -> Option<Handles> {
        Some(Handles::Option(Box::new(inner?)))
    }

    /// A list of `inner`.
    pub(crate) fn list(inner: Option<Handles>) -> Option<Handles> {
        Some(Handles::List(Box::new(inner?)))
    }

    /// A result with `ok` and `err` on its sides.
    pub(crate) fn result(ok: Option<Handles>, err: Option<Handles>) -> Option<Handles> {
        (ok.is_some() || err.is_some())
            .then(|| Handles::Result(ok.map(Box::new), err.map(Box::new)))
    }

    /// A tuple of `items`.
    pub(crate) fn tuple(items: Vec<Option<Handles>>) -> Option<Handles> {
        items
            .iter()
            .any(Option::is_some)
            .then_some(Handles::Tuple(items))
    }
}

/// Where an author's type, written in the module `scope`, holds a resource of the module, read by
/// the names `wit_type` reads: `Option`, `Vec`, `Result` and tuples around a type whose last
/// segment is the name of a resource's struct, which `resources` maps to the bindings' handle
/// type. A name a consumed interface's module reaches, `counter::Tally`, is that interface's,
/// whatever its last segment.
fn handles_of(
    ty: &Type,
    scope: usize,
    resources: &BTreeMap<String, Ident>,
    modules: &foreign::Modules,
) -> Option<Handles> {
    if modules.named(ty, scope).is_some() {
        return None;
    }
    match ty {
        Type::Path(path) => {
            let segment = path.path.segments.last()?;
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
            let name = segment.ident.to_string();
            match (name.as_str(), arguments.as_slice()) {
                (name, []) if resources.contains_key(name) => {
                    Some(Handles::Resource(resources[name].clone()))
                }
                ("Option", [inner]) => {
                    Handles::option(handles_of(inner, scope, resources, modules))
                }
                ("Vec", [inner]) => Handles::list(handles_of(inner, scope, resources, modules)),
                ("Result", [ok, err]) => Handles::result(
                    handles_of(ok, scope, resources, modules),
                    handles_of(err, scope, resources, modules),
                ),
                _ => None,
            }
        }
        Type::Tuple(tuple) => Handles::tuple(
            tuple
                .elems
                .iter()
                .map(|ty| handles_of(ty, scope, resources, modules))
                .collect(),
        ),
        _ => None,
    }
}

/// The author's type, written in the module `scope`, as the bindings spell it in `module`: each
/// resource of the module is the handle type there, and each type of `resources` the bindings'
/// type it names, however the author names it, through a `use` that renames it or an alias
/// included; free JSON is a string, a newtype the bindings' type of its field, everything else
/// keeps the author's spelling, a name a consumed interface's module reaches included.
fn with_handles(
    ty: &Type,
    scope: usize,
    resources: &BTreeMap<String, Ident>,
    module: &proc_macro2::TokenStream,
    modules: &foreign::Modules,
) -> proc_macro2::TokenStream {
    if modules.named(ty, scope).is_some() {
        return quote! { #ty };
    }
    // Free JSON is the bindings' string of its text, from the `alloc` of the scopes holding it.
    if wit::free_json(ty) {
        return quote! { __tembra_alloc::string::String };
    }
    // A newtype is the bindings' type of its field, which its trait names.
    if modules.newtype(ty, scope).is_some() {
        let newtype = modules.newtype_trait(scope);
        return quote! { <#ty as #newtype>::Bindings };
    }
    // A type of the module, by the name it resolves to, whatever the author's spelling of it.
    if let Some((named, _)) = modules.module_type(ty, scope)
        && let Some(handle) = resources.get(&named.to_string())
    {
        return quote! { #module::#handle };
    }
    match ty {
        Type::Path(path) => {
            let Some(last) = path.path.segments.last() else {
                return quote! { #ty };
            };
            let ident = &last.ident;
            match &last.arguments {
                syn::PathArguments::None if resources.contains_key(&ident.to_string()) => {
                    let handle = &resources[&ident.to_string()];
                    quote! { #module::#handle }
                }
                syn::PathArguments::AngleBracketed(arguments) => {
                    let leading = &path.path.leading_colon;
                    let prefix = path
                        .path
                        .segments
                        .iter()
                        .take(path.path.segments.len() - 1)
                        .map(|segment| quote! { #segment:: });
                    let arguments = arguments.args.iter().map(|argument| match argument {
                        syn::GenericArgument::Type(ty) => {
                            with_handles(ty, scope, resources, module, modules)
                        }
                        other => quote! { #other },
                    });
                    quote! { #leading #(#prefix)* #ident<#(#arguments),*> }
                }
                _ => quote! { #ty },
            }
        }
        Type::Tuple(tuple) => {
            let elements = tuple
                .elems
                .iter()
                .map(|ty| with_handles(ty, scope, resources, module, modules));
            quote! { (#(#elements,)*) }
        }
        _ => quote! { #ty },
    }
}

/// An expression turning `value` into the bindings' type: each resource `handles` places in it
/// is wrapped into its handle type of `module`, the interface's generated module, and the
/// options, lists, results and tuples around one are rebuilt.
fn wrap(
    handles: &Handles,
    value: proc_macro2::TokenStream,
    module: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    rebuild(handles, value, &|resource, value| {
        quote! { #module::#resource::new(#value) }
    })
}

/// An expression rebuilding `value` around the resources `handles` places in it: `leaf` turns
/// each resource, given the name of its handle type and the expression of its value, and the
/// options, lists, results and tuples around one are rebuilt.
fn rebuild(
    handles: &Handles,
    value: proc_macro2::TokenStream,
    leaf: &dyn Fn(&Ident, proc_macro2::TokenStream) -> proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let part = |handles: Option<&Handles>, value: proc_macro2::TokenStream| match handles {
        Some(handles) => rebuild(handles, value, leaf),
        None => value,
    };
    match handles {
        Handles::Resource(resource) => leaf(resource, value),
        Handles::Option(inner) => {
            let inner = rebuild(inner, quote! { item }, leaf);
            quote! { #value.map(|item| #inner) }
        }
        Handles::List(inner) => {
            let inner = rebuild(inner, quote! { item }, leaf);
            quote! { #value.into_iter().map(|item| #inner).collect() }
        }
        Handles::Result(ok, err) => {
            let ok = part(ok.as_deref(), quote! { item });
            let err = part(err.as_deref(), quote! { item });
            quote! {
                match #value {
                    ::core::result::Result::Ok(item) => ::core::result::Result::Ok(#ok),
                    ::core::result::Result::Err(item) => ::core::result::Result::Err(#err),
                }
            }
        }
        Handles::Tuple(items) => {
            let names: Vec<Ident> = (0..items.len())
                .map(|index| format_ident!("item{index}"))
                .collect();
            let parts = items
                .iter()
                .zip(&names)
                .map(|(handles, name)| part(handles.as_ref(), quote! { #name }));
            quote! { { let (#(#names,)*) = #value; (#(#parts,)*) } }
        }
    }
}

/// The bindings' trait of a derived resource, implemented by the author's struct: the constructor,
/// the methods and the static functions, named as the bindings name them, forward to the
/// author's `pub fn`.
fn derived_resource(
    resource: &WitResource,
    module: &proc_macro2::TokenStream,
    resources: &BTreeMap<String, Ident>,
    converted: &BTreeMap<String, Ident>,
    modules: &foreign::Modules,
) -> proc_macro2::TokenStream {
    let ident = &resource.ident;
    let scope = resource.scope;
    let guest = format_ident!("Guest{}", camel_ident(&wit::kebab(ident)));
    let bridged = |params: &[(Ident, Type, String)]| {
        resource_params::bridged(params, scope, module, resources, converted, modules)
    };
    let constructor = resource.constructor.as_ref().map(|constructor| {
        let (params, names) = bridged(&constructor.params);
        let function = &constructor.ident;
        quote! { fn new(#(#params),*) -> Self { #ident::#function(#(#names),*) } }
    });
    let methods = resource.methods.iter().map(|function| {
        let name = &function.ident;
        let generated = rust_ident(&wit::kebab(name));
        let (params, names) = bridged(&function.params);
        let (asynchronous, await_) = async_bridge(function.asynchronous);
        let call = quote! { #ident::#name(self, #(#names),*) #await_ };
        let (result, body) = derived_result(
            &function.result,
            scope,
            module,
            resources,
            converted,
            modules,
            call,
        );
        quote! { #asynchronous fn #generated(&self, #(#params),*) #result { #body } }
    });
    let statics = resource.statics.iter().map(|function| {
        let name = &function.ident;
        let generated = rust_ident(&wit::kebab(name));
        let (params, names) = bridged(&function.params);
        let (asynchronous, await_) = async_bridge(function.asynchronous);
        let call = quote! { #ident::#name(#(#names),*) #await_ };
        let (result, body) = derived_result(
            &function.result,
            scope,
            module,
            resources,
            converted,
            modules,
            call,
        );
        quote! { #asynchronous fn #generated(#(#params),*) #result { #body } }
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
/// declarations, the result, and where a returned value holds resources to wrap, if anywhere.
struct Signature<'a> {
    names: Vec<Ident>,
    params: Vec<proc_macro2::TokenStream>,
    result: proc_macro2::TokenStream,
    handles: Option<&'a Handles>,
}

/// The parameters and the result of a function of a consumed interface, spelled for a bridge
/// `depth` modules below the Orb module.
fn signature<'a>(
    params: &[(String, wit::Spelled)],
    result: Option<&'a wit::Spelled>,
    depth: usize,
) -> syn::Result<Signature<'a>> {
    let spelled =
        |spelling: &str| -> syn::Result<Type> { syn::parse_str(&rooted(spelling, depth)) };
    let mut names = Vec::new();
    let mut declarations = Vec::new();
    for (param, spelling) in params {
        let ident = rust_ident(param);
        let ty = spelled(&spelling.export)?;
        declarations.push(quote! { #ident: #ty });
        names.push(ident);
    }
    let spelled_result = match result {
        Some(spelling) => {
            let ty = spelled(&spelling.export)?;
            quote! { -> #ty }
        }
        None => quote! {},
    };
    Ok(Signature {
        names,
        params: declarations,
        result: spelled_result,
        handles: result.and_then(|spelling| spelling.handles.as_ref()),
    })
}

/// A call whose result is turned into the bindings' type of `module`, the interface's generated
/// module: each resource it holds is wrapped into its handle type.
fn wrapped(
    handles: Option<&Handles>,
    module: &proc_macro2::TokenStream,
    call: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    match handles {
        Some(handles) => wrap(handles, call, module),
        None => call,
    }
}

/// The author's function implementing the WIT function `name`: the one whose WIT name it is, so
/// `move_` or `r#move` implements `move` and `getEntry` implements `get-entry`. Two of them would
/// implement it twice.
fn implementing<'a>(
    functions: impl IntoIterator<Item = &'a Function>,
    name: &str,
) -> syn::Result<Option<&'a Function>> {
    let mut found = functions
        .into_iter()
        .filter(|function| wit::kebab(&function.ident) == name);
    let first = found.next();
    if let (Some(first), Some(second)) = (first, found.next()) {
        return Err(syn::Error::new(
            second.ident.span(),
            format!(
                "`{}` and `{}` both implement the WIT function `{name}`; rename one of them",
                first.ident, second.ident
            ),
        ));
    }
    Ok(first)
}

/// The `#[wit_resource]` struct implementing the resource `name` of `service`'s implemented
/// interface: the one of that WIT name the service owns.
fn implementation_of<'a>(
    resources: &'a [WitResource],
    service: &Ident,
    name: &str,
) -> Option<&'a WitResource> {
    resources.iter().find(|candidate| {
        wit::kebab(&candidate.ident) == name && candidate.service.as_ref() == Some(service)
    })
}

/// The bridge of an implemented interface, with signatures spelled from its WIT: the functions,
/// named as the bindings name them, forward to the author's functions of the same WIT names, in
/// the service's module, and each resource of the interface is the author's struct of its WIT
/// name, whose constructor, methods and static functions forward likewise from the struct's own
/// module, with the module's scope. There the author's functions are visible whatever their
/// visibility: a bridge elsewhere that could not see the author's `new` would call the trait's own
/// `new`, itself, forever.
fn implemented_service(
    service: &Service,
    identity: &str,
    interface: &wit::Consumed,
    resources: &[WitResource],
    modules: &[Vec<Ident>],
) -> syn::Result<(
    proc_macro2::TokenStream,
    Vec<(usize, proc_macro2::TokenStream)>,
)> {
    let depth = modules[service.scope].len();
    let root = supers(depth);
    let module = export_module(depth, &hook_path(identity));
    let target = &service.ident;
    let mut functions = Vec::new();
    for function in &interface.functions {
        let ident = rust_ident(&function.name);
        let Some(own) = implementing(&service.functions, &function.name)? else {
            continue;
        };
        let Signature {
            names,
            params,
            result,
            handles,
        } = signature(&function.params, function.result.as_ref(), depth)?;
        let (asynchronous, await_) = async_bridge(function.asynchronous);
        let author = &own.ident;
        let body = match own.context {
            Some(_) => native_only(target, author, &names),
            None => wrapped(
                handles,
                &module,
                answered(own, quote! { #target::#author(#(#names),*) #await_ }),
            ),
        };
        functions.push(quote! { #asynchronous fn #ident(#(#params),*) #result { #body } });
    }
    let mut owned = Vec::new();
    let mut implementations = Vec::new();
    for resource in &interface.resources {
        let Some(implementation) = implementation_of(resources, &service.ident, &resource.name)
        else {
            continue;
        };
        let ident = &implementation.ident;
        let camel = camel_ident(&resource.name);
        let struct_path = relative(depth, &modules[implementation.scope], ident);
        owned.push(quote! { type #camel = #struct_path; });
        // The resource's own bridge, in the struct's module.
        let local = modules[implementation.scope].len();
        let local_module = export_module(local, &hook_path(identity));
        let guest = format_ident!("Guest{}", camel);
        let constructor = match (&resource.constructor, &implementation.constructor) {
            (
                Some(params),
                Some(Constructor {
                    ident: constructor, ..
                }),
            ) => {
                let Signature { names, params, .. } = signature(params, None, local)?;
                quote! { fn new(#(#params),*) -> Self { #ident::#constructor(#(#names),*) } }
            }
            _ => quote! {},
        };
        let own: Vec<&Function> = implementation
            .methods
            .iter()
            .chain(&implementation.statics)
            .collect();
        let mut members = Vec::new();
        for (function, receiver) in resource
            .methods
            .iter()
            .map(|method| (method, true))
            .chain(resource.statics.iter().map(|function| (function, false)))
        {
            let Some(author) = implementing(own.iter().copied(), &function.name)? else {
                continue;
            };
            let author = &author.ident;
            let name = rust_ident(&function.name);
            let Signature {
                names,
                params,
                result,
                handles,
            } = signature(&function.params, function.result.as_ref(), local)?;
            let (asynchronous, await_) = async_bridge(function.asynchronous);
            let (receiver, call) = if receiver {
                (
                    quote! { &self, },
                    quote! { #ident::#author(self, #(#names),*) #await_ },
                )
            } else {
                (quote! {}, quote! { #ident::#author(#(#names),*) #await_ })
            };
            let body = wrapped(handles, &local_module, call);
            members
                .push(quote! { #asynchronous fn #name(#receiver #(#params),*) #result { #body } });
        }
        // The author's span, so that rustc's error on a function the struct lacks falls on it.
        implementations.push((
            implementation.scope,
            quote_spanned! { ident.span() =>
                impl #local_module::#guest for #ident {
                    #constructor
                    #(#members)*
                }
            },
        ));
    }
    // The service's span, so that rustc's error on a function it lacks falls on it.
    let bridge = quote_spanned! { target.span() =>
        impl #module::Guest for #root __TembraExports {
            #(#owned)*
            #(#functions)*
        }
    };
    Ok((bridge, implementations))
}

/// The type a name resolved in the module `scope` names among `named`, the types of the Orb that
/// take the name, each with the module declaring it: the one that module declares
/// (`foreign::Modules::references_in`), none when it declares none.
fn declared_in<T>(named: impl IntoIterator<Item = (usize, T)>, scope: usize) -> Option<T> {
    named
        .into_iter()
        .find(|(module, _)| *module == scope)
        .map(|(_, named)| named)
}

/// Keep, in module order, the candidate types that a derived service's method, a derived
/// resource's function or a derived hook's input reaches; the others, with their shapes, go back
/// to their modules as ordinary Rust. The resources of an implemented interface reach nothing:
/// their functions speak the consumed package's types, whatever the module's own are called. A
/// name resolves as rustc resolves it, in a signature as in a record's field
/// (`Modules::references_in`, `Candidate::references`), to the type of the module that declares
/// it, a resource included, which is a type of its own; a name that resolves to no struct or enum
/// of the module, a type of another shape among `types` or none, is an error, and so is a name the
/// macro does not follow (`Modules::resolved`), each with how to write it. What a consumed
/// interface's module names, a type or a handle, is no type of the module.
fn reachable(
    candidates: Vec<Candidate>,
    services: &[Service],
    resources: &[WitResource],
    types: &[(usize, Ident)],
    hook_inputs: &[(&Type, usize)],
    modules: &foreign::Modules,
    unreached: &mut Vec<Candidate>,
) -> syn::Result<Vec<Shape>> {
    let mut declared: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        declared
            .entry(candidate.ident.to_string())
            .or_default()
            .push(index);
    }
    let mut pending: Vec<(Ident, usize)> = Vec::new();
    let mention = |ty: &Type, scope: usize, pending: &mut Vec<(Ident, usize)>| {
        modules.resolved(ty, scope)?;
        modules.references_in(ty, scope, pending);
        Ok::<(), syn::Error>(())
    };
    for function in resource_params::derived(services, resources) {
        for (_, ty, _) in function.params {
            mention(ty, function.scope, &mut pending)?;
        }
        if let Some(ty) = function.result {
            mention(ty, function.scope, &mut pending)?;
        }
    }
    for (input, scope) in hook_inputs {
        mention(input, *scope, &mut pending)?;
    }
    let mut kept = BTreeSet::new();
    while let Some((ident, scope)) = pending.pop() {
        let indexes = declared
            .get(&ident.to_string())
            .map_or(&[][..], Vec::as_slice);
        if resource_params::resource_named(&ident, scope, resources).is_some() {
            continue;
        }
        let named = indexes
            .iter()
            .map(|&index| (candidates[index].scope, index));
        let Some(index) = declared_in(named, scope) else {
            let declares = types
                .iter()
                .any(|(module, name)| *module == scope && *name == ident);
            // A type of that name another module declares, which a `use` imports by name.
            let elsewhere = indexes
                .iter()
                .map(|&index| candidates[index].scope)
                .chain(
                    resources
                        .iter()
                        .filter(|resource| resource.ident == ident)
                        .map(|resource| resource.scope),
                )
                .next();
            // Free JSON imported by `use` reads as a type of the module: it is free JSON only by
            // its path.
            let hint = match elsewhere {
                _ if ident == "Value" => {
                    "; for free JSON, write it by its path, `serde_json::Value`".to_owned()
                }
                Some(module) if !declares => format!(
                    "; `{ident}` is declared in {}, which this module names through its path or a `use` of it, `use {}::{ident};`",
                    describe(modules.path(module)),
                    modules.path_to(scope, module)
                ),
                _ => String::new(),
            };
            return Err(syn::Error::new(
                ident.span(),
                format!(
                    "`{ident}` is used by a service method or a hook but is not a struct or enum of this module{hint}"
                ),
            ));
        };
        if kept.insert(index) {
            modules.item_resolved(&candidates[index].item, candidates[index].scope)?;
            pending.extend(candidates[index].references.iter().cloned());
        }
    }
    // Only the kept candidates are translated: what they hold must have a WIT spelling.
    let mut shapes = Vec::new();
    for (index, candidate) in candidates.into_iter().enumerate() {
        if kept.contains(&index) {
            shapes.push(candidate.translate(modules)?);
        } else {
            unreached.push(candidate);
        }
    }
    Ok(shapes)
}

/// The newtypes a type that enters the WIT names, in module order, each with the service or the
/// derived hook whose interface names it: in its functions or its resources' functions, in a
/// derived hook's input, through the fields of a type the interface defines, or through the field
/// of another newtype the interface names. That interface defines or `use`s every type of the
/// module the newtype's field holds, so its bindings name each of them.
fn named_newtypes(
    newtypes: &[Newtype],
    (services, resources, declared): (&[Service], &[WitResource], &[Declared]),
    shapes: &[Shape],
    placement: &BTreeMap<String, Ident>,
    modules: &foreign::Modules,
) -> Vec<(Newtype, Ident)> {
    let mut pending: Vec<(&Newtype, &Ident)> = Vec::new();
    for function in resource_params::derived(services, resources) {
        for ty in function
            .params
            .iter()
            .map(|(_, ty, _)| ty)
            .chain(function.result)
        {
            for newtype in modules.newtypes_in(ty, function.scope) {
                pending.push((newtype, function.interface));
            }
        }
    }
    for declaration in declared {
        let Some((input, _)) = &declaration.input else {
            continue;
        };
        for newtype in modules.newtypes_in(input, declaration.scope) {
            pending.push((newtype, &declaration.ident));
        }
    }
    for shape in shapes {
        let Some(interface) = placement.get(&shape.ident.to_string()) else {
            continue;
        };
        for ty in foreign::shape_types(shape) {
            for newtype in modules.newtypes_in(ty, shape.scope) {
                pending.push((newtype, interface));
            }
        }
    }
    let mut named: BTreeMap<String, &Ident> = BTreeMap::new();
    let mut next = 0;
    while let Some(&(newtype, interface)) = pending.get(next) {
        next += 1;
        if named.contains_key(&newtype.ident.to_string()) {
            continue;
        }
        named.insert(newtype.ident.to_string(), interface);
        for inner in modules.newtypes_in(&newtype.inner, newtype.scope) {
            pending.push((inner, interface));
        }
    }
    newtypes
        .iter()
        .filter_map(|newtype| {
            let interface = named.get(&newtype.ident.to_string())?;
            Some((newtype.clone(), (*interface).clone()))
        })
        .collect()
}

/// Refuse what the macro cannot evaluate in a type that enters the WIT, as it reads the type before
/// the compiler evaluates `#[cfg(..)]`: the type under `#[cfg(..)]`, which the WIT would hold, and
/// the macro convert, in every configuration; and a field, a case or a case's payload under
/// `#[cfg(..)]`, which the WIT would hold in every configuration. The type keeps its other
/// attributes as the author writes them.
fn unconditional(item: &Item) -> syn::Result<()> {
    // Each member, by what it is and its name as the errors say them.
    let (owner, attrs, members) = match item {
        Item::Struct(item) => (
            &item.ident,
            &item.attrs,
            item.fields
                .iter()
                .filter_map(|field| {
                    let name = format!("`{}::{}`", item.ident, field.ident.as_ref()?);
                    Some(("field", name, &field.attrs))
                })
                .collect::<Vec<_>>(),
        ),
        Item::Enum(item) => (
            &item.ident,
            &item.attrs,
            item.variants
                .iter()
                .flat_map(|variant| {
                    let case = format!("`{}::{}`", item.ident, variant.ident);
                    std::iter::once(("case", case.clone(), &variant.attrs)).chain(
                        variant
                            .fields
                            .iter()
                            .map(move |field| ("payload", format!("of {case}"), &field.attrs)),
                    )
                })
                .collect(),
        ),
        _ => return Ok(()),
    };
    if let Some(attribute) = attrs
        .iter()
        .find(|attribute| attribute.path().is_ident("cfg"))
    {
        let kind = match item {
            Item::Enum(item)
                if item
                    .variants
                    .iter()
                    .all(|variant| matches!(variant.fields, Fields::Unit)) =>
            {
                "enum"
            }
            Item::Enum(_) => "variant",
            _ => "record",
        };
        let named = format!("the macro would convert `{owner}` to and from the bindings' type");
        return Err(syn::Error::new(
            attribute.span(),
            format!(
                "`#[cfg(..)]` on the {kind} `{owner}`: `{owner}` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the {kind} and {named} in every configuration; remove the attribute, or keep `{owner}` out of the WIT"
            ),
        ));
    }
    for (kind, name, attrs) in &members {
        if let Some(attribute) = attrs
            .iter()
            .find(|attribute| attribute.path().is_ident("cfg"))
        {
            return Err(syn::Error::new(
                attribute.span(),
                format!(
                    "`#[cfg(..)]` on the {kind} {name}: `{owner}` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the {kind} in every configuration; remove the attribute, or keep `{owner}` out of the WIT"
                ),
            ));
        }
    }
    Ok(())
}

/// Refuse a `#[cfg(..)]`, written plainly or under `#[cfg_attr(..)]`, among the attributes of an
/// item the macro writes code for: a struct it marks, a function it calls, or the `impl` block or
/// the sub-module holding one. `what` the item is, the struct or function the code `names` and
/// what that code `does`, for the error: the macro reads the item before the compiler evaluates
/// the attribute, and that code names the struct or the function in every configuration, also
/// where the attribute is off and the compiler drops the item. Where the macro evaluates a plain
/// `#[cfg(..)]` itself (`cfg`), `evaluated`, on a service, its `impl` blocks and their functions,
/// a hook and a sub-module holding one of these, only one under `#[cfg_attr(..)]` is refused, which
/// it cannot evaluate, pointing to the plain form.
fn unconditional_marked(
    attrs: &[Attribute],
    what: &str,
    names: impl std::fmt::Display,
    does: &str,
    evaluated: bool,
) -> syn::Result<()> {
    let Some(attribute) = attrs.iter().find(|attribute| {
        conditional(&attribute.meta) && !(evaluated && attribute.path().is_ident("cfg"))
    }) else {
        return Ok(());
    };
    let written = written_condition(attribute);
    let instead = if evaluated {
        "write the condition as a plain `#[cfg(..)]`, which the macro evaluates"
    } else {
        "remove the attribute"
    };
    Err(syn::Error::new(
        attribute.span(),
        format!(
            "{written} on the {what} `{names}`: the macro writes the code that {does} before the compiler evaluates the attribute, so that code would name `{names}` in every configuration; {instead}"
        ),
    ))
}

/// Refuse a `#[cfg(..)]`, written plainly or under `#[cfg_attr(..)]`, on a parameter, `self`
/// included, of the function `names` the macro calls, which is `what`, for the error: the macro
/// reads the parameters before the compiler evaluates the attribute, and the code calling the
/// function passes each of them in every configuration, also where the attribute is off and the
/// compiler drops the parameter.
fn unconditional_params(
    signature: &syn::Signature,
    what: &str,
    names: impl std::fmt::Display,
) -> syn::Result<()> {
    for input in &signature.inputs {
        let (attrs, param) = match input {
            FnArg::Receiver(receiver) => (&receiver.attrs, "self".to_owned()),
            FnArg::Typed(typed) => {
                let pat = &typed.pat;
                (&typed.attrs, quote!(#pat).to_string())
            }
        };
        let Some(attribute) = attrs.iter().find(|attribute| conditional(&attribute.meta)) else {
            continue;
        };
        return Err(syn::Error::new(
            attribute.span(),
            format!(
                "{} on the parameter `{param}` of the {what} `{names}`: the macro writes the code that calls it before the compiler evaluates the attribute, so that code would pass `{param}` in every configuration; remove the attribute",
                written_condition(attribute)
            ),
        ));
    }
    Ok(())
}

/// Whether an attribute drops what it sits on where a configuration is off: a `#[cfg(..)]`, or a
/// `#[cfg_attr(..)]` wrapping one, directly or through another `#[cfg_attr(..)]`.
fn conditional(meta: &syn::Meta) -> bool {
    if meta.path().is_ident("cfg") {
        return true;
    }
    if !meta.path().is_ident("cfg_attr") {
        return false;
    }
    let wrapped = meta.require_list().and_then(|list| {
        list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated)
    });
    // The first is the configuration predicate.
    wrapped.is_ok_and(|wrapped| wrapped.iter().skip(1).any(conditional))
}

/// How the errors write an attribute that drops what it sits on where a configuration is off
/// (`conditional`).
fn written_condition(attribute: &Attribute) -> &'static str {
    if attribute.path().is_ident("cfg") {
        "`#[cfg(..)]`"
    } else {
        "`#[cfg_attr(..)]` wrapping `#[cfg(..)]`"
    }
}

/// The module enclosing the module `scope`, by scope, unless that is the Orb module.
fn parent(scope: usize, modules: &[Vec<Ident>]) -> Option<usize> {
    let (_, parent) = modules[scope].split_last()?;
    Some(
        modules
            .iter()
            .position(|module| module == parent)
            .expect("the parent of a scanned module"),
    )
}

/// What the glob imports of a module make of a name it neither binds nor declares.
enum Globbed {
    /// A module of the Orb that a glob imports from binds or declares it: the name resolves there.
    In(usize),
    /// None does, and a glob imports from a module the macro does not read, which may bring it.
    Outside,
    /// None does, and every glob imports from a module of the Orb.
    Nowhere,
}

/// Where the glob imports of the module `scope` bring `name` from: the first module of the Orb,
/// through the module's globs and those of the modules they import from, that binds the name with
/// `use` or declares an item of that name, a sub-module included. Each glob's own path is read
/// without this step (`module_from`): a glob may import from a module another glob brings in, and
/// following those in a circle would never end.
fn through_globs(name: &Ident, scope: usize, uses: &[Uses], modules: &[Vec<Ident>]) -> Globbed {
    let mut outside = false;
    let mut seen = BTreeSet::from([scope]);
    let mut pending = vec![scope];
    while let Some(scope) = pending.pop() {
        for glob in &uses[scope].globs {
            let Prefix::Module(module) = module_from(glob, scope, uses, modules, false) else {
                outside = true;
                continue;
            };
            let name = name.to_string();
            if uses[module].names.contains_key(&name) || uses[module].items.contains(&name) {
                return Globbed::In(module);
            }
            if seen.insert(module) {
                pending.push(module);
            }
        }
    }
    if outside {
        Globbed::Outside
    } else {
        Globbed::Nowhere
    }
}

/// Where the prefix of a path leads, a glob import's or a type path's (`module_from`).
enum Prefix {
    /// A module of the Orb, by scope.
    Module(usize),
    /// A path through `crate::` that a `use` of the Orb's modules binds a name of the prefix to:
    /// the macro can neither place it in the Orb nor tell that it leaves the Orb module.
    Crate,
    /// A module the macro does not read: one of a crate, under a `crate::` the prefix writes
    /// itself, or above the Orb module.
    Outside,
}

/// Where `glob` leads (`glob_module`), a `crate::` a `use` leads to told apart from a module the
/// macro does not read otherwise; a first name the module neither binds nor declares a sub-module
/// of is followed through the module's glob imports only when `globs` is set.
fn module_from(
    glob: &Named,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    globs: bool,
) -> Prefix {
    if glob.global {
        return Prefix::Outside;
    }
    let mut scope = scope;
    let mut segments = glob.segments.clone();
    // Whether a `use` has replaced a name by the path it imports.
    let mut imported = false;
    // Each step consumes a name or replaces one by what a declaration binds it to.
    for _ in 0..64 {
        let Some((first, rest)) = segments.split_first() else {
            return Prefix::Module(scope);
        };
        let (first, rest) = (first.clone(), rest.to_vec());
        if first == "crate" {
            return if imported {
                Prefix::Crate
            } else {
                Prefix::Outside
            };
        }
        if first == "super" {
            let Some(above) = parent(scope, modules) else {
                return Prefix::Outside;
            };
            scope = above;
        }
        if first == "self" || first == "super" {
            segments = rest;
            continue;
        }
        if let Some(bound) = uses[scope].names.get(&first.to_string()) {
            // `use name;` brings the crate `name` into scope.
            if bound.global || (bound.segments.len() == 1 && bound.segments[0] == first) {
                return Prefix::Outside;
            }
            segments = bound.segments.clone();
            segments.extend(rest);
            imported = true;
            continue;
        }
        let mut child = modules[scope].clone();
        child.push(first.clone());
        if let Some(index) = modules.iter().position(|module| *module == child) {
            scope = index;
            segments = rest;
            continue;
        }
        // A name the module neither binds nor declares a sub-module of: a glob import may bring
        // the module in from another module of the Orb, where the name resolves next.
        if !globs {
            return Prefix::Outside;
        }
        let Globbed::In(module) = through_globs(&first, scope, uses, modules) else {
            return Prefix::Outside;
        };
        scope = module;
    }
    Prefix::Outside
}

impl quote::ToTokens for Named {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let global = self.global.then(|| quote! { :: });
        let segments = &self.segments;
        tokens.extend(quote! { #global #(#segments)::* });
    }
}

/// Read one public function of a service: a plain name for each owned parameter, `&self` when
/// the service's create builds it, and at most one `&Context`, the call's, which stays out of the
/// WIT. A result spelled `tembra::Result<T>`, through `facade`, the name the crate depends on the
/// facade under, is the method's error channel, so its WIT result is `T`. A `local` method stays
/// out of the WIT whole, so its types need no WIT spelling; its documentation goes to its client's
/// method, as any method's does.
fn method(function: &syn::ImplItemFn, local: bool, facade: &Ident) -> syn::Result<Function> {
    let spelled = |ty: &Type| {
        if local {
            Ok(String::new())
        } else {
            wit::wit_type(ty)
        }
    };
    let mut params = Vec::new();
    let mut receiver = false;
    let mut context = None;
    for input in &function.sig.inputs {
        match input {
            FnArg::Receiver(borrowed)
                if borrowed.reference.is_some()
                    && borrowed.mutability.is_none()
                    && borrowed.colon_token.is_none() =>
            {
                receiver = true;
            }
            FnArg::Receiver(other) => {
                return Err(syn::Error::new(
                    other.span(),
                    "a method borrows `&self`; change the service through Cell or RefCell",
                ));
            }
            FnArg::Typed(typed) if is_context(&typed.ty) => {
                if context.replace(params.len()).is_some() {
                    return Err(syn::Error::new(
                        typed.span(),
                        "a method takes the call's `&Context` once",
                    ));
                }
            }
            FnArg::Typed(typed) => {
                let Pat::Ident(pat) = &*typed.pat else {
                    return Err(syn::Error::new(
                        typed.span(),
                        "parameters need a plain name",
                    ));
                };
                params.push((pat.ident.clone(), (*typed.ty).clone(), spelled(&typed.ty)?));
            }
        }
    }
    let output = result(&function.sig.output);
    let value = output.and_then(|ty| tembra_result(ty, facade));
    let result = match value.or(output) {
        Some(Type::Tuple(unit)) if unit.elems.is_empty() => None,
        Some(ty) => Some((ty.clone(), spelled(ty)?)),
        None => None,
    };
    Ok(Function {
        ident: function.sig.ident.clone(),
        asynchronous: function.sig.asyncness.is_some(),
        receiver,
        docs: wit::docs(&function.attrs)?,
        params,
        result,
        fallible: value.is_some(),
        context,
    })
}

/// The value of a result spelled `tembra::Result<T>`, the runtime's, `T`, through `facade`, the
/// name the crate depends on the facade under: a native method's error channel rather than a WIT
/// `result`, which only `Result<T, E>` spells.
fn tembra_result<'a>(ty: &'a Type, facade: &Ident) -> Option<&'a Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segments: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
    let ([tembra, result], None) = (segments.as_slice(), &path.qself) else {
        return None;
    };
    let syn::PathArguments::AngleBracketed(arguments) = &result.arguments else {
        return None;
    };
    let mut arguments = arguments.args.iter();
    match (arguments.next(), arguments.next()) {
        (Some(syn::GenericArgument::Type(value)), None)
            if tembra.ident == *facade
                && tembra.arguments.is_empty()
                && result.ident == "Result" =>
        {
            Some(value)
        }
        _ => None,
    }
}

/// Whether the module `scope` declares a type named `name` of its own: one of its candidate
/// types, or a struct, a union or a type alias it keeps as written.
fn declares(tree: &mut Module, candidates: &[Candidate], scope: usize, name: &str) -> bool {
    candidates
        .iter()
        .any(|candidate| candidate.scope == scope && candidate.ident == name)
        || tree.at(scope).items.iter().any(|item| match item {
            Item::Struct(item) => item.ident == name,
            Item::Union(item) => item.ident == name,
            Item::Type(item) => item.ident == name,
            _ => false,
        })
}

/// Whether a parameter takes the call's context: `&Context`, through any path to `Context`. No
/// WIT type is a reference, so the name alone tells it apart where the module declares no
/// `Context` of its own.
fn is_context(ty: &Type) -> bool {
    let Type::Reference(reference) = ty else {
        return false;
    };
    let Type::Path(path) = &*reference.elem else {
        return false;
    };
    reference.mutability.is_none()
        && path.qself.is_none()
        && path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Context" && segment.arguments.is_none())
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
    let id = wit_identity(&hook, "a hook", true)?;
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
        (Some(hook), None) => (wit_identity(&hook, "a hook", true)?, None),
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
        docs: wit::docs(&item.attrs)?,
        input,
        scope: 0,
    })
}

/// The code of one declared hook, for the module declaring it: the export the contract requires,
/// which traps because the contributions answer the hook, and `run` on the author's struct, which
/// calls the matching import so the kernel runs the plan. A derived hook's `run` speaks the
/// module's own types and converts them to and from the import side of the bindings, those the
/// macro converts, `converted`, and the newtypes `modules` reads included.
fn declaration_code(
    declaration: &Declared,
    callbacks: &BTreeMap<String, wit::Callback>,
    hook_shapes: &BTreeSet<String>,
    holding: &BTreeSet<String>,
    defining: Option<&[Ident; 3]>,
    (converted, modules): (&BTreeMap<String, Ident>, &foreign::Modules),
    depth: usize,
) -> syn::Result<proc_macro2::TokenStream> {
    let path = hook_path(&declaration.hook);
    let root = supers(depth);
    let export = export_module(depth, &path);
    let import = import_module(depth, &path);
    if let Some((input, _)) = &declaration.input {
        let defining = defining.expect("a derived hook defines types");
        return Ok(derived_hook_code(
            declaration,
            input,
            (hook_shapes, holding),
            (
                &export_module(depth, defining),
                &import_module(depth, defining),
            ),
            (converted, modules),
            depth,
        ));
    }
    let callback = &callbacks[&declaration.hook];
    let name = rust_ident(&callback.name);
    // The import takes a string, a list or a type holding one by reference, and an option, a
    // result or a tuple around them piece by piece.
    let argument = match &callback.passed {
        wit::Passed::Owned => quote! { input },
        wit::Passed::Str | wit::Passed::List | wit::Passed::Named => quote! { &input },
        parts => lent(parts, quote! { &input }),
    };
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
                #import::#name(#argument)
            }
        }
    })
}

/// An expression lending the parts of `value`, a reference to a hook's input on the import side,
/// the way the import takes them: a string as `&str`, a list as `&[T]`, another named type holding
/// one as `&T`, the options, results and tuples around them rebuilt, and every other part copied,
/// a scalar or a type holding no string, list or handle.
fn lent(passed: &wit::Passed, value: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    match passed {
        wit::Passed::Owned => quote! { ::core::clone::Clone::clone(#value) },
        wit::Passed::Str => quote! { #value.as_str() },
        wit::Passed::List => quote! { #value.as_slice() },
        wit::Passed::Named => value,
        wit::Passed::Option(inner) => {
            let inner = lent(inner, quote! { item });
            quote! {
                match #value {
                    ::core::option::Option::Some(item) => ::core::option::Option::Some(#inner),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }
        }
        wit::Passed::Result(ok, err) => {
            let ok = lent(ok, quote! { item });
            let err = lent(err, quote! { item });
            quote! {
                match #value {
                    ::core::result::Result::Ok(item) => ::core::result::Result::Ok(#ok),
                    ::core::result::Result::Err(item) => ::core::result::Result::Err(#err),
                }
            }
        }
        wit::Passed::Tuple(items) => {
            let names: Vec<Ident> = (0..items.len())
                .map(|index| format_ident!("item{index}"))
                .collect();
            let parts = items
                .iter()
                .zip(&names)
                .map(|(passed, name)| lent(passed, quote! { #name }));
            quote! { { let (#(#names,)*) = #value; (#(#parts,)*) } }
        }
    }
}

/// The code of a hook derived from `input = Type`: the export the contract requires, which traps
/// because the contributions answer the hook, and `run` on the author's struct, which converts
/// the module's types to the import side of the bindings, calls the hook's import so the kernel
/// runs the plan, and converts the composed result back. The export takes the types the macro
/// converts, `converted`, as the bindings' copies of `export_types`, as a service's bridge does,
/// and `run` converts them, and the newtypes `modules` reads, to and from `import_types`.
fn derived_hook_code(
    declaration: &Declared,
    input: &Type,
    (hook_shapes, holding): (&BTreeSet<String>, &BTreeSet<String>),
    (export_types, import_types): (&proc_macro2::TokenStream, &proc_macro2::TokenStream),
    (converted, modules): (&BTreeMap<String, Ident>, &foreign::Modules),
    depth: usize,
) -> proc_macro2::TokenStream {
    let path = hook_path(&declaration.hook);
    let root = supers(depth);
    let export = export_module(depth, &path);
    let import = import_module(depth, &path);
    let ident = &declaration.ident;
    let scope = declaration.scope;
    let export_input = with_handles(input, scope, converted, export_types, modules);
    let import_input = respell(input, hook_shapes, import_types, (modules, scope));
    // A hook's input holds no resource: `resource_params::check_reached` refuses one.
    let resources = BTreeMap::new();
    let at = Some(At {
        modules,
        scope,
        resources: &resources,
        module: import_types,
        import: true,
    });
    let (result, exported, settle) = match declaration.composition {
        Composition::Series => (quote! {}, quote! {}, None),
        Composition::Waterfall => (
            quote! { -> #input },
            quote! { -> #export_input },
            Some(convert(input, quote! { answer }, Towards::Author, at)),
        ),
        Composition::Bail => {
            let item = convert(input, quote! { item }, Towards::Author, at);
            (
                quote! { -> ::core::option::Option<#input> },
                quote! { -> ::core::option::Option<#export_input> },
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
    let converted = convert(input, quote! { input }, Towards::Bindings, at);
    // A string, a list, or a module type that holds one anywhere crosses to an import by
    // reference, as the bindings take them.
    let by_reference = holds_list(input, holding, (modules, scope));
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
    quote! {
        impl #export::Guest for #root __TembraExports {
            fn apply(_input: #export_input) #exported {
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

/// The module types whose values hold a string or a list anywhere, in their fields or payloads,
/// directly or through other module types and the newtypes `modules` reads.
fn holding_lists(shapes: &[Shape], modules: &foreign::Modules) -> BTreeSet<String> {
    let mut holding = BTreeSet::new();
    loop {
        let known = holding.len();
        for shape in shapes {
            if foreign::shape_types(shape)
                .iter()
                .any(|ty| holds_list(ty, &holding, (modules, shape.scope)))
            {
                holding.insert(shape.ident.to_string());
            }
        }
        if holding.len() == known {
            return holding;
        }
    }
}

/// Whether a type of the module, written in the module `scope`, holds a string or a list: it is
/// one, free JSON, which the bindings hold as its text, or a newtype whose field holds one, its
/// type parameters or its tuple elements hold one, or it is a module type of `holding`.
fn holds_list(
    ty: &Type,
    holding: &BTreeSet<String>,
    (modules, scope): (&foreign::Modules, usize),
) -> bool {
    if wit::free_json(ty) {
        return true;
    }
    if let Some(newtype) = modules.newtype(ty, scope) {
        return holds_list(&newtype.inner, holding, (modules, newtype.scope));
    }
    match ty {
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let name = segment.ident.to_string();
            let resolved = modules
                .module_type(ty, scope)
                .is_some_and(|(ident, _)| holding.contains(&ident.to_string()));
            let arguments = match &segment.arguments {
                syn::PathArguments::AngleBracketed(arguments) => arguments
                    .args
                    .iter()
                    .any(|argument| matches!(argument, syn::GenericArgument::Type(ty) if holds_list(ty, holding, (modules, scope)))),
                _ => false,
            };
            name == "String" || name == "Vec" || resolved || arguments
        }),
        Type::Tuple(tuple) => tuple
            .elems
            .iter()
            .any(|ty| holds_list(ty, holding, (modules, scope))),
        _ => false,
    }
}

/// Which side of the bindings a conversion goes to, which free JSON needs: the author's
/// `serde_json::Value` is the bindings' `String` of its text, written one way and read the other.
#[derive(Clone, Copy)]
enum Towards {
    /// From the author's type to the bindings'.
    Bindings,
    /// From the bindings' type to the author's.
    Author,
}

/// Where a conversion is written: the modules, the module's scope, which the newtypes and the
/// resources are read in, and the resources of the module, by the author's name of the struct,
/// each with the name of its handle type in `module`, the bindings' module of the other side.
#[derive(Clone, Copy)]
struct At<'a> {
    modules: &'a foreign::Modules,
    scope: usize,
    resources: &'a BTreeMap<String, Ident>,
    module: &'a proc_macro2::TokenStream,
    /// Whether `module` is the import side of a derived hook's bindings, whose copy of a type a
    /// newtype reaches through `IMPORT_NEWTYPE` rather than `NEWTYPE`.
    import: bool,
}

/// The side of the bindings a newtype's conversions reach (`newtype_conversions`): the export of
/// the interface that names it, where the types the macro converts, `converted`, are the bindings'
/// own, or the import of a derived hook, whose copies of the types the hooks reach, `hook_shapes`,
/// are another Rust type of the same WIT definition.
#[derive(Clone, Copy)]
enum Side<'a> {
    Export {
        converted: &'a BTreeMap<String, Ident>,
    },
    Import {
        hook_shapes: &'a BTreeSet<String>,
    },
}

/// An expression converting `value`, of the author's type `ty` on one side of the bindings, to the
/// other side, `towards`: containers are rebuilt element by element, free JSON is written as its
/// text or read from it, a newtype goes through the trait it implements, `NEWTYPE`, an own
/// resource is wrapped into its handle type or taken back from it, and everything else goes
/// through `Into`, the identity for scalars and strings, the generated `From` for the module's
/// types. A text that holds no JSON traps, as a component does when it cannot answer: only a
/// native Orb has an error channel for it; a component reads a newtype through `From`, which
/// refuses no value. `at` is where the conversion is written, for the newtypes and the resources,
/// which the bindings' two copies of a hook's types never hold.
fn convert(
    ty: &Type,
    value: proc_macro2::TokenStream,
    towards: Towards,
    at: Option<At<'_>>,
) -> proc_macro2::TokenStream {
    if let Some(at) = at
        && at.modules.newtype(ty, at.scope).is_some()
    {
        if at.import {
            let newtype = at.modules.import_newtype_trait(at.scope);
            return match towards {
                Towards::Bindings => quote! { <#ty as #newtype>::__tembra_into_import(#value) },
                Towards::Author => quote! { <#ty as #newtype>::__tembra_from_import(#value) },
            };
        }
        let newtype = at.modules.newtype_trait(at.scope);
        return match towards {
            Towards::Bindings => quote! { <#ty as #newtype>::__tembra_into_bindings(#value) },
            Towards::Author => quote! { <#ty as #newtype>::__tembra_from_bindings(#value) },
        };
    }
    if let Some(at) = at
        && let Some(handles @ Handles::Resource(_)) =
            handles_of(ty, at.scope, at.resources, at.modules)
    {
        return match towards {
            Towards::Bindings => wrap(&handles, value, at.module),
            Towards::Author => resource_params::taken(&handles, value),
        };
    }
    if let (true, Type::Path(path)) = (wit::free_json(ty), ty) {
        // The crate as the author's path names it, `serde_json` or `::serde_json`.
        let leading = &path.path.leading_colon;
        let segments = path.path.segments.iter().take(path.path.segments.len() - 1);
        let json = quote! { #leading #(#segments)::* };
        let (read, value) = match towards {
            Towards::Bindings => (quote! { #json::to_string(&#value) }, quote! { text }),
            Towards::Author => (quote! { #json::from_str::<#ty>(&#value) }, quote! { json }),
        };
        return quote! {
            match #read {
                ::core::result::Result::Ok(#value) => #value,
                ::core::result::Result::Err(error) => ::core::panic!("{}", error),
            }
        };
    }
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
                    let item = convert(item, quote! { item }, towards, at);
                    quote! { #value.into_iter().map(|item| #item).collect() }
                }
                ("Option", [item]) => {
                    let item = convert(item, quote! { item }, towards, at);
                    quote! { #value.map(|item| #item) }
                }
                ("Result", [ok, err]) => {
                    let ok = convert(ok, quote! { item }, towards, at);
                    let err = convert(err, quote! { item }, towards, at);
                    quote! {
                        match #value {
                            ::core::result::Result::Ok(item) => ::core::result::Result::Ok(#ok),
                            ::core::result::Result::Err(item) => ::core::result::Result::Err(#err),
                        }
                    }
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
                .map(|(ty, name)| convert(ty, quote! { #name }, towards, at));
            quote! { { let (#(#names,)*) = #value; (#(#converted,)*) } }
        }
        _ => quote! { ::core::convert::Into::into(#value) },
    }
}

/// The author's type, written in the module `scope`, as the import side of the bindings spells it:
/// a type of the module that a hook reaches, by the name `modules` resolves it to, lives in
/// `module` there, free JSON is a string, a newtype the import side of its field, which its trait
/// `IMPORT_NEWTYPE` names, as its own module spells the field, everything else keeps its spelling.
fn respell(
    ty: &Type,
    hook_shapes: &BTreeSet<String>,
    module: &proc_macro2::TokenStream,
    (modules, scope): (&foreign::Modules, usize),
) -> proc_macro2::TokenStream {
    if wit::free_json(ty) {
        return quote! { __tembra_alloc::string::String };
    }
    if modules.newtype(ty, scope).is_some() {
        let newtype = modules.import_newtype_trait(scope);
        return quote! { <#ty as #newtype>::Import };
    }
    match ty {
        Type::Path(path) => {
            let segments: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
            let Some((last, prefix)) = segments.split_last() else {
                return quote! { #ty };
            };
            let ident = &last.ident;
            if let Some((resolved, _)) = modules.module_type(ty, scope)
                && last.arguments.is_none()
                && hook_shapes.contains(&resolved.to_string())
            {
                let generated = type_ident(&wit::kebab(&resolved));
                return quote! { #module::#generated };
            }
            let syn::PathArguments::AngleBracketed(arguments) = &last.arguments else {
                return quote! { #ty };
            };
            let leading = &path.path.leading_colon;
            let prefix = prefix.iter().map(|segment| quote! { #segment:: });
            let arguments = arguments.args.iter().map(|argument| match argument {
                syn::GenericArgument::Type(ty) => {
                    respell(ty, hook_shapes, module, (modules, scope))
                }
                other => quote! { #other },
            });
            quote! { #leading #(#prefix)* #ident<#(#arguments),*> }
        }
        Type::Tuple(tuple) => {
            let elements = tuple
                .elems
                .iter()
                .map(|ty| respell(ty, hook_shapes, module, (modules, scope)));
            quote! { (#(#elements,)*) }
        }
        _ => quote! { #ty },
    }
}

/// The types that enter the WIT and stay the author's, converted to and from the bindings' types
/// at the boundary, by name, each with the bindings' name of its type: every record, variant and
/// enum of the module. The bindings' types are internal to the generated code, so the author's
/// type keeps its fields' privacy, its names and its attributes as written, and the author's
/// impls, which never clash with the bindings'.
fn converted(shapes: &[Shape]) -> BTreeMap<String, Ident> {
    shapes
        .iter()
        .map(|shape| {
            let generated = type_ident(&wit::kebab(&shape.ident));
            (shape.ident.to_string(), generated)
        })
        .collect()
}

/// Whether a type written in the module `scope` names a type the macro converts, free JSON or a
/// newtype, itself or inside its type parameters and tuples.
pub(crate) fn mentions(
    ty: &Type,
    scope: usize,
    converted: &BTreeMap<String, Ident>,
    modules: &foreign::Modules,
) -> bool {
    if wit::holds_free_json(ty) || modules.holds_newtype(ty, scope) {
        return true;
    }
    let mut names = Vec::new();
    modules.references(ty, scope, &mut names);
    names
        .iter()
        .any(|name| converted.contains_key(&name.to_string()))
}

/// Refuse a member without a WIT name on a type that enters the WIT and stays the author's: a
/// field, a case, or a field of a case, whose kebab-case form is no WIT identifier. The bindings
/// may name such a member otherwise, from its WIT name: the conversions use both names.
fn wit_named(item: &Item) -> syn::Result<()> {
    let members: Vec<(String, &Ident, &str)> = match item {
        Item::Struct(item) => item
            .fields
            .iter()
            .filter_map(|field| field.ident.as_ref())
            .map(|field| (item.ident.to_string(), field, "field"))
            .collect(),
        Item::Enum(item) => item
            .variants
            .iter()
            .flat_map(|variant| {
                let case = format!("{}::{}", item.ident, variant.ident);
                std::iter::once((item.ident.to_string(), &variant.ident, "case")).chain(
                    variant
                        .fields
                        .iter()
                        .filter_map(|field| field.ident.as_ref())
                        .map(move |field| (case.clone(), field, "field")),
                )
            })
            .collect(),
        _ => Vec::new(),
    };
    for (owner, member, kind) in members {
        let name = wit::kebab(member);
        if let Some(problem) = wit::name_problem(&name) {
            return Err(syn::Error::new(
                member.span(),
                format!(
                    "the {kind} `{owner}::{member}` has no WIT name: its kebab-case form `{name}` {problem}"
                ),
            ));
        }
    }
    Ok(())
}

/// Put each type the macro converts back in its module, as written, with the derives the author
/// writes. Its conversions are the component's to write, `converted_conversions`.
fn keep_converted(shapes: &[Shape], converted: &BTreeMap<String, Ident>, tree: &mut Module) {
    for shape in shapes
        .iter()
        .filter(|shape| converted.contains_key(&shape.ident.to_string()))
    {
        tree.at(shape.scope).items.push(shape.item.clone());
    }
}

/// The conversions between each type the macro converts and the bindings' type: `From` in both
/// directions, in the type's module, where its fields are visible. A record goes field by field,
/// an enum and a variant case by case, a case with named fields into the bindings' tuple case
/// around the record of the case; each part goes through `Into`, the identity for a scalar, a string, a
/// generated type and a consumed package's type, and the generated `From` for a type the macro
/// converts, inside lists, options, results and tuples too, a newtype `consumed` reads through
/// its trait, and an own resource of `resources`, by the author's name of the struct, through its
/// handle type. The bindings name a field or a case from its WIT name, so each side keeps its own
/// names. A type a derived hook's input reaches, of `hook_shapes`, has a second copy in the
/// bindings, the import's of the hook that defines it, `defining`, which it converts to and from
/// the same way.
fn converted_conversions(
    shapes: &[Shape],
    converted: &BTreeMap<String, Ident>,
    (placement, resources): (&BTreeMap<String, Ident>, &BTreeMap<String, Ident>),
    (ns, package): (&Ident, &Ident),
    tree: &mut Module,
    (modules, consumed): (&[Vec<Ident>], &foreign::Modules),
    (hook_shapes, defining): (&BTreeSet<String>, Option<&[Ident; 3]>),
) {
    for shape in shapes
        .iter()
        .filter(|shape| converted.contains_key(&shape.ident.to_string()))
    {
        let ident = &shape.ident;
        let name = ident.to_string();
        let depth = modules[shape.scope].len();
        let root = supers(depth);
        let interface = rust_ident(&wit::kebab(&placement[&name]));
        let export = quote! { #root exports::#ns::#package::#interface };
        let import = defining
            .filter(|_| hook_shapes.contains(&name))
            .map(|defining| import_module(depth, defining));
        let generated = &converted[&name];
        let sides = std::iter::once((export, false)).chain(import.map(|import| (import, true)));
        for (bindings, import) in sides {
            let at = Some(At {
                modules: consumed,
                scope: shape.scope,
                resources,
                module: &bindings,
                import,
            });
            let (outward, inward) = match &shape.kind {
                ShapeKind::Record(fields) => {
                    let outward = fields.iter().map(|(field, ty)| {
                        let named = rust_ident(&wit::kebab(field));
                        let value = convert(ty, quote! { value.#field }, Towards::Bindings, at);
                        quote! { #named: #value }
                    });
                    let inward = fields.iter().map(|(field, ty)| {
                        let named = rust_ident(&wit::kebab(field));
                        let value = convert(ty, quote! { value.#named }, Towards::Author, at);
                        quote! { #field: #value }
                    });
                    (
                        quote! { Self { #(#outward),* } },
                        quote! { Self { #(#inward),* } },
                    )
                }
                ShapeKind::Variant(cases) => {
                    let outward = cases.iter().map(|(case, payload)| {
                        let named = camel_ident(&wit::kebab(case));
                        match payload {
                            Payload::None => quote! { #ident::#case => Self::#named },
                            Payload::Unnamed(ty) => {
                                let item = convert(ty, quote! { item }, Towards::Bindings, at);
                                quote! { #ident::#case(item) => Self::#named(#item) }
                            }
                            Payload::Named(fields) => {
                                let record = type_ident(&wit::case_record(ident, case));
                                let names = fields.iter().map(|(field, _)| field);
                                let values = fields.iter().map(|(field, ty)| {
                                    let named = rust_ident(&wit::kebab(field));
                                    let value =
                                        convert(ty, quote! { #field }, Towards::Bindings, at);
                                    quote! { #named: #value }
                                });
                                quote! {
                                    #ident::#case { #(#names),* } => Self::#named(#bindings::#record { #(#values),* })
                                }
                            }
                        }
                    });
                    let inward = cases.iter().map(|(case, payload)| {
                        let named = camel_ident(&wit::kebab(case));
                        match payload {
                            Payload::None => {
                                quote! { #bindings::#generated::#named => Self::#case }
                            }
                            Payload::Unnamed(ty) => {
                                let item = convert(ty, quote! { item }, Towards::Author, at);
                                quote! { #bindings::#generated::#named(item) => Self::#case(#item) }
                            }
                            Payload::Named(fields) => {
                                let values = fields.iter().map(|(field, ty)| {
                                    let named = rust_ident(&wit::kebab(field));
                                    let value =
                                        convert(ty, quote! { item.#named }, Towards::Author, at);
                                    quote! { #field: #value }
                                });
                                quote! {
                                    #bindings::#generated::#named(item) => Self::#case { #(#values),* }
                                }
                            }
                        }
                    });
                    (
                        quote! { match value { #(#outward,)* } },
                        quote! { match value { #(#inward,)* } },
                    )
                }
                ShapeKind::Enum(cases) => {
                    let outward = cases.iter().map(|case| {
                        let named = camel_ident(&wit::kebab(case));
                        quote! { #ident::#case => Self::#named }
                    });
                    let inward = cases.iter().map(|case| {
                        let named = camel_ident(&wit::kebab(case));
                        quote! { #bindings::#generated::#named => Self::#case }
                    });
                    (
                        quote! { match value { #(#outward,)* } },
                        quote! { match value { #(#inward,)* } },
                    )
                }
            };
            tree.at(shape.scope).generated.push(quote! {
                impl ::core::convert::From<#ident> for #bindings::#generated {
                    fn from(value: #ident) -> Self { #outward }
                }
                impl ::core::convert::From<#bindings::#generated> for #ident {
                    fn from(value: #bindings::#generated) -> Self { #inward }
                }
            });
        }
    }
}

/// The name of the trait through which a newtype of the module crosses the bindings, declared in
/// the Orb module.
const NEWTYPE: &str = "__TembraNewtype";

/// The name of the trait through which a component reads a newtype from its field, declared in
/// the Orb module.
const FROM_FIELD: &str = "__TembraFromField";

/// The trait through which a newtype crosses to the import side of a derived hook's bindings,
/// which the Orb module declares beside `NEWTYPE`.
const IMPORT_NEWTYPE: &str = "__TembraImportNewtype";

/// The trait through which a newtype of the module crosses the bindings, as its field, and the one
/// through which a component reads it from its field: private to the Orb module, so its
/// sub-modules implement them and every bridge names them, and no other crate sees them. A module
/// whose services a `#[create]` builds has no bridge to call them.
///
/// A component reads a newtype through `From`, which refuses no value: it has no error channel for
/// an argument refused before its method runs, only a trap, which would retire its revision for
/// every caller until a reload. The second trait holds every newtype that `From` makes of its
/// field, and its diagnostic names one that only a refusing `TryFrom` makes, which a component
/// build then refuses: such a newtype is native only.
fn newtype_trait() -> proc_macro2::TokenStream {
    let name = format_ident!("{NEWTYPE}");
    let import_name = format_ident!("{IMPORT_NEWTYPE}");
    let from_field = format_ident!("{FROM_FIELD}");
    quote! {
        /// How a newtype of the module crosses the bindings: as the bindings' type of its field.
        #[allow(dead_code)]
        trait #name: ::core::marker::Sized {
            /// The bindings' type of the field.
            type Bindings;

            /// The newtype of the field's value: through `From` of the field's type in a
            /// component, through `TryFrom` in a host build, whose bridges never run.
            fn __tembra_from_bindings(value: Self::Bindings) -> Self;

            /// The field's value, as the bindings' type.
            fn __tembra_into_bindings(self) -> Self::Bindings;
        }

        /// How a newtype of the module crosses to the import side of a derived hook's bindings, whose
        /// copy of a type the hooks reach is another Rust type than the export's: as the import's
        /// type of its field.
        #[allow(dead_code)]
        trait #import_name: ::core::marker::Sized {
            /// The import's type of the field.
            type Import;

            /// The newtype of the field's value, as `NEWTYPE` reads it.
            fn __tembra_from_import(value: Self::Import) -> Self;

            /// The field's value, as the import's type.
            fn __tembra_into_import(self) -> Self::Import;
        }

        /// A newtype a component reads from its field: one `From` makes of every value of it.
        #[diagnostic::on_unimplemented(
            message = "`{Self}` is native only: a component reads it from its field through `From<{Field}>`, which `{Self}` does not implement",
            label = "a newtype a component takes, which only `TryFrom` makes",
            note = "a component has no error channel for an argument refused before its method runs, only a trap, which would retire its revision for every caller until a reload: implement `From<{Field}>` for `{Self}`, or take it natively only, where a value its `TryFrom` refuses fails that call alone"
        )]
        #[allow(dead_code)]
        trait #from_field<Field>: ::core::marker::Sized {
            /// The newtype of the field's value.
            fn __tembra_from_field(field: Field) -> Self;
        }

        impl<Field, Newtype: ::core::convert::From<Field>> #from_field<Field> for Newtype {
            fn __tembra_from_field(field: Field) -> Self {
                ::core::convert::From::from(field)
            }
        }
    }
}

/// The conversions of a newtype between the author's struct and the bindings' type of its field,
/// as its impl of the trait `NEWTYPE`, in its own module, where the field and its type are named
/// as the author wrote them: `module` is the bindings' module of an interface that names the
/// newtype, which names every type of the module the field holds. The bindings' value is read
/// into the field's type, then into the newtype: in a component through `From`, by the trait
/// `FROM_FIELD`, whose diagnostic, at the newtype, names one that implements no `From`; in a host
/// build, whose bridges never run, through `TryFrom`, which a `From` implements too. The field is
/// written back as it is. On the import `side` of a derived hook, `module` is that import, and the
/// impl is of the trait `IMPORT_NEWTYPE`, the same way.
fn newtype_conversions(
    newtype: &Newtype,
    module: &proc_macro2::TokenStream,
    side: Side<'_>,
    modules: &foreign::Modules,
) -> proc_macro2::TokenStream {
    let Newtype {
        ident,
        inner,
        scope,
    } = newtype;
    // A newtype holds no resource: the analysis refuses one.
    let resources = BTreeMap::new();
    let at = Some(At {
        modules,
        scope: *scope,
        resources: &resources,
        module,
        import: matches!(side, Side::Import { .. }),
    });
    let (bindings, newtype_trait, field, from, into) = match side {
        Side::Export { converted } => (
            with_handles(inner, *scope, converted, module, modules),
            modules.newtype_trait(*scope),
            format_ident!("Bindings"),
            format_ident!("__tembra_from_bindings"),
            format_ident!("__tembra_into_bindings"),
        ),
        Side::Import { hook_shapes } => (
            respell(inner, hook_shapes, module, (modules, *scope)),
            modules.import_newtype_trait(*scope),
            format_ident!("Import"),
            format_ident!("__tembra_from_import"),
            format_ident!("__tembra_into_import"),
        ),
    };
    let read = convert(inner, quote! { value }, Towards::Author, at);
    let written = convert(inner, quote! { self.0 }, Towards::Bindings, at);
    let root = supers(modules.depth(*scope));
    let from_field = format_ident!("{FROM_FIELD}", span = ident.span());
    let component = quote_spanned! { ident.span() =>
        <Self as #root #from_field<#inner>>::__tembra_from_field(#read)
    };
    quote! {
        impl #newtype_trait for #ident {
            type #field = #bindings;

            fn #from(value: Self::#field) -> Self {
                #[cfg(target_arch = "wasm32")]
                {
                    #component
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    match <Self as ::core::convert::TryFrom<#inner>>::try_from(#read) {
                        ::core::result::Result::Ok(value) => value,
                        ::core::result::Result::Err(error) => ::core::panic!("{}", error),
                    }
                }
            }

            fn #into(self) -> Self::#field {
                #written
            }
        }
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
    let Fields::Named(_) = &item.fields else {
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
                if !tembra_contracts::valid_version(version) {
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
        service,
        version,
        validator: None,
        identity: String::new(),
        scope: 0,
    })
}

/// `tokens` with every token at `span`, so rustc reports an error about them there.
fn respanned(tokens: proc_macro2::TokenStream, span: Span) -> proc_macro2::TokenStream {
    tokens
        .into_iter()
        .map(|mut token| {
            if let proc_macro2::TokenTree::Group(group) = &token {
                let mut inner =
                    proc_macro2::Group::new(group.delimiter(), respanned(group.stream(), span));
                inner.set_span(span);
                token = proc_macro2::TokenTree::Group(inner);
            }
            token.set_span(span);
            token
        })
        .collect()
}

/// The Rust side of the states: the kernel's `state-init` callback answering one declaration per
/// owning service, the accessors on each state type, and the JSON helpers they share, with the
/// kernel's `read` and `update` of a component, and the native side's in their place where the
/// facade, `facade` the name the crate depends on it under, links the runtime, which the accessors
/// call in both.
fn state_code(
    states: &[State],
    modules: &[Vec<Ident>],
    facade: &Ident,
) -> proc_macro2::TokenStream {
    if states.is_empty() {
        return quote! {};
    }
    let native = native::state_access(facade);
    // Every state is validated: the value a reload hands the new instance must at least read as
    // the state's type, or the first `read` would trap. The initial value is held to the same
    // rule before the kernel keeps it.
    let declarations = states.iter().map(|state| {
        let identity = LitStr::new(&state.identity, Span::call_site());
        let path = from_root(&modules[state.scope], &state.ident);
        let name = LitStr::new(&state.ident.to_string(), Span::call_site());
        let version = state.version;
        quote! {
            #identity => ::core::option::Option::Some(exports::tembra::guest::state_init::Declaration {
                initial: __tembra_state::initial(&<#path as ::core::default::Default>::default(), #name),
                version: #version,
                validated: true,
            }),
        }
    });
    // On reload the kernel hands the new instance the value it inherits: the new instance reads
    // it as its state's type, refusing a value of another shape, and runs the state's own rule,
    // if it has one.
    let validators = states.iter().map(|state| {
        let identity = LitStr::new(&state.identity, Span::call_site());
        let path = from_root(&modules[state.scope], &state.ident);
        let name = LitStr::new(&state.ident.to_string(), Span::call_site());
        let accepted = match &state.validator {
            Some(validator) => quote! { ::core::result::Result::Ok(state) => state.#validator(), },
            None => quote! { ::core::result::Result::Ok(_) => ::core::result::Result::Ok(()), },
        };
        quote! {
            #identity => match ::serde_json::from_str::<#path>(&value) {
                #accepted
                ::core::result::Result::Err(error) => ::core::result::Result::Err(
                    __tembra_state::format!("the retained state is not a `{}`: {error}", #name),
                ),
            },
        }
    });
    let accessors = states.iter().map(|state| {
        let path = from_root(&modules[state.scope], &state.ident);
        let service = state.service.as_ref().expect("resolved owner").to_string();
        let name = LitStr::new(&state.ident.to_string(), Span::call_site());
        let read_doc = format!(
            " The state of the `{service}` object running the current call, read from the kernel."
        );
        // A value whose JSON would not read back as the state, a NaN float for one, is never sent
        // to the kernel: a state with a rule answers the refusal like the rule's own, any other
        // traps; the kernel keeps the previous value either way.
        let update = if let Some(validator) = &state.validator {
            let doc = format!(
                " Change the state of the `{service}` object running the current call, validate it and store it in the kernel; returns the stored value, or the refusal of the validator or of a value JSON cannot hold, such as a NaN float, in which case the kernel keeps the previous value."
            );
            quote! {
                #[doc = #doc]
                pub fn update(
                    change: impl ::core::ops::FnOnce(&mut Self),
                ) -> ::core::result::Result<Self, __tembra_state::String> {
                    let mut value = Self::read();
                    change(&mut value);
                    value.#validator()?;
                    __tembra_state::update(&value, #name)
                }
            }
        } else {
            let doc = format!(
                " Change the state of the `{service}` object running the current call and store it in the kernel; returns the stored value. A value JSON cannot hold, such as a NaN float, traps instead, and the kernel keeps the previous value."
            );
            quote! {
                #[doc = #doc]
                pub fn update(change: impl ::core::ops::FnOnce(&mut Self)) -> Self {
                    let mut value = Self::read();
                    change(&mut value);
                    match __tembra_state::update(&value, #name) {
                        ::core::result::Result::Ok(stored) => stored,
                        ::core::result::Result::Err(refusal) => ::core::panic!("{}", refusal),
                    }
                }
            }
        };
        // The state's `Default`, its initial value, and serde's traits, which keep it as JSON, are
        // the author's: a state without one fails here, with rustc's error on the state's name.
        let span = state.ident.span();
        let named = respanned(path.clone(), span);
        let assertion = quote_spanned! {span=>
            const _: () = {
                fn state<T>()
                where
                    T: ::core::default::Default
                        + ::serde::Serialize
                        + ::serde::de::DeserializeOwned,
                {
                }
                let _ = state::<#named>;
            };
        };
        quote! {
            #assertion
            impl #path {
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
            ) -> ::core::option::Option<exports::tembra::guest::state_init::Declaration> {
                match service.as_str() {
                    #(#declarations)*
                    _ => ::core::option::Option::None,
                }
            }
            fn validate(
                service: __tembra_state::String,
                value: __tembra_state::String,
            ) -> ::core::result::Result<(), __tembra_state::String> {
                match service.as_str() {
                    #(#validators)*
                    _ => ::core::result::Result::Ok(()),
                }
            }
        }
        #(#accessors)*
        /// The kernel keeps a state as JSON; serde maps it to the state type and back.
        #[doc(hidden)]
        pub mod __tembra_state {
            extern crate alloc;
            pub use alloc::{format, string::String};

            // A component reaches the kernel through the bindings' import; a native host gets
            // `read` and `update` of its own, below, over the state of each method's object.
            ::#facade::__guest! {
                pub fn read<T: ::serde::de::DeserializeOwned>() -> T {
                    parse(&super::tembra::guest::state::read())
                }

                /// Store a value and return the stored one, or the refusal of a value whose JSON
                /// would not read back, which never reaches the kernel.
                pub fn update<T: ::serde::Serialize + ::serde::de::DeserializeOwned>(
                    value: &T,
                    name: &str,
                ) -> Result<T, String> {
                    let json = storable(value, name)?;
                    Ok(parse(&super::tembra::guest::state::update(&json)))
                }
            }

            #native

            /// The JSON of an initial value for the declaration, which has no refusal to answer
            /// with: a value whose JSON would not read back traps the object's creation.
            pub fn initial<T: ::serde::Serialize + ::serde::de::DeserializeOwned>(
                value: &T,
                name: &str,
            ) -> String {
                match storable(value, name) {
                    Ok(json) => json,
                    Err(refusal) => panic!("{}", refusal),
                }
            }

            /// The JSON of a state, once it reads back as the state's type. serde_json writes a
            /// NaN or infinite float as `null`, which reads back as no float: the kernel would
            /// keep a value that every later `read`, and the validation of a reload, refuses.
            fn storable<T: ::serde::Serialize + ::serde::de::DeserializeOwned>(
                value: &T,
                name: &str,
            ) -> Result<String, String> {
                let json = ::serde_json::to_string(value).map_err(|error| {
                    format!("the `{name}` state does not serialize to JSON, so it cannot be stored: {error}")
                })?;
                match ::serde_json::from_str::<T>(&json) {
                    Ok(_) => Ok(json),
                    Err(error) => Err(format!(
                        "the `{name}` state does not read back from its JSON, so it cannot be stored: {error}; JSON has no NaN or infinite float, which serde_json writes as `null`"
                    )),
                }
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
                if !tembra_contracts::valid_version(version) {
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

/// The Rust side of the resources: the kernel's `resources-init` callbacks listing, opening and
/// closing them, one slot in the component's memory per resource, and `with` on each type, which
/// reaches the component's slot in a component build and the Orb's resource natively, where the
/// native side declares each in the Orb's create and writes its own `with`, through the facade
/// named `facade`.
fn resource_code(
    resources: &[Resource],
    modules: &[Vec<Ident>],
    facade: &Ident,
) -> proc_macro2::TokenStream {
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
                name: ::core::convert::From::from(#name),
                version: #version,
            }
        }
    });
    let openers = resources.iter().map(|resource| {
        let name = wit::kebab(&resource.ident);
        let path = from_root(&modules[resource.scope], &resource.ident);
        let open = resource
            .open
            .as_ref()
            .expect("a resource has its #[open] function");
        let slot = slot(resource);
        quote! { #name => { #slot.open(#path::#open()); 1 } }
    });
    let closers = resources.iter().map(|resource| {
        let name = wit::kebab(&resource.ident);
        let slot = slot(resource);
        match &resource.close {
            Some(close) => {
                quote! {
                    #name => {
                        if let ::core::option::Option::Some(value) = #slot.close() {
                            value.#close();
                        }
                    }
                }
            }
            None => quote! { #name => { #slot.close(); } },
        }
    });
    let slots = resources.iter().map(|resource| {
        let path = from_root(&modules[resource.scope], &resource.ident);
        let slot = slot(resource);
        let name = wit::kebab(&resource.ident);
        let doc = format!(
            " Use the resource `{name}` of this Orb's root, opened by the kernel at activation; the kernel confirms it is open, the value lives in this component."
        );
        // A native host gets `with` of its own, in `__tembra_resource`, over the Orb its method
        // runs for.
        quote! {
            static #slot: __tembra_resource::Slot<#path> = __tembra_resource::Slot::new();
            ::#facade::__guest! {
                impl #path {
                    #[doc = #doc]
                    pub fn with<R>(use_: impl ::core::ops::FnOnce(&mut Self) -> R) -> R {
                        let _handle = self::tembra::guest::resources::read(#name);
                        #slot.with(use_)
                    }
                }
            }
        }
    });
    let native = native::resource_access(resources, modules, facade);
    quote! {
        // The kernel lists the root's resources when it creates the root, then opens and closes
        // each of them in its own order through these callbacks.
        impl exports::tembra::guest::resources_init::Guest for __TembraExports {
            fn declare() -> __tembra_resource::Vec<exports::tembra::guest::resources_init::Declaration> {
                ::core::convert::From::from([#(#declarations),*])
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
            pub struct Slot<T>(::core::cell::RefCell<::core::option::Option<T>>);

            // SAFETY: a component runs one export at a time on one thread, so the slot is never
            // reached from two threads; the RefCell refuses a reentrant use at run time.
            unsafe impl<T> ::core::marker::Sync for Slot<T> {}

            impl<T> Slot<T> {
                pub const fn new() -> Self {
                    Self(::core::cell::RefCell::new(::core::option::Option::None))
                }

                pub fn open(&self, value: T) {
                    *self.0.borrow_mut() = ::core::option::Option::Some(value);
                }

                pub fn close(&self) -> ::core::option::Option<T> {
                    self.0.borrow_mut().take()
                }

                pub fn with<R>(&self, use_: impl ::core::ops::FnOnce(&mut T) -> R) -> R {
                    use_(self.0.borrow_mut().as_mut().expect("the resource is not open"))
                }
            }

            impl<T> ::core::default::Default for Slot<T> {
                fn default() -> Self {
                    Self::new()
                }
            }

            #native
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
    let hook = Identity::of(hook);
    [
        rust_ident(hook.namespace),
        package_module(hook.package),
        rust_ident(hook.interface.unwrap_or_default()),
    ]
}

/// The Rust keywords `wit-bindgen` escapes with a trailing `_` when it names a module, a
/// function, a parameter or a field after a WIT name.
const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
];

/// The identifier `wit-bindgen` generates for the WIT name of a namespace, an interface, a
/// function, a parameter or a field: its snake_case form, with `_` after a Rust keyword.
pub(crate) fn rust_ident(name: &str) -> Ident {
    if RUST_KEYWORDS.contains(&name) {
        format_ident!("{name}_")
    } else {
        format_ident!("{}", name.to_snake_case())
    }
}

/// The name `wit-bindgen` gives the type of a WIT record, enum, variant or resource handle: its
/// UpperCamelCase form, but `Guest_` for `guest`, a name the bindings keep for their traits.
pub(crate) fn type_ident(name: &str) -> Ident {
    if name == "guest" {
        format_ident!("Guest_")
    } else {
        camel_ident(name)
    }
}

/// The name `wit-bindgen` gives the case of a WIT enum or variant, and a resource in the names of
/// its traits, `Guest<Name>` and the associated type of `Guest`: its UpperCamelCase form.
pub(crate) fn camel_ident(name: &str) -> Ident {
    format_ident!("{}", name.to_upper_camel_case())
}

/// The module `wit-bindgen` generates for a WIT package name: its snake_case form, never escaped,
/// which is why an identity's package name is no Rust keyword.
pub(crate) fn package_module(name: &str) -> Ident {
    format_ident!("{}", name.to_snake_case())
}

/// The keywords of WIT, which an identity cannot use as a name without the `%` escape.
const WIT_KEYWORDS: &[&str] = &[
    "use",
    "type",
    "func",
    "u8",
    "u16",
    "u32",
    "u64",
    "s8",
    "s16",
    "s32",
    "s64",
    "f32",
    "f64",
    "char",
    "resource",
    "own",
    "borrow",
    "record",
    "flags",
    "variant",
    "enum",
    "bool",
    "string",
    "option",
    "result",
    "future",
    "stream",
    "error-context",
    "list",
    "map",
    "_",
    "as",
    "from",
    "static",
    "interface",
    "tuple",
    "world",
    "import",
    "export",
    "package",
    "constructor",
    "include",
    "with",
    "async",
];

/// Read a WIT identity written on an attribute: `namespace:name@version` for a package, or
/// `namespace:name/interface@version` for an interface. Each name is a WIT identifier,
/// kebab-case words of lowercase letters and digits, each word starting with a letter, that is
/// no WIT keyword; the version, when there is one, is a semantic version. The identity reaches
/// the WIT text and the generated module paths as written, so anything else is an error on the
/// literal; `role` names what the identity is, for that error.
fn wit_identity(literal: &LitStr, role: &str, interface: bool) -> syn::Result<String> {
    let text = literal.value();
    let form = if interface {
        "namespace:name/interface@version"
    } else {
        "namespace:name@version"
    };
    match identity_problem(&text, interface) {
        None => Ok(text),
        Some(problem) => Err(syn::Error::new(
            literal.span(),
            format!(
                "{role} is named {form}, such as `example:name{}@1.0.0`; `{text}` is not: {problem}",
                if interface { "/interface" } else { "" }
            ),
        )),
    }
}

/// A WIT identity in its parts: `namespace:package/interface@version` names an interface,
/// `namespace:package@version` a package, either with or without its version.
pub(crate) struct Identity<'a> {
    pub namespace: &'a str,
    pub package: &'a str,
    pub interface: Option<&'a str>,
    pub version: Option<&'a str>,
}

impl<'a> Identity<'a> {
    /// Split the identity of an interface, or of a package when not `interface`, into its parts,
    /// or say which separator it lacks.
    fn split(text: &'a str, interface: bool) -> Result<Self, &'static str> {
        let (name, version) = match text.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (text, None),
        };
        let (package, interface) = match (interface, name.split_once('/')) {
            (true, Some((package, interface))) => (package, Some(interface)),
            (true, None) => return Err("it names no interface after a `/`"),
            (false, _) => (name, None),
        };
        let Some((namespace, package)) = package.split_once(':') else {
            return Err("it has no `:` between the namespace and the package name");
        };
        Ok(Self {
            namespace,
            package,
            interface,
            version,
        })
    }

    /// The parts of an identity `wit_identity` has read, or the macro has built from one: an
    /// interface's when it has a `/`.
    pub(crate) fn of(text: &'a str) -> Self {
        Self::split(text, text.contains('/')).expect("an identity the macro has read")
    }
}

/// The identity as the WIT text writes it.
impl std::fmt::Display for Identity<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.namespace, self.package)?;
        if let Some(interface) = self.interface {
            write!(formatter, "/{interface}")?;
        }
        if let Some(version) = self.version {
            write!(formatter, "@{version}")?;
        }
        Ok(())
    }
}

/// What makes a text no WIT identity of the given kind, if anything.
fn identity_problem(text: &str, interface: bool) -> Option<String> {
    let Identity {
        namespace,
        package,
        interface,
        version,
    } = match Identity::split(text, interface) {
        Ok(identity) => identity,
        Err(problem) => return Some(problem.into()),
    };
    let names = [("namespace", namespace), ("package name", package)]
        .into_iter()
        .chain(interface.map(|interface| ("interface name", interface)));
    for (part, value) in names {
        if let Some(problem) = identifier_problem(value) {
            return Some(format!("the {part} `{value}` {problem}"));
        }
        // The bindings name a module after each name: they escape a Rust keyword in a namespace
        // or an interface name, but neither a package name nor the 2024 edition's `gen`.
        if value == "gen" || (part == "package name" && RUST_KEYWORDS.contains(&value)) {
            return Some(format!(
                "the {part} `{value}` is a Rust keyword, which the bindings cannot name a module after"
            ));
        }
    }
    match version {
        Some(version) if !semantic_version(version) => Some(format!(
            "the version `{version}` is not a semantic version, such as `1.0.0`"
        )),
        _ => None,
    }
}

/// What makes a name no WIT identifier, if anything.
fn identifier_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("is empty");
    }
    if WIT_KEYWORDS.contains(&name) {
        return Some("is a WIT keyword");
    }
    for word in name.split('-') {
        if word.is_empty() {
            return Some("has an empty word around a `-`");
        }
        if !word.starts_with(|first: char| first.is_ascii_lowercase()) {
            return Some("has a word that does not start with a lowercase letter");
        }
        if !word
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        {
            return Some("is not kebab-case words of lowercase letters and digits");
        }
    }
    None
}

/// Whether a text is a semantic version: `major.minor.patch`, numbers without leading zeros,
/// optionally followed by `-` and pre-release identifiers and by `+` and build identifiers.
fn semantic_version(text: &str) -> bool {
    let number = |part: &str| {
        !part.is_empty()
            && part.chars().all(|character| character.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    };
    let identifiers = |list: &str, pre_release: bool| {
        list.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
                && (!pre_release
                    || !part.chars().all(|character| character.is_ascii_digit())
                    || number(part))
        })
    };
    let (rest, build) = match text.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (text, None),
    };
    let (core, pre_release) = match rest.split_once('-') {
        Some((core, pre_release)) => (core, Some(pre_release)),
        None => (rest, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers.iter().all(|part| number(part))
        && pre_release.is_none_or(|list| identifiers(list, true))
        && build.is_none_or(|list| identifiers(list, false))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod async_tests;

#[cfg(test)]
mod std_tests;

#[cfg(test)]
mod wasi_tests;

#[cfg(test)]
mod keyed_tests;

#[cfg(test)]
mod foreign_tests;

#[cfg(test)]
mod metadata_tests;

#[cfg(test)]
mod cfg_tests;
