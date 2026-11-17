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
//! directly or through another type; a state alone does not. Such a type is replaced by the type
//! the bindings generate, which takes the author's derives and the helper attributes they read.
//! A struct of `bool` fields marked `#[flags]` becomes a `flags` on the same terms, but stays the
//! author's: the bindings take it as it is.
//! The rest of the module stays ordinary Rust: a state that stays out of the WIT, and the module
//! types its fields reach, get serde's derives.
//!
//! The macro derives the WIT contract from those items and generates what a WIT-first crate
//! writes by hand: the `generate!` invocation of the `wit-bindgen` the facade re-exports, with
//! the WIT inline, the `Guest` implementations forwarding to the author's functions and
//! `export!`. The module uses the standard library by default; `no_std` on the attribute adds
//! the freestanding Wasm prelude.
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
mod flags;
mod foreign;
mod keyed;
mod resource_params;
mod wasi;
mod wit;

use heck::{ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    Attribute, Fields, FnArg, Ident, Item, ItemFn, ItemMod, LitByteStr, LitInt, LitStr, Pat,
    ReturnType, Token, Type, Visibility, spanned::Spanned,
};
use tembra_contracts::{EmbeddedManifest, HookComposition, Lifetime, ServicePolicy};

/// Declare a compiled Orb from a module; the crate documentation lists the rules.
#[proc_macro_attribute]
pub fn orb(attribute: TokenStream, item: TokenStream) -> TokenStream {
    expansion(attribute.into(), item.into()).into()
}

/// The module's expansion, or the macro's error next to the runtime prelude the module would have
/// brought: without its allocator and panic handler, a `no_std` crate would report their absence
/// after the error that matters. A std crate has its own runtime and gets none.
fn expansion(
    attribute: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    // Read from the tokens, so that an attribute the macro refuses still says it.
    let freestanding = attribute
        .clone()
        .into_iter()
        .any(|token| matches!(&token, proc_macro2::TokenTree::Ident(ident) if ident == "no_std"));
    let expanded = options(attribute)
        .and_then(|options| Ok((options, syn::parse2::<ItemMod>(item)?)))
        .and_then(|(options, module)| expand(options, module));
    match expanded {
        Ok(tokens) => tokens,
        Err(error) => {
            let error = error.to_compile_error();
            let prelude = freestanding.then(prelude);
            quote! { #error #prelude }
        }
    }
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
/// need no Wasm target.
fn prelude() -> proc_macro2::TokenStream {
    quote! {
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static __TEMBRA_ALLOCATOR: ::dlmalloc::GlobalDlmalloc = ::dlmalloc::GlobalDlmalloc;

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __tembra_panic(_: &::core::panic::PanicInfo<'_>) -> ! {
            ::core::arch::wasm32::unreachable()
        }
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

/// A type declared in the module, with its WIT definition and the module types it refers to.
struct Shape {
    ident: Ident,
    wit: String,
    references: Vec<Ident>,
    /// How the type is built, for the conversions between the two sides of a derived hook.
    kind: ShapeKind,
    /// The module declaring the type, by scope; 0 is the Orb module.
    scope: usize,
    /// The author's attributes the generated type replacing this one takes.
    carried: Carried,
}

/// The author's attributes the generated type replacing a type takes, as written: the paths of
/// its derives, and the helper attributes they read, on the type and on each field or case, by
/// the member's WIT name.
#[derive(Default)]
struct Carried {
    derives: Vec<syn::Path>,
    helpers: Vec<Attribute>,
    members: Vec<(String, Vec<Attribute>)>,
}

impl Carried {
    fn is_empty(&self) -> bool {
        self.derives.is_empty()
            && self.helpers.is_empty()
            && self.members.iter().all(|(_, helpers)| helpers.is_empty())
    }
}

/// A struct with named fields or an enum of the module, before reachability decides whether it
/// enters the WIT: the item, its name, the module types its fields name, followed by reachability
/// and serde's derives, and its module. Only a candidate that enters the WIT is translated, so a
/// type no method mentions may hold what WIT has no spelling for.
struct Candidate {
    item: Item,
    ident: Ident,
    /// The module types the fields name, read once the scan knows the consumed interfaces, whose
    /// modules name none.
    references: Vec<Ident>,
    /// The module declaring the type, by scope; 0 is the Orb module.
    scope: usize,
    /// A struct marked `#[flags]`, a `flags` once it enters the WIT, which stays the author's.
    flags: bool,
}

impl Candidate {
    /// A struct with named fields, a record once it enters the WIT.
    fn record(item: syn::ItemStruct, scope: usize) -> Self {
        Self {
            ident: item.ident.clone(),
            item: Item::Struct(item),
            references: Vec::new(),
            scope,
            flags: false,
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
            flags: false,
        }
    }

    /// The type's WIT definition, once it enters the WIT, with what the macro cannot evaluate and
    /// what the generated type replacing it would drop refused first and what it takes of the
    /// author's attributes. A flags struct is not replaced and keeps its attributes.
    fn translate(&self) -> syn::Result<Shape> {
        unconditional(&self.item, self.flags)?;
        if let (true, Item::Struct(item)) = (self.flags, &self.item) {
            let mut shape = flags::shape(item)?;
            shape.scope = self.scope;
            return Ok(shape);
        }
        replaceable(&self.item)?;
        let mut shape = match &self.item {
            Item::Struct(item) => wit::record(item)?,
            Item::Enum(item) => wit::enumeration(item)?,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "a type entering the WIT is a struct with named fields or an enum",
                ));
            }
        };
        shape.scope = self.scope;
        shape.references = self.references.clone();
        shape.carried = carried(&self.item)?;
        Ok(shape)
    }
}

/// The construction of a shape: the named fields of a record, the cases of an enum, the cases of
/// a variant with their payload, the author's struct of a flags, which the bindings take as it is.
enum ShapeKind {
    Record(Vec<(Ident, Type)>),
    Enum(Vec<Ident>),
    Variant(Vec<(Ident, Option<Type>)>),
    Flags(Box<syn::ItemStruct>),
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

/// One exported function, with the WIT spelling of its parameter and result types.
struct Function {
    ident: Ident,
    asynchronous: bool,
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
    /// The named fields, each with the span of its type, where a missing `Default` is reported,
    /// and its `#[cfg(..)]` attributes, which the macro's `Default` writes on its initializer.
    fields: Vec<(Ident, Span, Vec<Attribute>)>,
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
    /// The struct derives `Default`, by name or under another name, `SmartDefault`: the derive
    /// stays on the author's struct when the state stays out of the WIT, and goes to the generated
    /// type replacing it when the state enters it.
    derives_default: bool,
    /// The author gives the state its `Default`, its initial value: by an `impl Default` in the
    /// Orb module, or by a derive. Otherwise the macro implements it field by field.
    authored_default: bool,
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
                let (outer, inner) = placed(&self.attrs);
                let vis = &self.vis;
                let unsafety = &self.unsafety;
                quote! { #(#outer)* #vis #unsafety mod #ident { #(#inner)* #body } }
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
    impls: Vec<(usize, syn::ItemImpl)>,
    /// Each `impl Default for ..` of the Orb module, with the module it is written in.
    default_impls: Vec<(usize, syn::ItemImpl)>,
    marked: Vec<Marked>,
    /// What each module imports, by scope.
    uses: Vec<Uses>,
    /// Every type each module declares, with its scope: a struct of any shape, an enum, a type
    /// alias or a union, candidate for the WIT or not, which tell the type an `impl` names apart
    /// from a namesake a `use` brings in.
    types: Vec<(usize, Ident)>,
    /// The same types but the aliases of a namesake (`aliases_namesake`), which stand for the
    /// type they name, as a `use` does: a name the module writes resolves to one of them before a
    /// resource of another module.
    own_types: Vec<(usize, Ident)>,
}

/// The names a module binds with `use` and `extern crate`, each with the path it stands for, and
/// the paths it imports by glob, `use path::*;`: what the macro follows to name a derive the
/// author writes in that module from another module, the bindings'. With the names of the items
/// the module declares, which the bindings' module does not reach.
#[derive(Default)]
struct Uses {
    names: BTreeMap<String, Named>,
    globs: Vec<Named>,
    items: BTreeSet<String>,
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
            Item::Type(item) => &item.ident,
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

/// Whether an item declared `depth` modules below the Orb module is visible to it: `pub` and
/// `pub(crate)` at any depth, `pub(super)` one level down. Every item of the Orb module itself is.
fn reaches_root(vis: &Visibility, depth: usize) -> bool {
    reach(vis) >= depth
}

/// The visibilities that make an item visible `levels` modules above the one declaring it, for the
/// errors: `pub(super)` reaches one level up only.
fn reaching(levels: usize) -> &'static str {
    if levels == 1 {
        "`pub` or `pub(super)`"
    } else {
        "`pub` or `pub(crate)`"
    }
}

/// Refuse an item of the module at `path` when a module of the path is hidden from the module at
/// `from`, which names the item through every one of them; `seen_from` says where each module of
/// the path is visible. `what` the item is and `why` that module names it, for the error.
fn check_seen(
    path: &[Ident],
    seen_from: &[usize],
    from: &[Ident],
    ident: &Ident,
    what: &str,
    why: &str,
) -> syn::Result<()> {
    let Some(position) = seen_from
        .iter()
        .position(|&depth| !from.starts_with(&path[..depth]))
    else {
        return Ok(());
    };
    // The hidden module, declared `position` modules below the Orb module, is visible to `from`
    // once it is visible in the deepest module above both.
    let shared = from
        .iter()
        .zip(path)
        .take_while(|(named, declared)| named == declared)
        .count();
    let viewer = if from.is_empty() {
        "the Orb module".to_owned()
    } else {
        format!("the module {}", describe(from))
    };
    Err(syn::Error::new(
        ident.span(),
        format!(
            "`{ident}` is {what} of a sub-module {viewer} cannot see: make the module `{}` visible to {viewer}, {}, {why}",
            path[position],
            reaching(position - shared)
        ),
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
            if let Some(ident) = declared_type {
                self.types.push((scope, ident.clone()));
                if !matches!(&item, Item::Type(alias) if aliases_namesake(alias)) {
                    self.own_types.push((scope, ident.clone()));
                }
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
                        )?;
                    }
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
                        unconditional_marked(&s.attrs, "hook", &s.ident, "runs it")?;
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
                        )?;
                        // The Orb module's exports take the struct as a type of theirs; the
                        // modules of its path are checked from its service's module, which names
                        // it through them, once the service is known.
                        if !reaches_root(&s.vis, depth) {
                            return Err(syn::Error::new(
                                s.ident.span(),
                                format!(
                                    "`{}` is a resource of a sub-module: make it visible to the Orb module, {}, whose services hand it out",
                                    s.ident,
                                    reaching(depth)
                                ),
                            ));
                        }
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
                        )?;
                        let why = "which opens and closes it";
                        if !reaches_root(&s.vis, depth) {
                            return Err(syn::Error::new(
                                s.ident.span(),
                                format!(
                                    "`{}` is a resource of a sub-module: make it visible to the Orb module, {}, {why}",
                                    s.ident,
                                    reaching(depth)
                                ),
                            ));
                        }
                        check_seen(
                            &self.modules[scope],
                            &self.seen_from[scope],
                            &[],
                            &s.ident,
                            "a resource",
                            why,
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
                        unconditional_marked(&s.attrs, "state", &s.ident, "builds and reads it")?;
                        let why = "which builds and reads it";
                        let fields_visible = match &s.fields {
                            Fields::Named(fields) => fields
                                .named
                                .iter()
                                .all(|field| reaches_root(&field.vis, depth)),
                            _ => true,
                        };
                        if !reaches_root(&s.vis, depth) || !fields_visible {
                            return Err(syn::Error::new(
                                s.ident.span(),
                                format!(
                                    "`{}` is a state of a sub-module: make the struct and its fields visible to the Orb module, {}, {why}",
                                    s.ident,
                                    reaching(depth)
                                ),
                            ));
                        }
                        check_seen(
                            &self.modules[scope],
                            &self.seen_from[scope],
                            &[],
                            &s.ident,
                            "a state",
                            why,
                        )?;
                        let mut state = state(&s, &attribute)?;
                        state.scope = scope;
                        self.states.push(state);
                        self.candidates.push(Candidate::record(s, scope));
                    } else if let Some(attribute) = take(&mut s.attrs, "service") {
                        unconditional_marked(&s.attrs, "service", &s.ident, "calls its functions")?;
                        if !matches!(s.fields, Fields::Unit) {
                            return Err(syn::Error::new(
                                attribute.span(),
                                "a service is a unit struct: its methods are the pub fn of its impl blocks",
                            ));
                        }
                        let (implements, policy) = service_attribute(&attribute)?;
                        self.services.push(Service {
                            ident: s.ident.clone(),
                            docs: wit::docs(&s.attrs)?,
                            functions: Vec::new(),
                            policy,
                            implements,
                            scope,
                        });
                        passthrough.push(Item::Struct(s));
                    } else if let Some(attribute) = take(&mut s.attrs, "flags") {
                        let candidate = flags::candidate(s, &attribute, scope, depth)?;
                        check_seen(
                            &self.modules[scope],
                            &self.seen_from[scope],
                            &[],
                            &candidate.ident,
                            "a flags",
                            "whose bindings name it",
                        )?;
                        self.candidates.push(candidate);
                    } else if matches!(s.fields, Fields::Named(_)) {
                        self.candidates.push(Candidate::record(s, scope));
                    } else {
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
                        // The Orb module calls a state's rule and a root resource's functions; a
                        // WIT resource's constructor is called from its own module's bridge.
                        if marker != Marker::Constructor && !reaches_root(&function.vis, depth) {
                            return Err(syn::Error::new(
                                span,
                                format!(
                                    "#[{}] on a function of a sub-module: make it visible to the Orb module, {}, which calls it",
                                    marker.name(),
                                    reaching(depth)
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
                        // The code the macro writes calls the marked function.
                        let names = format!("{self_ty}::{}", function.sig.ident);
                        unconditional_marked(&i.attrs, "`impl` block holding", &names, "calls it")?;
                        unconditional_marked(&function.attrs, "function", &names, "calls it")?;
                        unconditional_params(&function.sig, "function", &names)?;
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
                    // block. An `impl Default` may give a state its initial value.
                    let implements_default = i.trait_.as_ref().is_some_and(|(_, path, _)| {
                        path.segments
                            .last()
                            .is_some_and(|segment| segment.ident == "Default")
                    });
                    if implements_default {
                        self.default_impls.push((scope, i.clone()));
                    }
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
                        unconditional_marked(
                            &function.attrs,
                            "contribution",
                            &function.sig.ident,
                            "calls it",
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
        .as_ref()
        .ok_or_else(|| syn::Error::new(module.span(), "missing package = \"ns:name@1.0.0\""))?;
    let package = wit_identity(package, "the package", false)?;
    let imports = options
        .imports
        .iter()
        .map(|import| wit_identity(import, "an imported interface", true))
        .collect::<syn::Result<Vec<String>>>()?;
    let Identity {
        namespace,
        package: name,
        ..
    } = Identity::of(&package);
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
    // The names the Orb module's own items take beside the modules it re-exports.
    let own_names = foreign::own_names(&items);

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
        impls: Vec::new(),
        default_impls: Vec::new(),
        marked: Vec::new(),
        uses: vec![Uses::default()],
        types: Vec::new(),
        own_types: Vec::new(),
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
        seen_from,
        mut candidates,
        mut services,
        contributions,
        declared,
        mut states,
        mut resources,
        mut wit_resources,
        impls,
        default_impls,
        marked,
        uses,
        types: declared_types,
        own_types,
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
                let names = format!("{}::{}", service.ident, function.sig.ident);
                unconditional_marked(&item.attrs, "`impl` block holding", &names, "calls it")?;
                unconditional_marked(&function.attrs, "function", &names, "calls it")?;
                unconditional_params(&function.sig, "function", &names)?;
                let read = method(function)?;
                // An implemented interface's names are fixed by its WIT, checked with it below.
                if service.implements.is_none()
                    && let Some(member) = reserved_member(&wit::kebab(&read.ident))
                {
                    return Err(syn::Error::new(
                        read.ident.span(),
                        format!(
                            "`{}::{}` is a method named `{member}`, a member every reference to a service object has ({}): the kernel refuses such a method, so rename it",
                            service.ident,
                            read.ident,
                            RESERVED_MEMBERS.join(", ")
                        ),
                    ));
                }
                service.functions.push(read);
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
            unconditional_marked(&block.attrs, "`impl` block holding", &names, "calls it")?;
            unconditional_marked(&function.attrs, "function", &names, "calls it")?;
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
        check_seen(
            &modules[resource.scope],
            &seen_from[resource.scope],
            &modules[owner.scope],
            &resource.ident,
            "a resource",
            &format!("whose service `{}` hands it out", owner.ident),
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
    let consumed_modules = foreign::Modules::new(
        options.context,
        &options.imports,
        &keyed_imports,
        &services,
        &contributions,
        &declared,
        &modules,
        &own_names,
    )?;
    for candidate in &mut candidates {
        candidate.references = consumed_modules.item_references(&candidate.item, candidate.scope);
    }
    // Consumed packages, parsed once for the expansion: the callback of every hook, contributed or
    // declared, and every interface implemented or imported are read from their WIT packages.
    let parsed = wit::Parsed::new(options.wit.as_deref().map(wit::absolute).transpose()?);
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
        (Some(dir), false) => wit::consumed(dir, implemented.iter().chain(imported))?,
        (None, false) => {
            return Err(syn::Error::new(
                Span::call_site(),
                "implementing or importing an interface needs wit = \"<dir>\", the directory with its WIT package",
            ));
        }
    };
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
    let callbacks = match (parsed.dir(), consumed_hooks.is_empty()) {
        (_, true) => Default::default(),
        (Some(dir), false) => wit::callbacks(dir, consumed_hooks.iter().copied())?,
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

    // A state whose `Default` the author implements by hand or derives starts from the author's
    // value: a derive stays on the author's struct or goes to the generated type replacing it.
    // The macro writes no `Default` for a state whose impls it reads, before the compiler
    // evaluates `#[cfg(..)]`, so those give it one in every configuration, with a derive under
    // `#[cfg_attr(..)]` too (`uncovered`).
    let authored: Vec<Vec<(usize, syn::ItemImpl)>> = states
        .iter()
        .map(|state| {
            default_impls
                .iter()
                .filter(|(scope, block)| {
                    names_state(
                        &block.self_ty,
                        *scope,
                        state,
                        &states,
                        &modules,
                        &uses,
                        &declared_types,
                    )
                })
                .cloned()
                .collect()
        })
        .collect();
    for (state, hands) in states.iter_mut().zip(authored) {
        state.authored_default = !hands.is_empty() || state.derives_default;
        if hands.is_empty() || state.derives_default {
            continue;
        }
        let attrs = candidates
            .iter()
            .find_map(|candidate| match &candidate.item {
                Item::Struct(item)
                    if candidate.scope == state.scope && item.ident == state.ident =>
                {
                    Some(item.attrs.as_slice())
                }
                _ => None,
            })
            .unwrap_or_default();
        let derived = attrs
            .iter()
            .filter(|attribute| {
                let applied = applied(&attribute.meta);
                !applied.is_empty() && derives(&[attrs, &applied].concat(), "Default")
            })
            .filter_map(|attribute| applying(&attribute.meta))
            .collect();
        if let Some(refusal) = uncovered(
            &hands,
            derived,
            "Default",
            &state.ident,
            &mut tree,
            &modules,
        ) {
            return Err(refusal);
        }
    }

    // Only the types a service method, a resource or a hook uses, directly or through another
    // type, enter the WIT; the others go back to their modules as ordinary Rust. A state alone
    // makes no type enter: a state that stays out of the WIT is serde's to serialize, and so is
    // every type of the module its fields reach that stays out too, so each of them gets the
    // serde derives the author did not write. The types that enter the WIT are generated with
    // the derives already.
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
    // The `Default` the macro writes for a state kept out of the WIT names each field, before the
    // compiler evaluates `#[cfg_attr(..)]`, and carries only a plain `#[cfg(..)]` onto a field's
    // initializer: a field under a `#[cfg_attr(..)]` wrapping one would be named in every
    // configuration, and a derive of `Default` under one would add a second `Default` where it
    // applies. A state entering the WIT has had the attribute refused already.
    for state in states.iter().filter(|state| !state.authored_default) {
        let Some(item) = unreached
            .iter()
            .find_map(|candidate| match &candidate.item {
                Item::Struct(item)
                    if candidate.scope == state.scope && item.ident == state.ident =>
                {
                    Some(item)
                }
                _ => None,
            })
        else {
            continue;
        };
        // A `#[cfg_attr(..)]` whose attributes, applied, make the struct derive `Default`: without
        // them it derives none, or the state's `Default` would be the author's.
        let derived = item.attrs.iter().find(|attribute| {
            let applied = applied(&attribute.meta);
            !applied.is_empty() && derives(&[item.attrs.as_slice(), &applied].concat(), "Default")
        });
        if let Some(attribute) = derived {
            let state = &state.ident;
            return Err(syn::Error::new(
                attribute.span(),
                format!(
                    "`#[cfg_attr(..)]` deriving `Default` on the state `{state}`: the macro writes the `Default` of the state `{state}` before the compiler evaluates the attribute, so `{state}` would implement `Default` twice where the attribute applies; derive `Default` without `#[cfg_attr(..)]`, or implement it"
                ),
            ));
        }
        for field in &item.fields {
            let condition = field.attrs.iter().find(|attribute| {
                attribute.path().is_ident("cfg_attr") && conditional(&attribute.meta)
            });
            if let (Some(name), Some(attribute)) = (&field.ident, condition) {
                let state = &state.ident;
                return Err(syn::Error::new(
                    attribute.span(),
                    format!(
                        "`#[cfg_attr(..)]` wrapping `#[cfg(..)]` on the field `{state}::{name}`: the macro writes the `Default` of the state `{state}` before the compiler evaluates the attribute, so that `Default` would name `{state}::{name}` in every configuration; write the condition as a plain `#[cfg(..)]`, which the macro carries onto the field's initializer, or derive or implement `Default`"
                    ),
                ));
            }
        }
    }
    // Where each type is defined, computed once for the WIT and for the re-exports below.
    let placement = wit::placement(
        &shapes,
        &services,
        &wit_resources,
        &declared,
        &consumed_modules,
    );
    // The view of each interface imported by key, derived from the interface's WIT: its module
    // re-exports the types the view uses, which a derived signature names.
    let views = keyed::views(&options.keyed, parsed.dir(), &world, &interface_names)?;
    // What each derived interface `use`s from the consumed interfaces: the types its functions and
    // the types it defines name, and the handles its functions take, once each is checked where
    // it is named, a state's fields included.
    let foreign = foreign::foreign(foreign::Reach {
        modules: &consumed_modules,
        parsed: &parsed,
        views: &views,
        services: &services,
        resources: &wit_resources,
        contributions: &contributions,
        declared: &declared,
        shapes: &shapes,
        placement: &placement,
        states: &states,
        unreached: &unreached,
    })?;
    let serialized = serialized(&unreached, &shapes, &states);
    for (index, candidate) in unreached.into_iter().enumerate() {
        let Candidate {
            mut item,
            ident,
            scope,
            ..
        } = candidate;
        let attrs = match &mut item {
            Item::Struct(item) => Some(&mut item.attrs),
            Item::Enum(item) => Some(&mut item.attrs),
            _ => None,
        };
        if let Some(attrs) = attrs
            && serialized.contains(&index)
        {
            // The traits of `serde` the type has already: a derive of one, under any name, or an
            // impl the Orb's modules write by hand. A derive the macro cannot follow to a crate
            // is `serde`'s by its name, as one a crate-root `#[macro_use]` brings.
            let derived = serde_derived(attrs, scope, &uses, &modules, &ident, true);
            let by_hand: Vec<(String, (usize, syn::ItemImpl))> =
                flags::impls_of(&mut tree, &modules, &uses, &declared_types, &ident, scope)
                    .into_iter()
                    .filter_map(|(written, block)| {
                        let (_, path, _) = block.trait_.as_ref()?;
                        let name = serde_trait(path, written, &uses, &modules, &ident, true)?;
                        Some((name, (written, block)))
                    })
                    .collect();
            let missing: Vec<(&str, proc_macro2::TokenStream)> = [
                ("Serialize", quote! { ::serde::Serialize }),
                ("Deserialize", quote! { ::serde::Deserialize }),
            ]
            .into_iter()
            .filter(|(name, _)| {
                !derived.iter().any(|has| has == name)
                    && !by_hand.iter().any(|(has, _)| has == name)
            })
            .collect();
            // The macro reads those impls before the compiler evaluates `#[cfg(..)]`, so those it
            // leaves a state's trait to give it in every configuration, with a derive under
            // `#[cfg_attr(..)]` too (`uncovered`): the code it writes serializes the state. What
            // a type the state reaches needs depends on the `#[serde(..)]` of the fields reaching
            // it, which the macro leaves to the compiler.
            let state = states
                .iter()
                .any(|state| state.ident == ident && state.scope == scope);
            for name in ["Serialize", "Deserialize"] {
                if !state || derived.iter().any(|has| has == name) {
                    continue;
                }
                let hands: Vec<(usize, syn::ItemImpl)> = by_hand
                    .iter()
                    .filter(|(has, _)| has == name)
                    .map(|(_, hand)| hand.clone())
                    .collect();
                if hands.is_empty() {
                    continue;
                }
                let under = attrs
                    .iter()
                    .filter(|attribute| {
                        let applied = applied(&attribute.meta);
                        serde_derived(&applied, scope, &uses, &modules, &ident, true)
                            .iter()
                            .any(|has| has == name)
                    })
                    .filter_map(|attribute| applying(&attribute.meta))
                    .collect();
                if let Some(refusal) = uncovered(&hands, under, name, &ident, &mut tree, &modules) {
                    return Err(refusal);
                }
            }
            // The macro reads the type's derives before the compiler evaluates `#[cfg_attr(..)]`:
            // one of those it writes, derived under such an attribute as well, would be a second
            // where the attribute applies.
            let twice = attrs.iter().find_map(|attribute| {
                let name = conditional_serde(&attribute.meta, scope, &uses, &modules, &ident)
                    .into_iter()
                    .find(|name| missing.iter().any(|(missing, _)| name == missing))?;
                Some((attribute, name))
            });
            if let Some((attribute, name)) = twice {
                let (owner, reached) = if state {
                    (format!("the state `{ident}`"), "")
                } else {
                    (format!("`{ident}`"), ", which a state's fields reach,")
                };
                return Err(syn::Error::new(
                    attribute.span(),
                    format!(
                        "`#[cfg_attr(..)]` deriving `{name}` on {owner}: the macro derives `serde`'s `{name}` for {owner}{reached} before the compiler evaluates the attribute, so `{ident}` would implement `{name}` twice where the attribute applies; derive `{name}` without `#[cfg_attr(..)]`, or remove the derive, which the macro writes in every configuration"
                    ),
                ));
            }
            if !missing.is_empty() {
                let paths = missing.iter().map(|(_, path)| path);
                attrs.push(syn::parse_quote!(#[derive(#(#paths),*)]));
            }
        }
        tree.at(&modules[scope]).items.push(item);
    }
    // A flags struct that enters the WIT stays the author's, with what the bindings call on it.
    flags::check_hooks(&shapes, &declared, &consumed_modules)?;
    flags::check_modules(&shapes, &mut tree, &modules)?;
    let mut implemented_elsewhere = flags::emit(
        &shapes,
        &mut tree,
        &modules,
        &impls,
        &uses,
        &declared_types,
        !states.is_empty(),
    )?;
    // What each other type that enters the WIT implements besides what the bindings implement for
    // it and what its derives implement, which the types holding one may derive: the traits the
    // Orb's modules implement for it by hand, which apply to the generated type replacing it, and
    // the `Default` the macro writes for a state the author gives none. Those impls are kept, by
    // type, for what the bindings implement themselves (`clashing`).
    let mut hand_impls: BTreeMap<String, Vec<(usize, syn::ItemImpl)>> = BTreeMap::new();
    for shape in &shapes {
        if matches!(shape.kind, ShapeKind::Flags(_)) {
            continue;
        }
        let hands = flags::implemented_by_hand(
            &mut tree,
            &modules,
            &uses,
            &declared_types,
            &shape.ident,
            shape.scope,
        );
        let mut implemented = Elsewhere::by_hand(&hands, &shape.ident, &mut tree, &modules);
        let macro_default = states.iter().any(|state| {
            state.ident == shape.ident && state.scope == shape.scope && !state.authored_default
        });
        if macro_default {
            implemented.traits.insert("Default".to_owned());
        }
        implemented_elsewhere.insert(shape.ident.to_string(), implemented);
        hand_impls.insert(
            shape.ident.to_string(),
            hands.into_values().flatten().collect(),
        );
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
    // A derived function takes and returns only its own interface's resources, owned, itself or
    // through the records and variants it reaches, and a derived hook's input holds no resource. An
    // implemented interface's functions take what its WIT says, in the consumed package's types.
    resource_params::check_reached(
        &services,
        &wit_resources,
        &declared,
        &shapes,
        &own_types,
        &consumed_modules,
    )?;
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
                        RESERVED_MEMBERS.join(", ")
                    ),
                ));
            }
            if implementing(&service.functions, &function.name)?.is_none() {
                return Err(syn::Error::new(
                    service.ident.span(),
                    format!(
                        "`{}` implements {identity} but has no `pub fn {}`",
                        service.ident,
                        rust_ident(&function.name)
                    ),
                ));
            }
        }
        for function in &interface.functions {
            let own = service
                .functions
                .iter()
                .find(|own| wit::kebab(&own.ident) == function.name)
                .expect("implemented function checked above");
            check_async(own, function)?;
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
            let own = implementation.methods.iter().chain(&implementation.statics);
            for function in resource.methods.iter().chain(&resource.statics) {
                // The WIT fixes whether each one is asynchronous.
                let authored = implementing(own.clone(), &function.name)?;
                if let Some(authored) = authored {
                    check_async(authored, function)?;
                }
                if authored.is_none() {
                    return Err(syn::Error::new(
                        implementation.ident.span(),
                        format!(
                            "`{ident}` implements the resource `{}` of {identity} but has no `pub fn {}`",
                            resource.name,
                            rust_ident(&function.name)
                        ),
                    ));
                }
            }
        }
    }
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
    let (wit, used) = wit::render(
        &package,
        &world,
        wit::Items {
            shapes: &shapes,
            placement: &placement,
            services: &services,
            resources: &wit_resources,
            declared: &declared,
            foreign: &foreign,
            modules: &consumed_modules,
        },
        consumed_hooks.iter().copied(),
        &imports,
        &views,
        wit::Kernel {
            state: !states.is_empty(),
            resources: !resources.is_empty(),
            context: options.context,
        },
    )?;

    // Generate the Rust side: the bindings from the inline WIT, the author's types replaced by
    // the generated ones, the bridges to the author's functions, each in the module of the item
    // it serves, and the optional freestanding runtime prelude.
    let module_ident = module.ident;
    let vis = module.vis;
    let (outer_attrs, inner_attrs) = placed(&module.attrs);
    let ns_ident = rust_ident(namespace);
    let name_ident = package_module(name);
    // The generated types carry the author's names and public fields, so the author's functions
    // and the generated traits speak about one type: each is re-exported in the module that
    // declared it, from the interface `placement` defines it in, under the author's name where
    // the bindings name it otherwise, `HttpRequest as HTTPRequest`. A flags struct is the
    // author's own, which the bindings take.
    let hook_shapes = wit::hook_shapes(&shapes, &declared, &consumed_modules);
    let interface_module = |ident: &Ident| rust_ident(&wit::kebab(ident));
    let mut grouped: Vec<((Ident, usize), Vec<proc_macro2::TokenStream>)> = Vec::new();
    for shape in shapes
        .iter()
        .filter(|shape| !matches!(shape.kind, ShapeKind::Flags(_)))
    {
        let interface = interface_module(&placement[&shape.ident.to_string()]);
        let key = (interface, shape.scope);
        let ident = &shape.ident;
        let generated = type_ident(&wit::kebab(ident));
        let imported = if generated == *ident {
            quote! { #ident }
        } else {
            quote! { #generated as #ident }
        };
        match grouped.iter_mut().find(|(known, _)| *known == key) {
            Some((_, imports)) => imports.push(imported),
            None => grouped.push((key, vec![imported])),
        }
    }
    for ((interface, scope), imports) in &grouped {
        let root = supers(modules[*scope].len());
        tree.at(&modules[*scope]).generated.push(quote! {
            pub use #root exports::#ns_ident::#name_ident::#interface::{#(#imports),*};
        });
    }
    // The generated types name each derive from its crate, so the author's item was the only one
    // to name it through the imports of its module: each derive's path, as written, is imported
    // again there, under no name, and those imports stay used.
    for shape in shapes
        .iter()
        .filter(|shape| !shape.carried.derives.is_empty())
    {
        let paths = &shape.carried.derives;
        tree.at(&modules[shape.scope]).generated.push(quote! {
            #(#[allow(unused_imports)] use #paths as _;)*
        });
    }
    // The bindings' handle type of each WIT resource, by the author's name of its struct.
    let handle_types: BTreeMap<String, Ident> = wit_resources
        .iter()
        .map(|resource| {
            let handle = type_ident(&wit::kebab(&resource.ident));
            (resource.ident.to_string(), handle)
        })
        .collect();
    // A derived service's bridge forwards each function, named as the bindings name it, to the
    // author's; a resource a parameter holds is taken back from the bindings' handle type, one the
    // result holds is wrapped into it, and the resources of the service's interface are the
    // author's structs, each bridged in its own module.
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
            let generated = rust_ident(&wit::kebab(ident));
            let (params, names) = resource_params::bridged(
                &function.params,
                service.scope,
                &module,
                &handle_types,
                &consumed_modules,
            );
            let (asynchronous, await_) = async_bridge(function.asynchronous);
            let call = quote! { #target::#ident(#(#names),*) #await_ };
            let (result, body) = derived_result(
                &function.result,
                service.scope,
                &module,
                &handle_types,
                &consumed_modules,
                call,
            );
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
                    &handle_types,
                    &consumed_modules,
                ));
        }
    }
    // An implemented interface's bridge takes its signatures from the WIT: the functions forward
    // to the author's of the same WIT names, and each resource of the interface is the author's
    // struct of its WIT name.
    for service in services
        .iter()
        .filter(|service| service.implements.is_some())
    {
        let identity = service.implements.as_deref().unwrap_or_default();
        let (bridge, implementations) = implemented_service(
            service,
            identity,
            &consumed[identity],
            &wit_resources,
            &modules,
        )?;
        tree.at(&modules[service.scope]).generated.push(bridge);
        for (scope, implementation) in implementations {
            tree.at(&modules[scope]).generated.push(implementation);
        }
    }
    // Implemented and imported interfaces are re-exported under their own names, like hooks, in
    // the Orb module; a sub-module reaches them through `super::`.
    let consumed_modules = services
        .iter()
        .filter_map(|service| service.implements.as_deref())
        .map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            quote! { pub use exports::#ns::#package::#interface; }
        })
        .chain(imports.iter().map(|identity| {
            let [ns, package, interface] = hook_path(identity);
            quote! { pub use self::#ns::#package::#interface; }
        }))
        .chain(keyed::reexports(&package, &views));
    // The kernel's context interface is re-exported as `context`, so the author writes
    // `context::principal()`.
    let context_module = options
        .context
        .then(|| quote! { pub use self::tembra::guest::context; });
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
        let callback = rust_ident(&callbacks[&contribution.hook].name);
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
    // The manifest travels with the component, like the WIT: the contributions with their
    // priorities, the declarations with their composition, the services that chose a
    // lifetime or a linger, the interfaces imported by key, and the directory of the consumed
    // packages, as written on the attribute: `cargo tembra build` nests the whole packages from
    // there, where the component's own types would keep only the functions it calls.
    let manifest = EmbeddedManifest {
        contributes: hooks.clone(),
        declares: declarations
            .iter()
            .map(|(hook, declaration)| {
                let composition = match declaration.composition {
                    Composition::Series => HookComposition::Series,
                    Composition::Waterfall => HookComposition::Waterfall,
                    Composition::Bail => HookComposition::Bail,
                };
                (hook.clone(), composition)
            })
            .collect(),
        services: services
            .iter()
            .filter_map(|service| {
                let policy = service.policy.as_ref()?;
                let lifetime = if policy.host {
                    Lifetime::Host
                } else {
                    Lifetime::Referenced
                };
                let policy = ServicePolicy {
                    lifetime,
                    linger_ms: policy.linger_ms,
                };
                Some((service.identity(&package), policy))
            })
            .collect(),
        references: keyed::manifest(&package, &views),
        wit: options.wit.clone(),
    };
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
    // may not name: the alias goes in every module that holds such a signature.
    // The resources of an implemented interface are bridged in their own modules.
    let implementers: Vec<&Ident> = services
        .iter()
        .filter(|service| service.implements.is_some())
        .map(|service| &service.ident)
        .collect();
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
        .collect();
    for scope in &alloc_scopes {
        tree.at(&modules[*scope])
            .generated
            .push(quote! { extern crate alloc as __tembra_alloc; });
    }
    // The hooks: the export the contract requires and `run`, in the declaring module; the
    // conversions between the two sides of the derived hooks' types, once, in the Orb module.
    let holding = holding_lists(&shapes);
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
            &holding,
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
    let body = tree.tokens();
    let wit_literal = LitStr::new(&wit, Span::call_site());
    let wit_bytes = LitByteStr::new(wit.as_bytes(), Span::call_site());
    let wit_len = proc_macro2::Literal::usize_unsuffixed(wit.len());
    let world_literal = LitStr::new(&wit::escaped(&world), Span::call_site());
    // The bindings read the author's WIT and, nested in it, the kernel package a state, a
    // resource or the context uses and the WASI packages the imports need, unless the consumed
    // directory holds them already, as a contract with its dependencies does, every copy of a
    // bundled package there checked whether the Orb uses it or not; the hook packages come from
    // the consumed directory. Foreign interfaces need `generate_all`.
    let kernel = !states.is_empty() || !resources.is_empty() || options.context;
    let copies = wit::bundled_copies(&parsed, kernel)?;
    let (standard, nested) = wasi::packages(&parsed, &options.imports, &copies)?;
    let guest = kernel && wit::guest_package_needed(&parsed, &copies)?;
    if let Some(literal) = &options.package {
        wit::claim_package(literal, &parsed, guest, &nested)?;
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
    // What the bindings make of the WIT, read when a generated type carries more than they give
    // it: the serde derives of a state, or the author's derives and the attributes they read, or
    // when the Orb's modules implement a trait by hand for a type that enters the WIT. The
    // bindings add the inline WIT to the consumed packages, so they read them last, and the
    // bundled packages it nests as the expansion parsed them.
    let serde = !states.is_empty();
    let read = serde
        || shapes.iter().any(|shape| !shape.carried.is_empty())
        || hand_impls.values().any(|hands| !hands.is_empty())
        || implemented_elsewhere
            .values()
            .any(|besides| !besides.conditional.is_empty());
    let bindings = if read {
        wit::bindings(parsed, &wit, guest, &nested, serde)
    } else {
        wit::Bindings::default()
    };
    // A trait the bindings implement for a generated type, which the Orb's modules implement for
    // it by hand too, would be implemented twice; and the bindings of a type holding a flags
    // struct need what the macro left to impls under `#[cfg(..)]`.
    for shape in &shapes {
        let (Some(hands), Some(generated)) = (
            hand_impls.get(&shape.ident.to_string()),
            bindings.types.get(&selector(shape, &placement, &package)),
        ) else {
            continue;
        };
        clashing(shape, generated, hands, &uses, &modules)?;
    }
    flags::check_held(
        &shapes,
        &bindings,
        &placement,
        &package,
        &implemented_elsewhere,
    )?;
    // A state's record that a method mentions is a generated type, which the kernel's JSON needs
    // to serialize; the derives reach every generated type that can carry them. A record or a
    // variant holding a resource handle or a consumed package's flags type cannot: the bindings
    // give those no serde implementation, so the derives leave it alone. The bindings leave a
    // type alone by its bare name, so one that can carry them but shares the name of such a record
    // or variant takes them by its full name, with the attributes below. The module's own flags
    // are the author's structs, which a state gives serde's derives.
    let derives = serde.then(|| {
        let ignored = &bindings.unserializable;
        let ignore =
            (!ignored.is_empty()).then(|| quote! { additional_derives_ignore: [#(#ignored),*], });
        quote! {
            additional_derives: [::serde::Serialize, ::serde::Deserialize],
            #ignore
        }
    });
    // The generated types replacing the author's take the author's derives, less what the
    // bindings implement themselves, and the attributes those derives read.
    let (types, members) = carried_attributes(
        &shapes,
        Layout {
            placement: &placement,
            used: &used,
        },
        &package,
        &bindings,
        &implemented_elsewhere,
        &uses,
        &modules,
    )?;
    let type_attributes =
        (!types.is_empty()).then(|| quote! { additional_type_attributes: { #(#types),* }, });
    let member_attributes =
        (!members.is_empty()).then(|| quote! { additional_member_attributes: { #(#members),* }, });
    // The bindings take each flags type as the author's struct.
    let remapped = flags::remapped(&shapes, &placement, &package, &modules);
    let with = (!remapped.is_empty()).then(|| quote! { with: { #(#remapped),* }, });
    // The generated code follows the derive rules and the flags lowering of one wit-bindgen
    // version, so the facade picks it, not the author's crate: the bindings come from its
    // re-export, and their runtime and the bitflags they name are reached there as well, under the
    // name the crate depends on the facade under.
    let facade = options
        .facade
        .clone()
        .unwrap_or_else(|| Ident::new("tembra", Span::call_site()));
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
            #type_attributes
            #member_attributes
            #with
        });
    };
    let runtime_prelude = options.no_std.then(prelude);
    Ok(quote! {
        #(#outer_attrs)*
        #vis mod #module_ident {
            #(#inner_attrs)*

            /// The WIT derived from this module.
            pub const WIT: &str = #wit_literal;

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

            struct __TembraExports;
            #conversions
            #state_code
            #resource_code
            export!(__TembraExports);
        }
    })
}

/// The members every reference to a service object has, which the kernel refuses as method names
/// when it installs an Orb; `tembra-core` checks the same list.
const RESERVED_MEMBERS: [&str; 6] = ["then", "release", "state", "ready", "status", "opaque"];

/// The reserved member a WIT function would take on a reference, if any: the kernel compares the
/// lowerCamelCase spelling of the official JavaScript bindings.
fn reserved_member(wit_name: &str) -> Option<String> {
    let member = wit_name.to_lower_camel_case();
    RESERVED_MEMBERS
        .contains(&member.as_str())
        .then_some(member)
}

/// The traits an item's `#[derive(..)]` attributes implement, by name: the trait of each derive's
/// last name, or those it implements under another name (`implemented_by`).
fn derived(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
        .filter_map(|attribute| {
            attribute
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, Token![,]>::parse_terminated,
                )
                .ok()
        })
        .flatten()
        .filter_map(|path| Some(implemented_by(&path.segments.last()?.ident, attrs)))
        .flatten()
        .collect()
}

/// Whether an item's `#[derive(..)]` attributes implement a trait (`derived`).
fn derives(attrs: &[Attribute], name: &str) -> bool {
    derived(attrs).iter().any(|implemented| implemented == name)
}

/// The type-level helper attribute through which a derive names the traits it implements, for
/// the derives that implement traits under another name that way: `#[educe(..)]` for `educe`'s
/// `Educe` and `#[derivative(..)]` for `derivative`'s `Derivative`.
fn trait_helper(derive: &Ident) -> Option<&'static str> {
    match derive.to_string().as_str() {
        "Educe" => Some("educe"),
        "Derivative" => Some("derivative"),
        _ => None,
    }
}

/// The helper attribute on a member through which a derive that implements a trait under another
/// name gives the member its own value or leaves it out, where the macro knows it: `#[default(..)]`
/// for `smart_default`'s `SmartDefault`, and the helper of `trait_helper` for its derives,
/// `#[educe(Hash(ignore))]`.
fn member_helper(derive: &Ident) -> Option<&'static str> {
    if derive == "SmartDefault" {
        return Some("default");
    }
    trait_helper(derive)
}

/// The traits a derive implements, by name: the trait of the derive's last name, `Hash` for
/// `ordering::Hash`, except for the derives that implement a trait under another name:
/// `smart_default`'s `SmartDefault` implements `Default`, and a derive of `trait_helper`
/// implements the traits its type-level helper attribute lists, one per entry, with or without
/// options, `Debug` and `Clone` for `#[educe(Debug(name(Spot)), Clone)]`. `attrs` are the
/// attributes of the type the derive is written on, where that helper attribute sits.
fn implemented_by(derive: &Ident, attrs: &[Attribute]) -> Vec<String> {
    if derive == "SmartDefault" {
        return vec!["Default".to_owned()];
    }
    let Some(helper) = trait_helper(derive) else {
        return vec![derive.to_string()];
    };
    attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident(helper))
        .filter_map(|attribute| {
            attribute
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated,
                )
                .ok()
        })
        .flatten()
        .filter_map(|meta| Some(meta.path().segments.last()?.ident.to_string()))
        .collect()
}

/// Whether the type of an `impl Default for ..`, written in the module `scope`, is `state`: its
/// path, followed from that module through the module's `use`s, `self::`, `super::` and
/// sub-modules (`type_module`), ends at the state; a path ending at another type of the Orb module
/// is not the state, and one leaving the Orb module neither. A name the macro cannot follow, one a
/// glob import brings in, is the state when no other state has its name.
fn names_state(
    ty: &Type,
    scope: usize,
    state: &State,
    states: &[State],
    modules: &[Vec<Ident>],
    uses: &[Uses],
    declared: &[(usize, Ident)],
) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    if path.qself.is_some() {
        return false;
    }
    let Some((module, name)) = type_module(&path.path, scope, uses, modules) else {
        return false;
    };
    if name != state.ident {
        return false;
    }
    if module == state.scope {
        return true;
    }
    let elsewhere = declared
        .iter()
        .any(|(scope, ident)| *scope == module && *ident == name);
    !elsewhere
        && states
            .iter()
            .filter(|other| other.ident == state.ident)
            .count()
            == 1
}

/// The module, as a path from the Orb module, in which a type path written in the module `from`
/// names its type: `self::` stays, `super::` goes up, any other segment goes down a sub-module.
/// `None` for a path that leaves the Orb module. `foreign::Modules` reads a path so, by its text
/// alone; `type_module` follows the Orb's modules' `use`s as well.
fn module_of(path: &syn::Path, from: &[Ident]) -> Option<Vec<Ident>> {
    if path.leading_colon.is_some() {
        return None;
    }
    let mut module = from.to_vec();
    let segments: Vec<&Ident> = path.segments.iter().map(|segment| &segment.ident).collect();
    let (_, prefix) = segments.split_last()?;
    for segment in prefix {
        if *segment == "self" {
            continue;
        }
        if *segment == "super" {
            module.pop()?;
        } else if *segment == "crate" || *segment == "Self" {
            return None;
        } else {
            module.push((*segment).clone());
        }
    }
    Some(module)
}

/// The module, by scope, in which a type path written in the module `scope` names its type, with
/// the type's name there. The path's prefix is followed as a glob import's is (`glob_module`):
/// through the `use` binding its first name, `self::`, `super::` and the sub-modules of the Orb.
/// A single name a `use` of the module binds, `Access` after `use super::rights::Access;`, stands
/// for the path it imports, and so does one the module reached re-exports. `None` for a path that
/// leaves the Orb module, through a crate, `crate::` or `Self`.
fn type_module(
    path: &syn::Path,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
) -> Option<(usize, Ident)> {
    if path.leading_colon.is_some() {
        return None;
    }
    let mut scope = scope;
    let mut segments: Vec<Ident> = path
        .segments
        .iter()
        .map(|segment| segment.ident.clone())
        .collect();
    // Each step consumes the path's prefix or replaces the name by what a `use` binds it to;
    // declarations binding one another in a circle would never end.
    for _ in 0..64 {
        let (name, prefix) = segments.split_last()?;
        let (name, prefix) = (name.clone(), prefix.to_vec());
        if !prefix.is_empty() {
            let prefix = Named {
                global: false,
                segments: prefix,
            };
            scope = glob_module(&prefix, scope, uses, modules)?;
        }
        match uses[scope].names.get(&name.to_string()) {
            // `use name;` brings the crate `name` into scope, and a path from the crates leaves.
            Some(bound)
                if bound.global || (bound.segments.len() == 1 && bound.segments[0] == name) =>
            {
                return None;
            }
            Some(bound) => segments = bound.segments.clone(),
            None => return Some((scope, name)),
        }
    }
    None
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
            implements = Some(wit_identity(&identity, "an implemented interface", true)?);
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
        docs: wit::docs(&function.attrs)?,
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
    modules: &foreign::Modules,
    call: proc_macro2::TokenStream,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let Some((ty, _)) = result else {
        return (quote! {}, call);
    };
    match handles_of(ty, scope, resources, modules) {
        Some(handles) => {
            let spelled = with_handles(ty, scope, resources, module, modules);
            (quote! { -> #spelled }, wrap(&handles, call, module))
        }
        None => (quote! { -> #ty }, call),
    }
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
/// resource of the module is the handle type there, everything else keeps the author's spelling,
/// a name a consumed interface's module reaches included.
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
    modules: &foreign::Modules,
) -> proc_macro2::TokenStream {
    let ident = &resource.ident;
    let scope = resource.scope;
    let guest = format_ident!("Guest{}", camel_ident(&wit::kebab(ident)));
    let constructor = resource.constructor.as_ref().map(|constructor| {
        let (params, names) =
            resource_params::bridged(&constructor.params, scope, module, resources, modules);
        let function = &constructor.ident;
        quote! { fn new(#(#params),*) -> Self { #ident::#function(#(#names),*) } }
    });
    let methods = resource.methods.iter().map(|function| {
        let name = &function.ident;
        let generated = rust_ident(&wit::kebab(name));
        let (params, names) =
            resource_params::bridged(&function.params, scope, module, resources, modules);
        let (asynchronous, await_) = async_bridge(function.asynchronous);
        let call = quote! { #ident::#name(self, #(#names),*) #await_ };
        let (result, body) =
            derived_result(&function.result, scope, module, resources, modules, call);
        quote! { #asynchronous fn #generated(&self, #(#params),*) #result { #body } }
    });
    let statics = resource.statics.iter().map(|function| {
        let name = &function.ident;
        let generated = rust_ident(&wit::kebab(name));
        let (params, names) =
            resource_params::bridged(&function.params, scope, module, resources, modules);
        let (asynchronous, await_) = async_bridge(function.asynchronous);
        let call = quote! { #ident::#name(#(#names),*) #await_ };
        let (result, body) =
            derived_result(&function.result, scope, module, resources, modules, call);
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
        let own = &own.ident;
        let Signature {
            names,
            params,
            result,
            handles,
        } = signature(&function.params, function.result.as_ref(), depth)?;
        let (asynchronous, await_) = async_bridge(function.asynchronous);
        let body = wrapped(
            handles,
            &module,
            quote! { #target::#own(#(#names),*) #await_ },
        );
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
        implementations.push((
            implementation.scope,
            quote! {
                impl #local_module::#guest for #ident {
                    #constructor
                    #(#members)*
                }
            },
        ));
    }
    let bridge = quote! {
        impl #module::Guest for #root __TembraExports {
            #(#owned)*
            #(#functions)*
        }
    };
    Ok((bridge, implementations))
}

/// The type a name written in the module `scope` names among `named`, the types of the Orb that
/// take the name, each with the module declaring it: that module's own, else the only one in the
/// Orb; none of two in other modules, which the name does not tell apart.
fn own_or_only<T>(named: impl IntoIterator<Item = (usize, T)>, scope: usize) -> Option<T> {
    let mut named: Vec<(usize, T)> = named.into_iter().collect();
    let index = match named.iter().position(|(module, _)| *module == scope) {
        Some(own) => own,
        None if named.len() == 1 => 0,
        None => return None,
    };
    Some(named.swap_remove(index).1)
}

/// Keep, in module order, the candidate types that a derived service's method, a derived
/// resource's function or a derived hook's input reaches; the others, with their shapes, go back
/// to their modules as ordinary Rust. The resources of an implemented interface reach nothing:
/// their functions speak the consumed package's types, whatever the module's own are called. A
/// name resolves by `own_or_only`: to the type of the module naming it, a resource included, else
/// to the only resource of that name, else to the only type of that name in the Orb; two of other
/// modules are ambiguous, a type the module does not declare is an error, and a resource is a type
/// of its own. A type of any shape the module declares, one of `types`, keeps the name from the
/// resources and the types of other modules wherever the module writes it, in a signature or in a
/// field, as in Rust. What a consumed interface's module names, a type or a handle, is no type of
/// the module.
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
        let mut idents = Vec::new();
        modules.references(ty, scope, &mut idents);
        pending.extend(idents.into_iter().map(|ident| (ident, scope)));
    };
    for function in resource_params::derived(services, resources) {
        for (_, ty, _) in function.params {
            mention(ty, function.scope, &mut pending);
        }
        if let Some(ty) = function.result {
            mention(ty, function.scope, &mut pending);
        }
    }
    for (input, scope) in hook_inputs {
        mention(input, *scope, &mut pending);
    }
    let mut kept = BTreeSet::new();
    while let Some((ident, scope)) = pending.pop() {
        let indexes = declared
            .get(&ident.to_string())
            .map_or(&[][..], Vec::as_slice);
        let declares = types
            .iter()
            .any(|(module, name)| *module == scope && *name == ident);
        if resource_params::resource_named(&ident, scope, resources, declares).is_some() {
            continue;
        }
        // Resources take the name here only when several of other modules do: the name tells
        // them apart no more than several candidates.
        let shared = !declares && resources.iter().any(|resource| resource.ident == ident);
        let named = indexes
            .iter()
            .map(|&index| (candidates[index].scope, index));
        // A type the module declares keeps the name from the candidates of other modules too, so
        // one that is no candidate is an error, as a type the module does not declare is.
        let index = match own_or_only(named, scope)
            .filter(|&index| candidates[index].scope == scope || !declares)
        {
            Some(index) if !shared => index,
            None if declares || (indexes.is_empty() && !shared) => {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "`{ident}` is used by a service method or a hook but is not a struct or enum of this module"
                    ),
                ));
            }
            _ => {
                return Err(syn::Error::new(
                    ident.span(),
                    format!(
                        "`{ident}` is declared in several modules of the Orb; a module uses its own `{ident}`, or the types take different names"
                    ),
                ));
            }
        };
        if kept.insert(index) {
            let candidate = &candidates[index];
            pending.extend(
                candidate
                    .references
                    .iter()
                    .cloned()
                    .map(|reference| (reference, candidate.scope)),
            );
        }
    }
    // Only the kept candidates are translated: what they hold must have a WIT spelling.
    let mut shapes = Vec::new();
    for (index, candidate) in candidates.into_iter().enumerate() {
        if kept.contains(&index) {
            shapes.push(candidate.translate()?);
        } else {
            unreached.push(candidate);
        }
    }
    Ok(shapes)
}

/// Refuse what a type that enters the WIT carries but the generated type replacing it would drop
/// without a word: an explicit discriminant, which a WIT enum has no room for, a `#[serde(..)]`
/// attribute, which the generated type cannot carry, so a state's JSON would silently change
/// shape, and a field or a case the bindings name otherwise, from its WIT name, so the author's
/// name would be gone.
fn replaceable(item: &Item) -> syn::Result<()> {
    let members: Vec<(&Ident, Ident, &str)> = match item {
        Item::Struct(item) => item
            .fields
            .iter()
            .filter_map(|field| field.ident.as_ref())
            .map(|field| (field, rust_ident(&wit::kebab(field)), "field"))
            .collect(),
        Item::Enum(item) => item
            .variants
            .iter()
            .map(|variant| {
                let case = camel_ident(&wit::kebab(&variant.ident));
                (&variant.ident, case, "case")
            })
            .collect(),
        _ => Vec::new(),
    };
    let owner = match item {
        Item::Struct(item) => &item.ident,
        Item::Enum(item) => &item.ident,
        _ => return Ok(()),
    };
    for (member, generated, kind) in members {
        let name = wit::kebab(member);
        if let Some(problem) = wit::name_problem(&name) {
            return Err(syn::Error::new(
                member.span(),
                format!(
                    "the {kind} `{owner}::{member}` has no WIT name: its kebab-case form `{name}` {problem}"
                ),
            ));
        }
        if *member != generated {
            return Err(syn::Error::new(
                member.span(),
                format!(
                    "the {kind} `{owner}::{member}` enters the WIT as `{name}`, which the generated type replacing `{owner}` names `{generated}`: name the {kind} `{generated}`"
                ),
            ));
        }
    }
    let (ident, attrs, fields): (&Ident, Vec<&Attribute>, Vec<&syn::Field>) = match item {
        Item::Struct(item) => (
            &item.ident,
            item.attrs.iter().collect(),
            item.fields.iter().collect(),
        ),
        Item::Enum(item) => {
            for variant in &item.variants {
                if let Some((_, discriminant)) = &variant.discriminant {
                    return Err(syn::Error::new(
                        discriminant.span(),
                        format!(
                            "`{0}::{1} = {2}`: `{0}` enters the WIT, whose enums have no explicit discriminants, and the generated type replaces this one; remove the discriminant, or keep `{0}` out of the WIT",
                            item.ident,
                            variant.ident,
                            quote!(#discriminant)
                        ),
                    ));
                }
            }
            let attrs = item
                .attrs
                .iter()
                .chain(item.variants.iter().flat_map(|variant| &variant.attrs))
                .collect();
            let fields = item
                .variants
                .iter()
                .flat_map(|variant| &variant.fields)
                .collect();
            (&item.ident, attrs, fields)
        }
        _ => return Ok(()),
    };
    let attributes: Vec<&Attribute> = attrs
        .into_iter()
        .chain(fields.into_iter().flat_map(|field| &field.attrs))
        .collect();
    let named = |name: &str| {
        attributes
            .iter()
            .find(|attribute| attribute.path().is_ident(name))
    };
    if let Some(attribute) = named("serde") {
        return Err(syn::Error::new(
            attribute.span(),
            format!(
                "`{ident}` enters the WIT and the generated type replaces it, which cannot carry `#[serde(..)]`: a state would change its JSON without a word; remove the attribute, or keep `{ident}` out of the WIT"
            ),
        ));
    }
    Ok(())
}

/// Refuse what the macro cannot evaluate in a type that enters the WIT, as it reads the type before
/// the compiler evaluates `#[cfg(..)]` and `#[cfg_attr(..)]`: the type under `#[cfg(..)]`, which
/// the WIT would hold in every configuration, as the generated type replacing a record, an enum or
/// a variant would be, while a flags struct stays the author's, gone where the attribute is off,
/// and the bindings name it; a field, a case or a case's payload under `#[cfg(..)]`, which the WIT
/// would hold in every configuration; and a `#[cfg_attr(..)]` on the type or a member hiding what
/// the macro reads, which would come and go on the generated type replacing the type, or beside the
/// derives the macro adds to a flags struct.
fn unconditional(item: &Item, flags: bool) -> syn::Result<()> {
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
            _ if flags => "flags",
            _ => "record",
        };
        // A flags struct stays the author's, which the bindings name; the generated type replaces
        // any other type.
        let named = if flags {
            "the bindings would name the struct".to_owned()
        } else {
            format!("the generated type would replace `{owner}`")
        };
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
    let hiding = attrs
        .iter()
        .chain(members.iter().flat_map(|(_, _, attrs)| attrs.iter()))
        .find(|attribute| hidden(&attribute.meta));
    if let Some(attribute) = hiding {
        let message = if flags {
            format!(
                "`{owner}` enters the WIT as a flags, which the macro completes before the compiler evaluates `#[cfg_attr(..)]`: write the attribute without it, or keep `{owner}` out of the WIT"
            )
        } else {
            format!(
                "`{owner}` enters the WIT and the generated type replaces it, which takes the author's derives and the attributes they read as written, never under `#[cfg_attr(..)]`: write them without it, or keep `{owner}` out of the WIT"
            )
        };
        return Err(syn::Error::new(attribute.span(), message));
    }
    Ok(())
}

/// Refuse a `#[cfg(..)]`, written plainly or under `#[cfg_attr(..)]`, among the attributes of an
/// item the macro writes code for: a struct it marks, a function it calls, or the `impl` block or
/// the sub-module holding one. `what` the item is, the struct or function the code `names` and
/// what that code `does`, for the error: the macro reads the item before the compiler evaluates
/// the attribute, and that code names the struct or the function in every configuration, also
/// where the attribute is off and the compiler drops the item.
fn unconditional_marked(
    attrs: &[Attribute],
    what: &str,
    names: impl std::fmt::Display,
    does: &str,
) -> syn::Result<()> {
    let Some(attribute) = attrs.iter().find(|attribute| conditional(&attribute.meta)) else {
        return Ok(());
    };
    let written = written_condition(attribute);
    Err(syn::Error::new(
        attribute.span(),
        format!(
            "{written} on the {what} `{names}`: the macro writes the code that {does} before the compiler evaluates the attribute, so that code would name `{names}` in every configuration; remove the attribute"
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

/// The attributes a `#[cfg_attr(..)]` applies where its configuration holds, directly or through
/// another `#[cfg_attr(..)]`: none for another attribute, or for one that does not parse.
fn applied(meta: &syn::Meta) -> Vec<Attribute> {
    if !meta.path().is_ident("cfg_attr") {
        return Vec::new();
    }
    let wrapped = meta.require_list().and_then(|list| {
        list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated)
    });
    // The first is the configuration predicate.
    wrapped
        .into_iter()
        .flatten()
        .skip(1)
        .flat_map(|meta| {
            if meta.path().is_ident("cfg_attr") {
                applied(&meta)
            } else {
                vec![syn::parse_quote!(#[#meta])]
            }
        })
        .collect()
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

/// A configuration predicate, as `#[cfg(..)]` and `#[cfg_attr(..)]` write it.
enum Predicate {
    /// A configuration option, set or not, by its tokens: `test`, `feature = "audit"`.
    Config(String),
    /// `all(..)`: each holds, as an empty one does.
    All(Vec<Predicate>),
    /// `any(..)`: one holds.
    Any(Vec<Predicate>),
    /// `not(..)`.
    Not(Box<Predicate>),
}

impl Predicate {
    /// The predicate `meta` writes: `all`, `any` or `not` of others, or an option.
    fn of(meta: &syn::Meta) -> Self {
        if let syn::Meta::List(list) = meta
            && let Ok(parts) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated,
            )
        {
            let mut parts: Vec<Predicate> = parts.iter().map(Predicate::of).collect();
            if list.path.is_ident("all") {
                return Predicate::All(parts);
            }
            if list.path.is_ident("any") {
                return Predicate::Any(parts);
            }
            if list.path.is_ident("not") && parts.len() == 1 {
                return Predicate::Not(Box::new(parts.remove(0)));
            }
        }
        Predicate::Config(quote!(#meta).to_string())
    }

    /// Whether it holds in the configuration that sets the options `set` and no other.
    fn holds(&self, set: &BTreeSet<&str>) -> bool {
        match self {
            Predicate::Config(option) => set.contains(option.as_str()),
            Predicate::All(parts) => parts.iter().all(|part| part.holds(set)),
            Predicate::Any(parts) => parts.iter().any(|part| part.holds(set)),
            Predicate::Not(part) => !part.holds(set),
        }
    }

    /// Add the options it reads to `options`.
    fn options<'a>(&'a self, options: &mut BTreeSet<&'a str>) {
        match self {
            Predicate::Config(option) => {
                options.insert(option);
            }
            Predicate::All(parts) | Predicate::Any(parts) => {
                for part in parts {
                    part.options(options);
                }
            }
            Predicate::Not(part) => part.options(options),
        }
    }
}

/// Where an attribute keeps what it sits on: everywhere, but where a `#[cfg(..)]` it is turns it
/// off, or one that a `#[cfg_attr(..)]` it is applies where its predicate holds, directly or
/// through another `#[cfg_attr(..)]`. A predicate that does not parse keeps it everywhere, as far
/// as the macro tells: the compiler reports it.
fn keeping(meta: &syn::Meta) -> Predicate {
    let everywhere = || Predicate::All(Vec::new());
    if meta.path().is_ident("cfg") {
        return match meta
            .require_list()
            .and_then(|list| list.parse_args::<syn::Meta>())
        {
            Ok(predicate) => Predicate::of(&predicate),
            Err(_) => everywhere(),
        };
    }
    if !meta.path().is_ident("cfg_attr") {
        return everywhere();
    }
    let wrapped = meta.require_list().and_then(|list| {
        list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated)
    });
    let Ok(wrapped) = wrapped else {
        return everywhere();
    };
    let Some(predicate) = wrapped.first() else {
        return everywhere();
    };
    // Where its predicate holds, the attributes it wraps apply.
    let kept = wrapped.iter().skip(1).map(keeping).collect();
    Predicate::Any(vec![
        Predicate::Not(Box::new(Predicate::of(predicate))),
        Predicate::All(kept),
    ])
}

/// The predicate of a `#[cfg_attr(..)]`, where it applies the attributes it wraps; one under
/// another `#[cfg_attr(..)]` in it applies in some of those configurations only, which the macro
/// takes for all of them.
fn applying(meta: &syn::Meta) -> Option<Predicate> {
    if !meta.path().is_ident("cfg_attr") {
        return None;
    }
    let wrapped = meta
        .require_list()
        .and_then(|list| {
            list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated,
            )
        })
        .ok()?;
    Some(Predicate::of(wrapped.first()?))
}

/// Whether one of `predicates` holds in every configuration, for each value of the options they
/// read. Past twelve options, which the macro does not try one by one, it takes them for holding.
fn always(predicates: &[Predicate]) -> bool {
    let mut options = BTreeSet::new();
    for predicate in predicates {
        predicate.options(&mut options);
    }
    let options: Vec<&str> = options.into_iter().collect();
    if options.len() > 12 {
        return true;
    }
    (0..1u32 << options.len()).all(|bits| {
        let set = options
            .iter()
            .enumerate()
            .filter(|(index, _)| bits & (1 << index) != 0)
            .map(|(_, option)| *option)
            .collect();
        predicates.iter().any(|predicate| predicate.holds(&set))
    })
}

/// What a type that enters the WIT implements besides what the bindings implement for it and what
/// its derives implement, for the derives of the types holding it: the traits the Orb's modules
/// implement for it by hand in every configuration, the `Default` the macro writes for a state the
/// author gives none and a flags struct's derives; and, by trait, the refusal of the impls that
/// give it a trait in some configurations only (`uncovered`), for a derive that needs the trait.
#[derive(Default)]
struct Elsewhere {
    traits: BTreeSet<String>,
    conditional: BTreeMap<String, syn::Error>,
}

impl Elsewhere {
    /// What the impls the Orb's modules write for `owner` give it, by trait (`uncovered`).
    fn by_hand(
        hands: &BTreeMap<String, Vec<(usize, syn::ItemImpl)>>,
        owner: &Ident,
        tree: &mut Module,
        modules: &[Vec<Ident>],
    ) -> Self {
        let mut elsewhere = Self::default();
        for (name, impls) in hands {
            match uncovered(impls, Vec::new(), name, owner, tree, modules) {
                None => {
                    elsewhere.traits.insert(name.clone());
                }
                Some(refusal) => {
                    elsewhere.conditional.insert(name.clone(), refusal);
                }
            }
        }
        elsewhere
    }
}

/// The refusal of the impls `hands` of the trait `name` that the Orb's modules write for `owner`,
/// each with the module it is written in, when the code the macro writes counts on them and they
/// leave a configuration without the trait: the macro reads them before the compiler evaluates
/// `#[cfg(..)]`, while each holds where the attributes on it and on the modules around it keep it
/// (`keeping`), and so does a derive of the trait under `#[cfg_attr(..)]`, where one of `derived`
/// holds. An impl that holds everywhere, or impls and derives that hold together in every
/// configuration, a `#[cfg(x)]` one beside a `#[cfg(not(x))]` one, give the trait. The refusal
/// goes to the first attribute that may drop an impl, on the impl itself, then on the module
/// nearest it.
fn uncovered(
    hands: &[(usize, syn::ItemImpl)],
    mut derived: Vec<Predicate>,
    name: &str,
    owner: &Ident,
    tree: &mut Module,
    modules: &[Vec<Ident>],
) -> Option<syn::Error> {
    let mut first = None;
    for (scope, block) in hands {
        // The attributes that may drop the impl, its own first, then those of each module around
        // it, the nearest first, with the module's name.
        let path = &modules[*scope];
        let mut dropping: Vec<(Attribute, Option<Ident>)> = block
            .attrs
            .iter()
            .map(|attribute| (attribute.clone(), None))
            .collect();
        for depth in (1..=path.len()).rev() {
            let module = &path[depth - 1];
            let attrs = &tree.at(&path[..depth]).attrs;
            dropping.extend(
                attrs
                    .iter()
                    .map(|attribute| (attribute.clone(), Some(module.clone()))),
            );
        }
        dropping.retain(|(attribute, _)| conditional(&attribute.meta));
        // An impl nothing drops gives the trait in every configuration.
        let blamed = dropping.first().cloned()?;
        derived.push(Predicate::All(
            dropping
                .iter()
                .map(|(attribute, _)| keeping(&attribute.meta))
                .collect(),
        ));
        first.get_or_insert((block, blamed));
    }
    if always(&derived) {
        return None;
    }
    let (block, (attribute, module)) = first?;
    let (_, path, _) = block.trait_.as_ref()?;
    let self_ty = &block.self_ty;
    let shown = format!(
        "`impl {} for {}`",
        written(path),
        quote!(#self_ty).to_string().replace(' ', "")
    );
    let place = match module {
        Some(module) => format!("the module `{module}` holding {shown}"),
        None => shown,
    };
    Some(syn::Error::new(
        attribute.span(),
        format!(
            "{} on {place}: the macro reads the impl before the compiler evaluates the attribute and counts on it, while the code it writes needs `{owner}` to implement `{name}` in every configuration; remove the attribute, or implement `{name}` for the configurations it turns off too, under `#[cfg(not(..))]`",
            written_condition(&attribute)
        ),
    ))
}

/// The trait a trait impl's path, written in the module `scope`, names for certain, by name, as
/// the macro follows it (`followed`): one of the standard library's, through a path to `core`,
/// `std` or `alloc` or a name of the prelude nothing in the module binds, or one of `serde`'s,
/// through a path to `serde`. A path the macro cannot follow to a crate, which a glob import may
/// bring from any crate, or a re-export through `crate::`, which may be the crate's own trait,
/// names none of them as far as it tells.
fn known_trait(
    path: &syn::Path,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    owner: &Ident,
) -> Option<String> {
    const PRELUDE: [&str; 6] = ["Clone", "Copy", "Eq", "Ord", "PartialEq", "PartialOrd"];
    match followed(path, scope, uses, modules, owner).ok()? {
        Followed::Path(named) if named.global => {
            let first = named.segments.first()?.to_string();
            let last = named.segments.last()?.to_string();
            let standard = ["core", "std", "alloc"].contains(&first.as_str());
            (standard || (first == "serde" && serde_name(&last))).then_some(last)
        }
        Followed::Name {
            ident,
            globs: false,
        } if PRELUDE.contains(&ident.to_string().as_str()) => Some(ident.to_string()),
        _ => None,
    }
}

/// Refuse a trait impl the Orb's modules write for `shape`, a type that enters the WIT other than
/// a flags struct, of a trait the bindings implement for the generated type replacing it
/// (`implemented`), or of `serde`'s, which they derive on it in an Orb with a state: the generated
/// type would implement it twice. `hands` are the impls, each with the module it is written in,
/// whose trait the macro follows from there (`known_trait`).
fn clashing(
    shape: &Shape,
    generated: &wit::Generated,
    hands: &[(usize, syn::ItemImpl)],
    uses: &[Uses],
    modules: &[Vec<Ident>],
) -> syn::Result<()> {
    let owner = &shape.ident;
    let own = implemented(&shape.kind, generated);
    for (scope, block) in hands {
        let Some((_, path, _)) = &block.trait_ else {
            continue;
        };
        let Some(name) = known_trait(path, *scope, uses, modules, owner) else {
            continue;
        };
        let reason = match own.iter().find(|(known, _)| *known == name) {
            Some((_, reason)) => *reason,
            None if generated.serialized && serde_name(&name) => {
                "the bindings derive it for a type that enters the WIT of an Orb with a state"
            }
            None => continue,
        };
        let self_ty = &block.self_ty;
        return Err(syn::Error::new(
            path.span(),
            format!(
                "`impl {} for {}`: `{owner}` enters the WIT and the generated type replacing it implements `{name}` already, as {reason}; remove the impl, or keep `{owner}` out of the WIT",
                written(path),
                quote!(#self_ty).to_string().replace(' ', "")
            ),
        ));
    }
    Ok(())
}

/// The traits of `serde`, by name, that a `#[cfg_attr(..)]` derives where its configuration holds,
/// directly or through another `#[cfg_attr(..)]`, on `owner` in the module `scope`: through a path
/// the macro follows to `serde`, or through a `Serialize` or `Deserialize` it cannot follow to a
/// crate, from a crate's glob import or from above the Orb module, which `carried_derive` takes
/// for `serde`'s too. Another crate's derive of either name implements another trait.
fn conditional_serde(
    meta: &syn::Meta,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    owner: &Ident,
) -> Vec<String> {
    serde_derived(&applied(meta), scope, uses, modules, owner, false)
}

/// The traits of `serde`, by name, that the `#[derive(..)]`s among `attrs` implement on `owner`,
/// in the module `scope`, each derive's path followed as `serde_trait` follows it.
fn serde_derived(
    attrs: &[Attribute],
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    owner: &Ident,
    unbound: bool,
) -> Vec<String> {
    attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
        .filter_map(|derive| {
            derive
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, Token![,]>::parse_terminated,
                )
                .ok()
        })
        .flatten()
        .filter_map(|path| serde_trait(&path, scope, uses, modules, owner, unbound))
        .collect()
}

/// The trait of `serde`, `Serialize` or `Deserialize`, that a path written in the module `scope`
/// for `owner` names, a derive's or an implemented trait's, followed as `carried_derive` follows a
/// derive: through a path the macro follows to `serde`, a re-export through `crate::` included,
/// under any name, or by the name of a path it cannot follow to a crate, from a crate's glob
/// import or from above the Orb module, which `carried_derive` takes for `serde`'s too, and, with
/// `unbound`, from nothing the module binds, as a crate-root `#[macro_use]` brings it. Another
/// crate's trait of either name, `rkyv::Serialize`, is another trait.
fn serde_trait(
    path: &syn::Path,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    owner: &Ident,
    unbound: bool,
) -> Option<String> {
    let name = match followed(path, scope, uses, modules, owner).ok()? {
        Followed::Path(named) => match Derive::of(named) {
            Derive::Serde(named) => named.segments.last()?.to_string(),
            Derive::Standard(..) | Derive::Other(_) => return None,
        },
        Followed::Name { ident, globs } if globs || unbound => ident.to_string(),
        Followed::Above(ident) => ident.to_string(),
        Followed::Name { .. } => return None,
    };
    serde_name(&name).then_some(name)
}

/// Whether an attribute is a `#[cfg_attr(..)]` hiding from the macro, until the compiler evaluates
/// it, what the macro reads of a type: a derive, an attribute a derive may read or a `#[cfg(..)]`,
/// directly or through another `#[cfg_attr(..)]`, or what does not parse. Rust's own attributes
/// other than `derive` and `cfg`, `allow`, `doc` or `deprecated` among them, and an attribute named
/// by a path, the generated type never takes, so under `#[cfg_attr(..)]` they change nothing the
/// macro writes.
fn hidden(meta: &syn::Meta) -> bool {
    if !meta.path().is_ident("cfg_attr") {
        return false;
    }
    let wrapped = meta.require_list().and_then(|list| {
        list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated)
    });
    let Ok(wrapped) = wrapped else {
        return true;
    };
    // The first is the configuration predicate.
    wrapped.iter().skip(1).any(|meta| {
        hidden(meta)
            || meta.path().get_ident().is_some_and(|ident| {
                ident == "cfg"
                    || ident == "derive"
                    || !BUILT_IN_ATTRIBUTES.contains(&ident.to_string().as_str())
            })
    })
}

/// The attributes Rust defines itself that may sit on a type, a field or a case: none of them is a
/// derive's helper attribute, and the generated type replacing a type that enters the WIT takes
/// none of them.
const BUILT_IN_ATTRIBUTES: &[&str] = &[
    "allow",
    "automatically_derived",
    "cfg",
    "cfg_attr",
    "debugger_visualizer",
    "deny",
    "deprecated",
    "derive",
    "doc",
    "expect",
    "forbid",
    "must_use",
    "non_exhaustive",
    "repr",
    "warn",
];

/// Whether an attribute may be a derive's helper attribute, as `#[default]` is for `Default`: a
/// single name, which none of Rust's own attributes takes.
fn helper(attribute: &Attribute) -> bool {
    attribute
        .path()
        .get_ident()
        .is_some_and(|ident| !BUILT_IN_ATTRIBUTES.contains(&ident.to_string().as_str()))
}

/// What the generated type replacing a type that enters the WIT takes of the author's attributes:
/// the paths of its derives, and the helper attributes a derive reads, on the type and on each
/// field or case. Its documentation goes into the WIT instead, and it takes none of Rust's own
/// attributes. A case's payload has no place of its own in the bindings' options, so a helper
/// attribute there is refused.
fn carried(item: &Item) -> syn::Result<Carried> {
    let helpers = |attrs: &[Attribute]| -> Vec<Attribute> {
        attrs
            .iter()
            .filter(|attribute| helper(attribute))
            .cloned()
            .collect()
    };
    let (attrs, members) = match item {
        Item::Struct(item) => (
            &item.attrs,
            item.fields
                .iter()
                .filter_map(|field| {
                    Some((wit::kebab(field.ident.as_ref()?), helpers(&field.attrs)))
                })
                .collect(),
        ),
        Item::Enum(item) => {
            for variant in &item.variants {
                let payload = variant.fields.iter().flat_map(|field| &field.attrs);
                if let Some(attribute) = payload.into_iter().find(|attribute| helper(attribute)) {
                    return Err(syn::Error::new(
                        attribute.span(),
                        format!(
                            "`{0}` enters the WIT and the generated type replaces it, which takes the attributes of its cases but not of their payloads: move this one to the case `{1}`, or keep `{0}` out of the WIT",
                            item.ident, variant.ident
                        ),
                    ));
                }
            }
            (
                &item.attrs,
                item.variants
                    .iter()
                    .map(|variant| (wit::kebab(&variant.ident), helpers(&variant.attrs)))
                    .collect(),
            )
        }
        _ => return Ok(Carried::default()),
    };
    let mut derives = Vec::new();
    for attribute in attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
    {
        derives.extend(attribute.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, Token![,]>::parse_terminated,
        )?);
    }
    Ok(Carried {
        derives,
        helpers: helpers(attrs),
        members,
    })
}

/// The derives of the standard library, each with the module of `core` defining it.
const STANDARD_DERIVES: [(&str, &str); 9] = [
    ("Clone", "clone"),
    ("Copy", "marker"),
    ("Debug", "fmt"),
    ("Default", "default"),
    ("Eq", "cmp"),
    ("Hash", "hash"),
    ("Ord", "cmp"),
    ("PartialEq", "cmp"),
    ("PartialOrd", "cmp"),
];

/// What a derive's path names, followed from the module that writes it, so the bindings' module
/// can name it too.
enum Followed {
    /// A path from the crates, `::strum::EnumIter`, or from this crate, `crate::derives::Thing`,
    /// which names the same item in every module of the crate.
    Path(Named),
    /// A single name the module neither declares nor imports by name, `Hash`: one a prelude
    /// brings, or, when `globs` is set, a glob import from a module the macro does not read.
    Name { ident: Ident, globs: bool },
    /// An item of a module above the Orb module, `super::Serialize` written in the Orb module, by
    /// its name: the macro does not read that module.
    Above(Ident),
}

/// Follow a derive's path, written in the module `scope`, to what it names: through the module's
/// `use` and `extern crate` declarations, `self::`, `super::`, the sub-modules of the Orb and the
/// glob imports from its modules, to a path from the crates or from this crate, where a first
/// name nothing in the Orb declares is a crate's, to a single name nothing in the module binds, or
/// to an item above the Orb module. `owner` is the type the derive is written on, for the errors.
fn followed(
    path: &syn::Path,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    owner: &Ident,
) -> syn::Result<Followed> {
    let written = written(path);
    let mut scope = scope;
    let mut named = Named {
        global: path.leading_colon.is_some(),
        segments: path
            .segments
            .iter()
            .map(|segment| segment.ident.clone())
            .collect(),
    };
    // Each step consumes a name or replaces one by what a declaration binds it to; declarations
    // binding one another in a circle would never end.
    for _ in 0..64 {
        if named.global {
            return Ok(Followed::Path(named));
        }
        let Some((first, rest)) = named.segments.split_first() else {
            break;
        };
        let (first, rest) = (first.clone(), rest.to_vec());
        if first == "crate" {
            return Ok(Followed::Path(named));
        }
        if first == "self" || first == "super" {
            if first == "super" {
                let Some(parent) = parent(scope, modules) else {
                    return Ok(Followed::Above(rest.last().unwrap_or(&first).clone()));
                };
                scope = parent;
            }
            if rest.is_empty() {
                break;
            }
            named.segments = rest;
            continue;
        }
        if let Some(bound) = uses[scope].names.get(&first.to_string()) {
            // `use name;` brings the crate `name` into scope.
            if !bound.global && bound.segments.len() == 1 && bound.segments[0] == first {
                named.global = true;
                continue;
            }
            let mut segments = bound.segments.clone();
            segments.extend(rest);
            named = Named {
                global: bound.global,
                segments,
            };
            continue;
        }
        if !rest.is_empty() {
            let mut child = modules[scope].clone();
            child.push(first.clone());
            if let Some(index) = modules.iter().position(|module| *module == child) {
                scope = index;
                named.segments = rest;
                continue;
            }
        }
        // A name the module neither binds nor declares: a glob import may bring it from another
        // module of the Orb, where it resolves next, or from a module the macro does not read.
        let outside = match through_globs(&first, scope, uses, modules) {
            Globbed::In(module) => {
                scope = module;
                continue;
            }
            Globbed::Outside => true,
            Globbed::Nowhere => false,
        };
        if rest.is_empty() {
            return Ok(Followed::Name {
                ident: first,
                globs: outside,
            });
        }
        named.global = true;
    }
    Err(syn::Error::new(
        path.span(),
        format!(
            "the macro cannot follow `{written}` on `{owner}` to the item it names; the generated type replacing `{owner}` is declared in the bindings' module, so name the derive from its crate, such as `::name::Derive`, or from `crate::`"
        ),
    ))
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
/// `use` or declares a sub-module of that name.
fn through_globs(name: &Ident, scope: usize, uses: &[Uses], modules: &[Vec<Ident>]) -> Globbed {
    let mut outside = false;
    let mut seen = BTreeSet::from([scope]);
    let mut pending = vec![scope];
    while let Some(scope) = pending.pop() {
        for glob in &uses[scope].globs {
            let Some(module) = glob_module(glob, scope, uses, modules) else {
                outside = true;
                continue;
            };
            let mut child = modules[module].clone();
            child.push(name.clone());
            if uses[module].names.contains_key(&name.to_string()) || modules.contains(&child) {
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

/// The module of the Orb that a glob import written in the module `scope` imports from, by scope:
/// `self`, `super`, a sub-module by name, or what a `use` of the module binds one of those to;
/// `None` for a module the macro does not read, one of a crate, under `crate::` or above the Orb
/// module.
fn glob_module(glob: &Named, scope: usize, uses: &[Uses], modules: &[Vec<Ident>]) -> Option<usize> {
    if glob.global {
        return None;
    }
    let mut scope = scope;
    let mut segments = glob.segments.clone();
    // Each step consumes a name or replaces one by what a declaration binds it to.
    for _ in 0..64 {
        let Some((first, rest)) = segments.split_first() else {
            return Some(scope);
        };
        let (first, rest) = (first.clone(), rest.to_vec());
        if first == "crate" {
            return None;
        }
        if first == "super" {
            scope = parent(scope, modules)?;
        }
        if first == "self" || first == "super" {
            segments = rest;
            continue;
        }
        if let Some(bound) = uses[scope].names.get(&first.to_string()) {
            // `use name;` brings the crate `name` into scope.
            if bound.global || (bound.segments.len() == 1 && bound.segments[0] == first) {
                return None;
            }
            segments = bound.segments.clone();
            segments.extend(rest);
            continue;
        }
        let mut child = modules[scope].clone();
        child.push(first);
        scope = modules.iter().position(|module| *module == child)?;
        segments = rest;
    }
    None
}

/// A path as the author writes it, for the errors: `derive_more::Debug`.
fn written(path: &syn::Path) -> String {
    quote!(#path).to_string().replace(' ', "")
}

/// The traits the bindings implement themselves for a generated type, each with the reason, as
/// `wit-bindgen` 0.63 writes the type: `Debug` by hand for every type; for a record or a variant,
/// `Clone` derived unless it holds a resource handle, and `Copy` unless it also holds a string or a
/// list; for an enum, `Clone`, `Copy`, `PartialEq`, `Eq`, `PartialOrd` and `Ord` derived; and
/// `Display` and `Error` by hand for the error of a result.
fn implemented(kind: &ShapeKind, generated: &wit::Generated) -> Vec<(&'static str, &'static str)> {
    let mut traits = vec![("Debug", "the bindings implement `Debug` for every type")];
    match kind {
        ShapeKind::Enum(_) => traits.extend(
            ["Clone", "Copy", "PartialEq", "Eq", "PartialOrd", "Ord"]
                .map(|name| (name, "the bindings derive it for every enum")),
        ),
        ShapeKind::Record(_) | ShapeKind::Variant(_) => {
            if !generated.resource {
                traits.push((
                    "Clone",
                    "the bindings derive it for a record or a variant that holds no resource handle",
                ));
            }
            if !generated.resource && !generated.list {
                traits.push((
                    "Copy",
                    "the bindings derive it for a record or a variant that holds no string, list or resource handle",
                ));
            }
        }
        // A flags struct keeps the author's derives on the author's struct.
        ShapeKind::Flags(_) => {}
    }
    if generated.error {
        traits.extend(["Display", "Error"].map(|name| {
            (
                name,
                "the bindings implement it for the error of a function's result",
            )
        }));
    }
    traits
}

/// A derive written on a type that enters the WIT, by what it implements.
enum Derive {
    /// One of the standard library: its trait and the module of `core` defining it.
    Standard(&'static str, &'static str),
    /// `serde`'s `Serialize` or `Deserialize`, which a state makes the bindings derive.
    Serde(Named),
    /// Any other, by the path naming it, which implements the trait of its last name, or the
    /// traits it implements under another name (`implemented_by`).
    Other(Named),
}

impl Derive {
    /// What a followed path names: a derive of the standard library when it leads from `core`,
    /// `std` or `alloc` to one of their derives, `serde`'s when it leads from `serde` or
    /// `serde_derive` to one of its two, or from `crate::` to the name of one, a re-export of this
    /// crate whose source the macro does not read, another derive otherwise.
    fn of(named: Named) -> Self {
        let first = named.segments.first().map(Ident::to_string);
        let last = named.segments.last().map(Ident::to_string);
        let from = |crates: &[&str]| {
            named.global
                && first
                    .as_deref()
                    .is_some_and(|first| crates.contains(&first))
        };
        if from(&["core", "std", "alloc"])
            && let Some(&(name, module)) = last
                .as_deref()
                .and_then(|last| STANDARD_DERIVES.iter().find(|(name, _)| *name == last))
        {
            return Self::Standard(name, module);
        }
        let re_exported = !named.global && first.as_deref() == Some("crate");
        if (from(&["serde", "serde_derive"]) || re_exported)
            && last.as_deref().is_some_and(serde_name)
        {
            return Self::Serde(named);
        }
        Self::Other(named)
    }
}

/// Whether a derive of this name is one of `serde`'s two, where no path says otherwise.
fn serde_name(name: &str) -> bool {
    matches!(name, "Serialize" | "Deserialize")
}

impl quote::ToTokens for Named {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let global = self.global.then(|| quote! { :: });
        let segments = &self.segments;
        tokens.extend(quote! { #global #(#segments)::* });
    }
}

/// One derive of a type that enters the WIT, as the generated type replacing the type takes it:
/// `None` when the bindings, or the derives a state adds, implement it already, otherwise the
/// path that names it from the bindings' module, `::core::hash::Hash` for a derive of the standard
/// library. A derive the generated type cannot take is refused: one of another crate implementing
/// a trait the bindings implement, which would implement it twice, with `unread`, the helper
/// attributes no other derive of the type reads, which go with it, and one of the standard library
/// or of `serde`, by name or under another name, that the type's contents rule out, trait by trait
/// (`ruled_out`); `module_types` tells what the generated types of the module's own types
/// implement, by WIT name, for the fields holding one.
fn carried_derive(
    shape: &Shape,
    path: &syn::Path,
    generated: &wit::Generated,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    unread: &str,
    module_types: &BTreeMap<String, ModuleType>,
) -> syn::Result<Option<proc_macro2::TokenStream>> {
    let owner = &shape.ident;
    let written = written(path);
    let refuse = |message: String| Err(syn::Error::new(path.span(), message));
    // Where the bindings derive serde's traits, the author's `Serialize` or `Deserialize` is
    // theirs under a path the macro cannot follow to a crate as well: the module calls it so.
    let covered = |ident: &Ident| generated.serialized && serde_name(&ident.to_string());
    // The refusal of a derive of the trait `name` that the type's contents rule out, `held` saying
    // what the type holds and `remove` how to take the derive off.
    let cannot = |name: &str, held: &str, remove: &str| {
        format!(
            "`{written}` on `{owner}`: `{owner}` enters the WIT and {held}, so the generated type replacing it cannot derive `{name}`; {remove}, or keep `{owner}` out of the WIT"
        )
    };
    // A resource handle implements none of the traits a derive implements.
    let holds_handle = |name: &str, remove: &str| {
        cannot(
            name,
            &format!("holds a resource handle, which implements no `{name}`"),
            remove,
        )
    };
    // serde's derives apply to no type holding what the bindings give no `serde` implementation.
    let unwritable = |name: &str| {
        if generated.resource {
            return Some(refuse(holds_handle(name, "remove the derive")));
        }
        let held = if generated.consumed {
            "holds a type of a consumed package, which the bindings give no `serde` implementation in an Orb without a state"
        } else if generated.unserializable {
            "holds a consumed package's flags type, which the bindings give no `serde` implementation"
        } else {
            return None;
        };
        Some(refuse(cannot(name, held, "remove the derive")))
    };
    let followed = followed(path, shape.scope, uses, modules, owner)?;
    // A `Serialize` or `Deserialize` the macro cannot follow to a crate, from a crate's glob or
    // from above the Orb module, is serde's where the bindings derive serde's traits, so what the
    // type holds is the reason to give first, before the path.
    if let Followed::Name { ident, globs: true } | Followed::Above(ident) = &followed
        && serde_name(&ident.to_string())
        && let Some(refused) = unwritable(&ident.to_string())
    {
        return refused;
    }
    let derive = match followed {
        Followed::Path(named) => Derive::of(named),
        // A single name no declaration of the module binds is a prelude's, which brings the
        // derives of the standard library, or a glob import's, which the macro cannot read.
        Followed::Name { ident, globs } => {
            match STANDARD_DERIVES.iter().find(|(name, _)| ident == name) {
                Some(&(name, module)) => Derive::Standard(name, module),
                None if globs && covered(&ident) => return Ok(None),
                None => {
                    let glob = if globs {
                        ", unless a glob import brings it"
                    } else {
                        ""
                    };
                    return refuse(format!(
                        "`{ident}` on `{owner}`: `{owner}` enters the WIT and the generated type replacing it is declared in the bindings' module, which `{ident}` does not reach: this module imports no `{ident}` by name{glob}; import it with `use`, or name it from its crate, such as `::name::{ident}`"
                    ));
                }
            }
        }
        Followed::Above(ident) if covered(&ident) => return Ok(None),
        Followed::Above(_) => {
            return refuse(format!(
                "`{written}` on `{owner}` leaves the Orb module, where the macro cannot follow it; the generated type replacing `{owner}` is declared in the bindings' module, so name the derive from its crate, such as `::name::Derive`, or from `crate::`"
            ));
        }
    };
    let own = implemented(&shape.kind, generated);
    let implements = |name: &str| own.iter().find(|(known, _)| *known == name);
    // The helper attribute on a member through which the derive gives the member its own value or
    // leaves it out, where the macro knows it (`member_helper`): what such a member holds is the
    // derive's.
    let customizing = match &derive {
        Derive::Other(named) => named.segments.last().and_then(member_helper),
        Derive::Standard(..) | Derive::Serde(_) => None,
    };
    // Why the type's contents rule out the trait `name` of the standard library, if they do, for
    // the generated type to derive it, with `remove` saying how to take the derive off: the first
    // part of the fields (`wit::Held`) that lacks the trait, but for the parts of a member the
    // derive customizes and, for `Default`, which makes a value without them, the parts an option,
    // a list or a variant's case holds. A type of the module or a consumed type held in a field
    // rules the trait out for a handle, a string or a list it holds, which the type holds through
    // it, unless the macro sees the trait implemented for it; a trait it lacks otherwise is left to
    // the author, who may implement it by hand where the macro does not read, outside the Orb
    // module or through a `crate::` path, but for one the macro sees implemented in some
    // configurations only, whose impl is refused (`uncovered`). A trait the macro does not know
    // the rules of, `Display`, is left to the derive.
    let ruled_out = |name: &str, remove: &str| -> Option<syn::Error> {
        if !STANDARD_DERIVES.iter().any(|(known, _)| *known == name) {
            return None;
        }
        let no_default = "every value of it holds a resource handle, which implements no `Default`";
        let not_copy = || {
            format!(
                "`{written}` on `{owner}`: `{owner}` enters the WIT and holds a string or a list, so the generated type replacing it cannot be `Copy`; {remove}"
            )
        };
        for part in &generated.parts {
            let customized = customizing.is_some_and(|helper| {
                shape.carried.members.iter().any(|(member, helpers)| {
                    *member == part.member
                        && helpers
                            .iter()
                            .any(|attribute| attribute.path().is_ident(helper))
                })
            });
            if customized || (name == "Default" && part.absent_in_default) {
                continue;
            }
            let message = match &part.held {
                wit::Held::Handle if name == "Default" => cannot(name, no_default, remove),
                wit::Held::Handle => holds_handle(name, remove),
                wit::Held::List if name == "Copy" => not_copy(),
                wit::Held::Float(float) if matches!(name, "Eq" | "Ord" | "Hash") => cannot(
                    name,
                    &format!("holds an `{float}`, which implements no `{name}`"),
                    remove,
                ),
                wit::Held::Result if name == "Default" => cannot(
                    name,
                    "holds a `Result`, which implements no `Default`",
                    remove,
                ),
                wit::Held::Own(wit_name) => {
                    let Some(inner) = module_types.get(wit_name) else {
                        continue;
                    };
                    if inner.traits.contains(name) {
                        continue;
                    }
                    if let Some(refusal) = inner.conditional.get(name) {
                        return Some(refusal.clone());
                    }
                    if name == "Default" && inner.generated.needs_handle {
                        cannot(name, no_default, remove)
                    } else if name != "Default" && inner.generated.resource {
                        holds_handle(name, remove)
                    } else if name == "Copy" && inner.generated.list {
                        not_copy()
                    } else {
                        continue;
                    }
                }
                wit::Held::Consumed {
                    name: inner,
                    kind,
                    interface,
                    list,
                } if name == "Copy" && *list => cannot(
                    name,
                    &format!(
                        "holds `{inner}`, {kind} of {interface}, which holds a string or a list"
                    ),
                    remove,
                ),
                _ => continue,
            };
            return Some(syn::Error::new(path.span(), message));
        }
        None
    };
    match derive {
        Derive::Standard(name, module) => {
            if implements(name).is_some() {
                return Ok(None);
            }
            if let Some(refusal) = ruled_out(name, "remove the derive") {
                return Err(refusal);
            }
            let (module, name) = (format_ident!("{module}"), format_ident!("{name}"));
            Ok(Some(quote! { ::core::#module::#name }))
        }
        Derive::Serde(named) => {
            let name = named.segments.last().expect("serde's derive has a name");
            if let Some(refused) = unwritable(&name.to_string()) {
                return refused;
            }
            Ok((!generated.serialized).then(|| quote! { #named }))
        }
        Derive::Other(named) => {
            let last = named.segments.last().expect("a derive's path has a name");
            for trait_name in implemented_by(last, &shape.carried.helpers) {
                let remove = match trait_helper(last) {
                    Some(helper) => format!("remove `{trait_name}` from `#[{helper}(..)]`"),
                    None if unread.is_empty() => "remove the derive".to_owned(),
                    None => format!("remove the derive and the attributes it reads, {unread}"),
                };
                if let Some((name, reason)) = implements(&trait_name) {
                    return refuse(format!(
                        "`{written}` on `{owner}`: `{owner}` enters the WIT and the generated type replacing it implements `{name}` already, as {reason}; {remove}, or keep `{owner}` out of the WIT"
                    ));
                }
                if let Some(refusal) = ruled_out(&trait_name, &remove) {
                    return Err(refusal);
                }
            }
            Ok(Some(quote! { #named }))
        }
    }
}

/// What a derive written on a type reads of the helper attributes, as far as the macro knows.
#[derive(Clone, Copy)]
enum Reads {
    /// Those of one name: `#[default]` for the standard library's `Default`, `#[serde]` for
    /// serde's derives.
    Named(&'static str),
    /// None, as the other derives of the standard library.
    Nothing,
    /// Any: a derive of another crate, or one the macro cannot follow, whose helper attributes
    /// the macro does not know.
    Any,
}

impl Reads {
    /// What the derive `path` written on `shape` reads, by what it names, followed as
    /// `carried_derive` follows it.
    fn of(shape: &Shape, path: &syn::Path, uses: &[Uses], modules: &[Vec<Ident>]) -> Self {
        let derive = match followed(path, shape.scope, uses, modules, &shape.ident) {
            Ok(Followed::Path(named)) => Derive::of(named),
            Ok(Followed::Name { ident, .. }) => {
                match STANDARD_DERIVES.iter().find(|(name, _)| ident == name) {
                    Some(&(name, module)) => Derive::Standard(name, module),
                    None => return Self::Any,
                }
            }
            Ok(Followed::Above(_)) | Err(_) => return Self::Any,
        };
        match derive {
            Derive::Standard("Default", _) => Self::Named("default"),
            Derive::Standard(..) => Self::Nothing,
            Derive::Serde(_) => Self::Named("serde"),
            Derive::Other(_) => Self::Any,
        }
    }

    /// Whether it reads a helper attribute of this name.
    fn reads(self, name: &str) -> bool {
        match self {
            Self::Named(known) => known == name,
            Self::Nothing => false,
            Self::Any => true,
        }
    }
}

/// How a module binds a name that the bindings' module does not reach.
enum Bound {
    /// By an item it declares.
    Declared,
    /// By `use` or `extern crate`.
    Imported,
}

/// How the module `scope` binds `name`, itself or through its glob imports from the Orb's
/// modules: `None` for a name it does not bind, and for a crate imported under its own name,
/// `use serde;`, which every module reaches.
fn bound(name: &Ident, scope: usize, uses: &[Uses], modules: &[Vec<Ident>]) -> Option<Bound> {
    let name = name.to_string();
    let mut seen = BTreeSet::from([scope]);
    let mut pending = vec![scope];
    while let Some(scope) = pending.pop() {
        if uses[scope].items.contains(&name) {
            return Some(Bound::Declared);
        }
        if let Some(named) = uses[scope].names.get(&name) {
            let itself = named.segments.len() == 1 && named.segments[0] == name;
            return (!itself).then_some(Bound::Imported);
        }
        for glob in &uses[scope].globs {
            if let Some(module) = glob_module(glob, scope, uses, modules)
                && seen.insert(module)
            {
                pending.push(module);
            }
        }
    }
    None
}

/// The first path in a helper attribute's arguments that starts from the module `scope`, which
/// the bindings' module does not reach, as written up to its first part that is not a name: one
/// from `self::` or `super::`, or from a name the module binds. A name after `::`, `.` or `'`
/// continues a path, reads a field or is a lifetime, a name before `=` is a key, the name of a
/// derive of the standard library, which the arguments give as a keyword or as a derive, reaches
/// the prelude's from every module, and one of `defined`, a type of the module that the bindings'
/// module declares under the same name, reaches that declaration. With `code`, the fields of the
/// type, the arguments are what a known derive reads as code (`read_as_code`): a field, which the
/// derive binds, names that field, and a name nothing in the module binds is one a glob import the
/// macro cannot follow may bring (`through_globs`), but for a keyword, a macro's name, derive_more's
/// `_0` for an unnamed field, a name of the standard prelude or a primitive type, which every
/// module reaches, and a lowercase name before `::`, which may be a crate's. A type that enters the
/// WIT has no generic parameter to name.
fn from_module(
    arguments: proc_macro2::TokenStream,
    scope: usize,
    uses: &[Uses],
    modules: &[Vec<Ident>],
    defined: &BTreeSet<String>,
    code: Option<&BTreeSet<String>>,
) -> Option<String> {
    use proc_macro2::{Spacing, TokenTree};
    let trees: Vec<TokenTree> = arguments.into_iter().collect();
    let punct = |index: usize, char: char| match trees.get(index) {
        Some(TokenTree::Punct(punct)) => punct.as_char() == char,
        _ => false,
    };
    let colons = |index: usize| punct(index, ':') && punct(index + 1, ':');
    for (index, tree) in trees.iter().enumerate() {
        let ident = match tree {
            TokenTree::Group(group) => {
                match from_module(group.stream(), scope, uses, modules, defined, code) {
                    Some(path) => return Some(path),
                    None => continue,
                }
            }
            TokenTree::Ident(ident) => ident,
            TokenTree::Punct(_) | TokenTree::Literal(_) => continue,
        };
        let continued = index > 0
            && (punct(index - 1, '.')
                || punct(index - 1, '\'')
                || (index > 1 && colons(index - 2)));
        let key = match trees.get(index + 1) {
            Some(TokenTree::Punct(punct)) => {
                punct.as_char() == '=' && punct.spacing() == Spacing::Alone
            }
            _ => false,
        };
        let name = ident.to_string();
        if continued
            || key
            || STANDARD_DERIVES.iter().any(|(derive, _)| *derive == name)
            || defined.contains(&name)
            || code.is_some_and(|fields| fields.contains(&name))
        {
            continue;
        }
        let relative = ident == "super" || (ident == "self" && colons(index + 1));
        let reached = name == "Self"
            || RUST_KEYWORDS.contains(&name.as_str())
            || STANDARD_NAMES.contains(&name.as_str())
            || (punct(index + 1, '!') && matches!(trees.get(index + 2), Some(TokenTree::Group(_))))
            || name
                .strip_prefix('_')
                .is_some_and(|digits| digits.chars().all(|digit| digit.is_ascii_digit()))
            || (colons(index + 1) && name.starts_with(|first: char| first.is_ascii_lowercase()));
        let globbed = code.is_some()
            && !reached
            && matches!(through_globs(ident, scope, uses, modules), Globbed::Outside);
        if relative || bound(ident, scope, uses, modules).is_some() || globbed {
            let mut path = ident.to_string();
            let mut next = index + 1;
            while colons(next)
                && let Some(TokenTree::Ident(segment)) = trees.get(next + 2)
            {
                path.push_str(&format!("::{segment}"));
                next += 3;
            }
            return Some(path);
        }
    }
    None
}

/// The names the standard prelude brings into every module, the bindings' module among them, with
/// the primitive types.
const STANDARD_NAMES: &[&str] = &[
    "AsMut",
    "AsRef",
    "Box",
    "Clone",
    "Copy",
    "Default",
    "DoubleEndedIterator",
    "Drop",
    "Eq",
    "Err",
    "ExactSizeIterator",
    "Extend",
    "Fn",
    "FnMut",
    "FnOnce",
    "From",
    "FromIterator",
    "Future",
    "Into",
    "IntoFuture",
    "IntoIterator",
    "Iterator",
    "None",
    "Ok",
    "Option",
    "Ord",
    "PartialEq",
    "PartialOrd",
    "Result",
    "Send",
    "Sized",
    "Some",
    "String",
    "Sync",
    "ToOwned",
    "ToString",
    "TryFrom",
    "TryInto",
    "Unpin",
    "Vec",
    "align_of",
    "align_of_val",
    "drop",
    "size_of",
    "size_of_val",
    "bool",
    "char",
    "f32",
    "f64",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "str",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
];

/// What a known derive reads as code in the arguments of the helper attribute `name`, which then
/// names from the bindings' module: the tokens of smart_default's `#[default(..)]` or
/// `#[default = ..]`, the format arguments after the string of thiserror's `#[error(..)]` and of
/// derive_more's `#[display(..)]` and `#[debug(..)]`, and, written in strings, the names the string
/// of thiserror's `#[error(..)]` captures, `{PAGE}`, derivative's `value = ".."`, and serde's
/// `default`, `with`, `serialize_with`, `deserialize_with` and `skip_serializing_if`. Elsewhere a
/// word may be the derive's own syntax, as `ignore` in `#[from(ignore)]`.
fn read_as_code(
    name: &Ident,
    arguments: proc_macro2::TokenStream,
) -> Vec<proc_macro2::TokenStream> {
    use proc_macro2::TokenTree;
    if name == "default" {
        return vec![arguments];
    }
    if name == "derivative" {
        return strings(arguments, &["value"]);
    }
    if name == "serde" {
        return strings(
            arguments,
            &[
                "default",
                "with",
                "serialize_with",
                "deserialize_with",
                "skip_serializing_if",
            ],
        );
    }
    if name != "error" && name != "display" && name != "debug" {
        return Vec::new();
    }
    let trees: Vec<TokenTree> = arguments.into_iter().collect();
    let format = trees
        .iter()
        .enumerate()
        .find_map(|(index, tree)| match tree {
            TokenTree::Literal(literal) => match syn::Lit::new(literal.clone()) {
                syn::Lit::Str(format) => Some((index, format.value())),
                _ => None,
            },
            _ => None,
        });
    let Some((position, format)) = format else {
        return Vec::new();
    };
    let mut code = vec![trees[position + 1..].iter().cloned().collect()];
    if name == "error" {
        code.push(captures(&format));
    }
    code
}

/// The code in the strings of the arguments `key = ".."`, at any depth, for the keys `keys`.
fn strings(arguments: proc_macro2::TokenStream, keys: &[&str]) -> Vec<proc_macro2::TokenStream> {
    use proc_macro2::TokenTree;
    let trees: Vec<TokenTree> = arguments.into_iter().collect();
    let mut code = Vec::new();
    for (index, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Group(group) => code.extend(strings(group.stream(), keys)),
            TokenTree::Ident(key) if keys.iter().any(|known| key == known) => {
                let (Some(TokenTree::Punct(equals)), Some(TokenTree::Literal(literal))) =
                    (trees.get(index + 1), trees.get(index + 2))
                else {
                    continue;
                };
                if equals.as_char() == '='
                    && let syn::Lit::Str(string) = syn::Lit::new(literal.clone())
                    && let Ok(tokens) = string.parse::<proc_macro2::TokenStream>()
                {
                    code.push(tokens);
                }
            }
            _ => {}
        }
    }
    code
}

/// The names a format string captures from its scope, `{PAGE}` or `{PAGE:?}`, as tokens: what is
/// neither a position, `{}` or `{0}`, nor a brace a doubled `{{` escapes.
fn captures(format: &str) -> proc_macro2::TokenStream {
    let mut names = proc_macro2::TokenStream::new();
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        if let Some(escaped) = rest.strip_prefix('{') {
            rest = escaped;
            continue;
        }
        let end = rest.find(['}', ':']).unwrap_or(rest.len());
        if let Ok(name) = syn::parse_str::<Ident>(rest[..end].trim()) {
            names.extend(quote! { #name , });
        }
        rest = &rest[end..];
    }
    names
}

/// A helper attribute as the errors show it: `#[strum(..)]`, `#[default = ..]` or `#[default]`.
fn shown(attribute: &Attribute) -> String {
    let name = written(attribute.path());
    match &attribute.meta {
        syn::Meta::Path(_) => format!("#[{name}]"),
        syn::Meta::List(_) => format!("#[{name}(..)]"),
        syn::Meta::NameValue(_) => format!("#[{name} = ..]"),
    }
}

/// The helper attributes of `carried` that none of `others` reads, as the errors show them, each
/// once: those a derive refused for what the bindings implement takes with it.
fn unread(carried: &Carried, others: &[Reads]) -> String {
    let members = carried.members.iter().flat_map(|(_, helpers)| helpers);
    let mut unread: Vec<String> = Vec::new();
    for attribute in carried.helpers.iter().chain(members) {
        let name = written(attribute.path());
        let shown = format!("`{}`", shown(attribute));
        if !others.iter().any(|reads| reads.reads(&name)) && !unread.contains(&shown) {
            unread.push(shown);
        }
    }
    unread.join(", ")
}

/// Refuse a helper attribute that the bindings' module, which reads it on the generated type
/// replacing `shape`, would not know: an attribute macro the type's module imports, which is no
/// derive's helper attribute, unless a derive the generated type takes reads an attribute of
/// that name, an attribute none of the derives the generated type takes reads, by what each of
/// them reads, `readers`, and one whose arguments name what the type's module
/// declares or imports, or start from that module, but for the types of the module the bindings'
/// module declares beside the generated type under the same name, `defined`: in the tokens, and
/// in what a known derive reads as code, strings included, where a name a glob import the macro
/// cannot follow may bring is refused too (`from_module`).
fn readable(
    shape: &Shape,
    readers: &[Reads],
    uses: &[Uses],
    modules: &[Vec<Ident>],
    defined: &BTreeSet<String>,
) -> syn::Result<()> {
    let owner = &shape.ident;
    let carried = &shape.carried;
    let fields: BTreeSet<String> = match &shape.kind {
        ShapeKind::Record(fields) => fields.iter().map(|(field, _)| field.to_string()).collect(),
        _ => BTreeSet::new(),
    };
    let members = carried.members.iter().flat_map(|(_, helpers)| helpers);
    for attribute in carried.helpers.iter().chain(members) {
        let shown = shown(attribute);
        let refuse = |message: String| Err(syn::Error::new(attribute.span(), message));
        let name = attribute
            .path()
            .get_ident()
            .expect("a helper attribute is a single name");
        // A derive's helper attribute resolves in the macro namespace: an item of its name the
        // module imports, `use core::default;` under `#[default]`, shadows nothing where a derive
        // the generated type takes reads it by name, and the macro cannot tell a module or a type
        // imported from an attribute macro.
        let named = readers
            .iter()
            .any(|reads| matches!(reads, Reads::Named(known) if name == known));
        if !named && let Some(Bound::Imported) = bound(name, shape.scope, uses, modules) {
            return refuse(format!(
                "`{shown}` on `{owner}`: `{owner}` enters the WIT and the generated type replacing it takes its derives and the helper attributes they read, which come with the derives, while this module imports `{name}`, an attribute macro, which the generated type cannot take; remove it, or keep `{owner}` out of the WIT"
            ));
        }
        if !readers.iter().any(|reads| reads.reads(&name.to_string())) {
            return refuse(format!(
                "`{shown}` on `{owner}`: `{owner}` enters the WIT and the generated type replacing it takes no derive that reads this attribute: a derive of the standard library reads none but `#[default]`, for `Default`, and a derive of what the bindings implement themselves is left to them; remove the attribute, or keep `{owner}` out of the WIT"
            ));
        }
        let arguments = match &attribute.meta {
            syn::Meta::Path(_) => continue,
            syn::Meta::List(list) => list.tokens.clone(),
            syn::Meta::NameValue(pair) => {
                let value = &pair.value;
                quote! { #value }
            }
        };
        let named = from_module(arguments.clone(), shape.scope, uses, modules, defined, None)
            .or_else(|| {
                read_as_code(name, arguments).into_iter().find_map(|code| {
                    from_module(code, shape.scope, uses, modules, defined, Some(&fields))
                })
            });
        if let Some(path) = named {
            return refuse(format!(
                "`{shown}` on `{owner}` names `{path}` from this module: `{owner}` enters the WIT and the generated type replacing it is declared in the bindings' module, which reads the attribute and does not reach `{path}`; name it from `crate::` or from its crate, `::name::..`, or keep `{owner}` out of the WIT"
            ));
        }
    }
    Ok(())
}

/// A type of the module that enters the WIT, for the types holding it: how the bindings generate
/// it, and the traits its generated type, or the author's flags struct, implements as far as the
/// macro sees: what the bindings implement for it, what its derives implement, and what it
/// implements besides, as `carried_attributes` is told, with the refusal of the impls that give it
/// a trait in some configurations only, by trait.
struct ModuleType {
    generated: wit::Generated,
    traits: BTreeSet<String>,
    conditional: BTreeMap<String, syn::Error>,
}

/// The full WIT name of a type of the module that enters the WIT, `ns:name/interface@version/type`,
/// as the bindings' options select it, from the interface `placement` defines it in.
fn selector(shape: &Shape, placement: &BTreeMap<String, Ident>, package: &str) -> String {
    let interface = wit::kebab(&placement[&shape.ident.to_string()]);
    format!(
        "{}/{}",
        identity(package, &interface),
        wit::kebab(&shape.ident)
    )
}

/// Where the WIT puts the module's types: the interface defining each, by the type's name
/// (`wit::placement`), and the WIT names of the types each interface `use`s from the others, by
/// interface name, which its bindings' module declares as aliases (`wit::render`).
struct Layout<'a> {
    placement: &'a BTreeMap<String, Ident>,
    used: &'a BTreeMap<String, BTreeSet<String>>,
}

/// The attributes the bindings put on the generated types replacing the author's, as the options
/// `additional_type_attributes` and `additional_member_attributes` of `wit-bindgen` select them,
/// by the type's full WIT name, `ns:name/interface@version/type`, and the member's after a `.`:
/// for each type that enters the WIT, its derives, less those the bindings implement themselves,
/// and the helper attributes they read, on the type and on each field or case, each of which the
/// bindings' module must know (`readable`). A type the ignore option leaves without serde's derives
/// only for the name of one that cannot carry them, of the module or consumed, takes them here
/// instead (`Bindings::namesakes`). `elsewhere` is what each type that enters the WIT implements
/// besides what the bindings implement for it and what its derives implement, by the type's name:
/// the traits the Orb's modules implement for it by hand, a state's `Default`, a flags struct's
/// derives, and the refusal of the impls that give it a trait in some configurations only.
fn carried_attributes(
    shapes: &[Shape],
    layout: Layout<'_>,
    package: &str,
    bindings: &wit::Bindings,
    elsewhere: &BTreeMap<String, Elsewhere>,
    uses: &[Uses],
    modules: &[Vec<Ident>],
) -> syn::Result<(Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>)> {
    let Layout { placement, used } = layout;
    let mut types = Vec::new();
    let mut members = Vec::new();
    let mut namesakes: BTreeSet<&String> = bindings.namesakes.iter().collect();
    let selector_of = |shape: &Shape| selector(shape, placement, package);
    // The types of the module, by WIT name, for the types holding one.
    let module_types: BTreeMap<String, ModuleType> = shapes
        .iter()
        .map(|shape| {
            let generated = bindings
                .types
                .get(&selector_of(shape))
                .cloned()
                .unwrap_or_default();
            let besides = elsewhere.get(&shape.ident.to_string());
            let traits = implemented(&shape.kind, &generated)
                .into_iter()
                .map(|(name, _)| name.to_owned())
                .chain(
                    shape
                        .carried
                        .derives
                        .iter()
                        .filter_map(|path| path.segments.last())
                        .flat_map(|segment| implemented_by(&segment.ident, &shape.carried.helpers)),
                )
                .chain(
                    besides
                        .into_iter()
                        .flat_map(|besides| besides.traits.clone()),
                )
                .collect();
            let conditional = besides
                .map(|besides| besides.conditional.clone())
                .unwrap_or_default();
            let module_type = ModuleType {
                generated,
                traits,
                conditional,
            };
            (wit::kebab(&shape.ident), module_type)
        })
        .collect();
    for shape in shapes {
        let selector = selector_of(shape);
        let namesake = namesakes.remove(&selector);
        if shape.carried.is_empty() && !namesake {
            continue;
        }
        let generated = bindings.types.get(&selector).cloned().unwrap_or_default();
        // What each derive reads of the helper attributes: a derive refused for what the bindings
        // implement names those no other derive reads, which go with it.
        let reads: Vec<Reads> = shape
            .carried
            .derives
            .iter()
            .map(|path| Reads::of(shape, path, uses, modules))
            .collect();
        let mut derives = Vec::new();
        if namesake {
            derives.extend([
                quote! { ::serde::Serialize },
                quote! { ::serde::Deserialize },
            ]);
        }
        let mut readers = Vec::new();
        for (index, path) in shape.carried.derives.iter().enumerate() {
            let mut others = reads.clone();
            others.remove(index);
            let unread = unread(&shape.carried, &others);
            if let Some(derive) = carried_derive(
                shape,
                path,
                &generated,
                uses,
                modules,
                &unread,
                &module_types,
            )? {
                derives.push(derive);
                readers.push(reads[index]);
            }
        }
        // The types of the module the bindings' module declares beside the generated type, under
        // the author's name: those the owner's interface defines, and those it `use`s, for any of
        // its functions or types, which the module declares as aliases; a flags struct only where
        // it is used, since `with` maps the defining interface's flags onto the author's struct
        // without declaring it there.
        let interface = &placement[&shape.ident.to_string()];
        let aliased = used.get(&wit::kebab(interface));
        let defined: BTreeSet<String> = shapes
            .iter()
            .filter(|other| {
                let together = placement[&other.ident.to_string()] == *interface;
                let used = aliased.is_some_and(|names| names.contains(&wit::kebab(&other.ident)));
                let declared = match other.kind {
                    ShapeKind::Flags(_) => used,
                    _ => together || used,
                };
                declared && type_ident(&wit::kebab(&other.ident)) == other.ident
            })
            .map(|other| other.ident.to_string())
            .collect();
        readable(shape, &readers, uses, modules, &defined)?;
        let derive = (!derives.is_empty()).then(|| quote! { #[derive(#(#derives),*)] });
        let helpers = &shape.carried.helpers;
        if derive.is_some() || !helpers.is_empty() {
            let literal = LitStr::new(&selector, Span::call_site());
            types.push(quote! { #literal: [#derive #(#helpers)*] });
        }
        for (member, helpers) in &shape.carried.members {
            if !helpers.is_empty() {
                let literal = LitStr::new(&format!("{selector}.{member}"), Span::call_site());
                members.push(quote! { #literal: [#(#helpers)*] });
            }
        }
    }
    for selector in namesakes {
        let literal = LitStr::new(selector, Span::call_site());
        types.push(quote! { #literal: [#[derive(::serde::Serialize, ::serde::Deserialize)]] });
    }
    Ok((types, members))
}

/// The types, among those that stay out of the WIT, that serde serializes: each state that stays
/// out, and every type of the module its fields reach, directly or through other such types. A
/// name resolves by `own_or_only`; one that resolves to a type entering the WIT stops there,
/// because that type is generated with serde's derives, and one the module does not declare is
/// the author's to serialize.
fn serialized(unreached: &[Candidate], entered: &[Shape], states: &[State]) -> BTreeSet<usize> {
    let mut serialized = BTreeSet::new();
    let mut pending: Vec<(Ident, usize)> = Vec::new();
    for (index, candidate) in unreached.iter().enumerate() {
        let state = matches!(candidate.item, Item::Struct(_))
            && states
                .iter()
                .any(|state| state.ident == candidate.ident && state.scope == candidate.scope);
        if state && serialized.insert(index) {
            pending.extend(
                candidate
                    .references
                    .iter()
                    .map(|name| (name.clone(), candidate.scope)),
            );
        }
    }
    while let Some((name, scope)) = pending.pop() {
        // Each type of that name, with its module and its place among the unreached, if there.
        let named = entered
            .iter()
            .filter(|shape| shape.ident == name)
            .map(|shape| (shape.scope, None))
            .chain(
                unreached
                    .iter()
                    .enumerate()
                    .filter(|(_, candidate)| candidate.ident == name)
                    .map(|(index, candidate)| (candidate.scope, Some(index))),
            );
        if let Some(Some(index)) = own_or_only(named, scope)
            && serialized.insert(index)
        {
            let candidate = &unreached[index];
            pending.extend(
                candidate
                    .references
                    .iter()
                    .map(|name| (name.clone(), candidate.scope)),
            );
        }
    }
    serialized
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
        asynchronous: function.sig.asyncness.is_some(),
        docs: wit::docs(&function.attrs)?,
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
/// module's own types and converts them to and from the import side of the bindings.
fn declaration_code(
    declaration: &Declared,
    callbacks: &BTreeMap<String, wit::Callback>,
    hook_shapes: &BTreeSet<String>,
    holding: &BTreeSet<String>,
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
            holding,
            &import_types,
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
/// runs the plan, and converts the composed result back.
fn derived_hook_code(
    declaration: &Declared,
    input: &Type,
    hook_shapes: &BTreeSet<String>,
    holding: &BTreeSet<String>,
    import_types: &proc_macro2::TokenStream,
    depth: usize,
) -> proc_macro2::TokenStream {
    let path = hook_path(&declaration.hook);
    let root = supers(depth);
    let export = export_module(depth, &path);
    let import = import_module(depth, &path);
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
    // A string, a list, or a module type that holds one anywhere crosses to an import by
    // reference, as the bindings take them.
    let by_reference = holds_list(input, holding);
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

/// The module types whose values hold a string or a list anywhere, in their fields or payloads,
/// directly or through other module types.
fn holding_lists(shapes: &[Shape]) -> BTreeSet<String> {
    let mut holding = BTreeSet::new();
    loop {
        let known = holding.len();
        for shape in shapes {
            if foreign::shape_types(shape)
                .iter()
                .any(|ty| holds_list(ty, &holding))
            {
                holding.insert(shape.ident.to_string());
            }
        }
        if holding.len() == known {
            return holding;
        }
    }
}

/// Whether a type of the module holds a string or a list: it is one, its type parameters or its
/// tuple elements hold one, or it is a module type of `holding`.
fn holds_list(ty: &Type, holding: &BTreeSet<String>) -> bool {
    match ty {
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let name = segment.ident.to_string();
            let arguments = match &segment.arguments {
                syn::PathArguments::AngleBracketed(arguments) => arguments
                    .args
                    .iter()
                    .any(|argument| matches!(argument, syn::GenericArgument::Type(ty) if holds_list(ty, holding))),
                _ => false,
            };
            name == "String" || name == "Vec" || holding.contains(&name) || arguments
        }),
        Type::Tuple(tuple) => tuple.elems.iter().any(|ty| holds_list(ty, holding)),
        _ => false,
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
            // The bindings' name of the type, on both sides.
            let ident = type_ident(&wit::kebab(&shape.ident));
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
                ShapeKind::Flags(_) => unreachable!("a derived hook reaches no flags"),
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
                let generated = type_ident(&wit::kebab(ident));
                return quote! { #module::#generated };
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
            .filter_map(|field| {
                let cfg = field
                    .attrs
                    .iter()
                    .filter(|attribute| attribute.path().is_ident("cfg"))
                    .cloned()
                    .collect();
                Some((field.ident.clone()?, field.ty.span(), cfg))
            })
            .collect(),
        service,
        version,
        validator: None,
        identity: String::new(),
        scope: 0,
        derives_default: derives(&item.attrs, "Default"),
        authored_default: false,
    })
}

/// The Rust side of the states: the kernel's `state-init` callback answering one declaration per
/// owning service, the accessors on each state type, and the JSON helpers they share.
fn state_code(states: &[State], modules: &[Vec<Ident>]) -> proc_macro2::TokenStream {
    if states.is_empty() {
        return quote! {};
    }
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
        // Each field takes its type's `Default`; a type without one is reported on the field,
        // where the author either gives the type a `Default` or implements the state's. The
        // initializer carries the field's `#[cfg(..)]`, which the compiler evaluates after the
        // macro, so the value holds the fields the struct holds, as a derived `Default` does.
        let fields = state.fields.iter().map(|(field, span, cfg)| {
            let value = quote::quote_spanned!(*span=> ::core::default::Default::default());
            quote! { #(#cfg)* #field: #value }
        });
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
        let default = (!state.authored_default).then(|| {
            quote! {
                impl ::core::default::Default for #path {
                    fn default() -> Self {
                        Self { #(#fields),* }
                    }
                }
            }
        });
        quote! {
            #default
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

/// The Rust side of the resources: the kernel's `resources-init` callbacks listing, opening and
/// closing them, one slot in the component's memory per resource, and `with` on each type.
fn resource_code(resources: &[Resource], modules: &[Vec<Ident>]) -> proc_macro2::TokenStream {
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
        quote! {
            static #slot: __tembra_resource::Slot<#path> = __tembra_resource::Slot::new();
            impl #path {
                #[doc = #doc]
                pub fn with<R>(use_: impl ::core::ops::FnOnce(&mut Self) -> R) -> R {
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
