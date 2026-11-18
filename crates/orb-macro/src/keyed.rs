//! Keyed imports: `import("ns:name/interface@version", keyed)` on the attribute reaches the
//! objects of another Orb's service by key, where `import = "..."` reaches its `main` object.
//!
//! The kernel serves a keyed import as a reference view: an interface of the importer whose one
//! resource has the provider interface's functions as its methods, next to an asynchronous `get`
//! that acquires the object of a key, and an artifact role that maps the view to the provider
//! interface. The macro derives the view from the provider's WIT in the consumed directory, names
//! it after the provider interface followed by `-ref`, imports it in the world, re-exports its
//! generated module under that name and writes the role in the manifest's `references`.
use crate::wit::{anywhere, escaped, unsupported};
use proc_macro2::TokenStream;
use quote::quote;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use syn::{Ident, LitStr, Token};
use wit_parser::{FunctionKind, Resolve, Type, TypeDefKind, TypeOwner};

/// The acquisition function of every view.
const GET: &str = "get";

/// The methods `wit-bindgen` gives every imported resource, the view's included, for its handle.
const HANDLE_METHODS: [&str; 3] = ["handle", "take_handle", "from_handle"];

/// How the attribute writes a keyed import, for the errors.
const USAGE: &str = "an interface imported by key is written import(\"ns:name/interface@version\", keyed); import = \"ns:name/interface@version\" reaches the provider's main object";

/// One keyed import: the provider interface it reaches and the view the world imports for it.
pub(crate) struct View {
    /// The provider interface, as the attribute names it: `ns:name/interface@version`.
    pub target: String,
    /// The view's interface in the Orb's package: the provider interface's name and `-ref`.
    pub name: String,
    /// The view's WIT: an interface of the Orb's package, with its documentation.
    pub text: String,
}

/// Read the parenthesized form of the attribute's `import` option,
/// `import("ns:name/interface@version", keyed)`, and return the identity as written.
pub(crate) fn option(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<LitStr> {
    let content;
    syn::parenthesized!(content in meta.input);
    let identity: LitStr = content.parse()?;
    if content.is_empty() {
        return Err(syn::Error::new(identity.span(), USAGE));
    }
    content.parse::<Token![,]>()?;
    let mode: Ident = content
        .parse()
        .map_err(|error| syn::Error::new(error.span(), USAGE))?;
    if mode != "keyed" {
        return Err(syn::Error::new(mode.span(), USAGE));
    }
    if !content.is_empty() {
        content.parse::<Token![,]>()?;
    }
    if !content.is_empty() {
        return Err(content.error(USAGE));
    }
    Ok(identity)
}

/// The views of the interfaces the attribute imports by key, in the order it names them, whose
/// literals `expand` has read as WIT identities already. Each is another Orb's interface, or one
/// this module implements, a proxy reaching other providers' objects, not WASI's, which the
/// installation grants instead, imported by key once, read from the consumed packages of `dir`,
/// and its view takes a name that neither the world nor another interface of the package, `taken`,
/// has. The view's `use` of an implemented interface's types makes the elaborated world import the
/// interface it exports, for the types alone, which `cargo tembra build` and the kernel tell from a
/// hook, an interface the world imports itself.
pub(crate) fn views(
    imports: &[LitStr],
    dir: Option<&crate::wit::Directory>,
    world: &str,
    taken: &BTreeMap<String, &Ident>,
) -> syn::Result<Vec<View>> {
    if imports.is_empty() {
        return Ok(Vec::new());
    }
    let resolve = dir.map(crate::wit::Directory::packages).transpose()?;
    let mut views: Vec<View> = Vec::new();
    for literal in imports {
        let target = literal.value();
        if crate::wasi::is_wasi(&target) {
            return Err(syn::Error::new(
                literal.span(),
                format!(
                    "{target} is a WASI interface, which the installation grants and no Orb provides: import it with import = \"{target}\""
                ),
            ));
        }
        let (Some(dir), Some(resolve)) = (dir, resolve) else {
            return Err(syn::Error::new(
                literal.span(),
                "importing an interface by key needs wit = \"<dir>\", the directory with its WIT package",
            ));
        };
        let name = view_name(&target);
        let clash = if views.iter().any(|view| view.target == target) {
            Some(format!("{target} is imported by key twice"))
        } else if let Some(view) = views.iter().find(|view| view.name == name) {
            Some(format!(
                "{target} and {} both have their view in the interface `{name}`; import one of them by key",
                view.target
            ))
        } else if name == world {
            Some(format!(
                "the view of {target}, the interface `{name}`, has the name of the world, taken from the module; rename the module"
            ))
        } else {
            taken.get(&name).map(|ident| {
                format!(
                    "the view of {target}, the interface `{name}`, has the name of the interface of `{ident}`; rename `{ident}`"
                )
            })
        };
        if let Some(clash) = clash {
            return Err(syn::Error::new(literal.span(), clash));
        }
        let text = view_text(resolve, dir.path(), &target, &name)
            .map_err(|message| syn::Error::new(literal.span(), message))?;
        views.push(View { target, name, text });
    }
    Ok(views)
}

/// The view's interface name for the provider interface `ns:name/interface@version`.
pub(crate) fn view_name(target: &str) -> String {
    let interface = crate::Identity::of(target).interface.unwrap_or_default();
    format!("{interface}-ref")
}

/// The view's WIT: the provider interface's types its functions name, `use`d from it; a resource
/// of the provider interface's name whose methods are the interface's functions, each `async`
/// exactly when the function is, with its documentation; and `get`, the acquisition the kernel
/// answers with the object of a key or the reason it refused it.
fn view_text(resolve: &Resolve, dir: &Path, target: &str, name: &str) -> Result<String, String> {
    let interface =
        crate::wit::interface(resolve, dir, target).map_err(|error| error.to_string())?;
    let resource = interface.name.clone().unwrap_or_default();
    if interface.functions.is_empty() {
        return Err(format!(
            "{target} has no function: the objects a keyed import reaches have the interface's functions as their methods"
        ));
    }
    let mut spelling = Spelling {
        resolve,
        target,
        used: BTreeSet::new(),
    };
    let mut methods = String::new();
    for function in interface.functions.values() {
        let asynchronous = match function.kind {
            FunctionKind::Freestanding => "",
            FunctionKind::AsyncFreestanding => "async ",
            _ => {
                return Err(format!(
                    "{target} cannot be imported by key: `{}` is not a plain function, and the objects a keyed import reaches have the interface's plain functions as their methods",
                    function.name
                ));
            }
        };
        // rustc would report the name defined twice on the whole attribute, naming neither the
        // import nor the function.
        let method = crate::rust_ident(&function.name).to_string();
        if HANDLE_METHODS.contains(&method.as_str()) {
            return Err(format!(
                "{target} cannot be imported by key: `{}` would be the method `{method}` of the objects a keyed import reaches, which their bindings define already for the object's handle",
                function.name
            ));
        }
        // A method takes its object as `self`, and WIT tells names apart by neither case nor
        // dashes: `SELF` is `self` too.
        if let Some(param) = function
            .params
            .iter()
            .find(|param| param.name.replace('-', "").eq_ignore_ascii_case("self"))
        {
            return Err(format!(
                "{target} cannot be imported by key: `{}` has a parameter `{}`, and as a method of the objects a keyed import reaches it takes its object as `self`",
                function.name, param.name
            ));
        }
        let carried = |problem: Carried| match problem {
            Carried::NoData(problem) => format!(
                "{target} cannot be imported by key: `{}` carries {problem}, and the methods of the objects a keyed import reaches carry data only",
                function.name
            ),
            Carried::Unsupported(problem) => format!(
                "{target} cannot be imported by key: `{}` carries {problem}",
                function.name
            ),
        };
        let params = function
            .params
            .iter()
            .map(|param| {
                Ok(format!(
                    "{}: {}",
                    escaped(&param.name),
                    spelling.spell(param.ty)?
                ))
            })
            .collect::<Result<Vec<_>, Carried>>()
            .map_err(carried)?;
        let result = match function.result {
            Some(ty) => format!(" -> {}", spelling.spell(ty).map_err(carried)?),
            None => String::new(),
        };
        methods.push_str(&docs(function.docs.contents.as_deref(), "    "));
        methods.push_str(&format!(
            "    {}: {asynchronous}func({}){result};\n",
            escaped(&function.name),
            params.join(", ")
        ));
    }
    // A resource among the interface's types, named by no function: the kernel binds an interface
    // with a resource as callable, so the view's `use` of the interface would need a provider of
    // its own, which the installation names for the view alone.
    if let Some((type_name, _)) = interface
        .types
        .iter()
        .find(|(_, id)| is_resource(resolve, **id))
    {
        return Err(format!(
            "{target} cannot be imported by key: it has the resource `{type_name}`, and the objects of a view carry data, so an interface with a resource is not imported by key"
        ));
    }
    // Types and functions share the interface's names: what it uses, its resource and `get`.
    for claimed in [resource.as_str(), GET] {
        if spelling.used.contains(claimed) {
            return Err(format!(
                "{target} names a type `{claimed}`, which its view, the interface `{name}`, gives its {}",
                if claimed == GET {
                    "acquisition function"
                } else {
                    "resource"
                }
            ));
        }
    }
    if resource == GET {
        return Err(format!(
            "{target} is named like the acquisition function `{GET}` of its view, which gives its resource the interface's name"
        ));
    }
    let mut text = format!("/// The objects of `{target}`, each selected by its key.\n");
    text.push_str(&format!("interface {} {{\n", escaped(name)));
    if !spelling.used.is_empty() {
        let used: Vec<String> = spelling.used.iter().map(|used| escaped(used)).collect();
        text.push_str(&format!("  use {target}.{{{}}};\n", used.join(", ")));
    }
    text.push_str(&format!(
        "  /// An object of `{target}`: its methods are the interface's functions.\n"
    ));
    text.push_str(&format!("  resource {} {{\n", escaped(&resource)));
    text.push_str(&methods);
    text.push_str("  }\n");
    text.push_str(
        "  /// The object of `key` under the principal of the call, or why it was refused.\n",
    );
    text.push_str(&format!(
        "  {GET}: async func(key: list<string>) -> result<{}, string>;\n",
        escaped(&resource)
    ));
    text.push_str("}\n");
    Ok(text)
}

/// Whether a type of an interface is a resource, defined there or `use`d from another interface.
fn is_resource(resolve: &Resolve, mut id: wit_parser::TypeId) -> bool {
    loop {
        match &resolve.types[id].kind {
            TypeDefKind::Resource => return true,
            TypeDefKind::Type(Type::Id(inner)) => id = *inner,
            _ => return false,
        }
    }
}

/// A function's documentation as `///` lines at the given indentation, as WIT prints it.
fn docs(contents: Option<&str>, indent: &str) -> String {
    contents
        .into_iter()
        .flat_map(str::lines)
        .map(|line| match line {
            "" => format!("{indent}///\n"),
            line => format!("{indent}/// {line}\n"),
        })
        .collect()
}

/// The WIT spelling of the provider interface's types in the view: a named type by its name,
/// which the view `use`s from the provider interface, anything else by its structure.
struct Spelling<'a> {
    resolve: &'a Resolve,
    target: &'a str,
    used: BTreeSet<String>,
}

/// What a function of the provider interface carries that a view's method cannot: what is no data,
/// which the objects of a view do not carry, or data the runtime refuses in a signature.
enum Carried {
    NoData(String),
    Unsupported(String),
}

impl Spelling<'_> {
    /// Spell a type of a function's signature, or say what it carries that a view's method cannot.
    fn spell(&mut self, ty: Type) -> Result<String, Carried> {
        Ok(match ty {
            Type::Bool => "bool".into(),
            Type::U8 => "u8".into(),
            Type::U16 => "u16".into(),
            Type::U32 => "u32".into(),
            Type::U64 => "u64".into(),
            Type::S8 => "s8".into(),
            Type::S16 => "s16".into(),
            Type::S32 => "s32".into(),
            Type::S64 => "s64".into(),
            Type::F32 => "f32".into(),
            Type::F64 => "f64".into(),
            Type::Char => "char".into(),
            Type::String => "string".into(),
            Type::ErrorContext => return Err(Carried::NoData("an error context".into())),
            Type::Id(id) => {
                let definition = &self.resolve.types[id];
                if let Some(type_name) = &definition.name {
                    let owned = matches!(definition.owner, TypeOwner::Interface(owner)
                        if self.resolve.id_of(owner).as_deref() == Some(self.target));
                    if !owned {
                        return Err(Carried::NoData(format!(
                            "the type `{type_name}`, which {} does not name",
                            self.target
                        )));
                    }
                    if let Some(problem) = no_data(self.resolve, ty) {
                        return Err(Carried::NoData(format!(
                            "{problem} in the type `{type_name}`"
                        )));
                    }
                    if let Some(problem) = unsupported(self.resolve, ty) {
                        return Err(Carried::Unsupported(format!(
                            "the type `{type_name}`, which the runtime refuses in a signature: {problem}"
                        )));
                    }
                    self.used.insert(type_name.clone());
                    return Ok(escaped(type_name));
                }
                let spelled = match &definition.kind {
                    TypeDefKind::List(inner) => format!("list<{}>", self.spell(*inner)?),
                    TypeDefKind::Option(inner) => format!("option<{}>", self.spell(*inner)?),
                    TypeDefKind::Result(result) => match (result.ok, result.err) {
                        (None, None) => "result".into(),
                        (Some(ok), None) => format!("result<{}>", self.spell(ok)?),
                        (None, Some(err)) => format!("result<_, {}>", self.spell(err)?),
                        (Some(ok), Some(err)) => {
                            format!("result<{}, {}>", self.spell(ok)?, self.spell(err)?)
                        }
                    },
                    TypeDefKind::Tuple(tuple) => format!(
                        "tuple<{}>",
                        tuple
                            .types
                            .iter()
                            .map(|ty| self.spell(*ty))
                            .collect::<Result<Vec<_>, _>>()?
                            .join(", ")
                    ),
                    TypeDefKind::Type(inner) => self.spell(*inner)?,
                    _ => {
                        return Err(match no_data(self.resolve, ty) {
                            Some(problem) => Carried::NoData(problem.to_owned()),
                            None => match unsupported(self.resolve, ty) {
                                Some(problem) => Carried::Unsupported(refused(&problem)),
                                None => Carried::NoData("a type without a name".to_owned()),
                            },
                        });
                    }
                };
                // What the runtime refuses of the type itself, its parts spelled: an option of an
                // option.
                if let Some(problem) = unsupported(self.resolve, ty) {
                    return Err(Carried::Unsupported(refused(&problem)));
                }
                spelled
            }
        })
    }
}

/// A type without a name the runtime refuses in a signature, for the errors.
fn refused(problem: &str) -> String {
    format!("a type the runtime refuses in a signature: {problem}")
}

/// What a type holds anywhere that is no data, if anything, as `no_data_itself` finds it in each
/// type.
pub(crate) fn no_data(resolve: &Resolve, ty: Type) -> Option<&'static str> {
    anywhere(resolve, ty, |ty| no_data_itself(resolve, ty))
}

/// Whether a type is itself no data, and what: a resource or a handle to one, a future, a stream
/// or an error context.
pub(crate) fn no_data_itself(resolve: &Resolve, ty: Type) -> Option<&'static str> {
    let id = match ty {
        Type::Id(id) => id,
        Type::ErrorContext => return Some("an error context"),
        _ => return None,
    };
    match &resolve.types[id].kind {
        TypeDefKind::Resource => Some("a resource"),
        TypeDefKind::Handle(_) => Some("a resource handle"),
        TypeDefKind::Future(_) => Some("a future"),
        TypeDefKind::Stream(_) => Some("a stream"),
        _ => None,
    }
}

/// The world's import of a view: its identity in the Orb's package.
fn binding(package: &str, view: &View) -> String {
    crate::identity(package, &view.name)
}

/// The manifest's `references`: each view's binding mapped to the provider interface it reaches.
pub(crate) fn manifest(package: &str, views: &[View]) -> BTreeMap<String, String> {
    views
        .iter()
        .map(|view| (binding(package, view), view.target.clone()))
        .collect()
}

/// The re-export of each view's generated module in the Orb module, under the view's interface
/// name: `counter_ref::get(key)` and the methods of the `counter_ref::Counter` it returns.
pub(crate) fn reexports(package: &str, views: &[View]) -> Vec<TokenStream> {
    views
        .iter()
        .map(|view| {
            let [namespace, package, interface] = crate::hook_path(&binding(package, view));
            quote! { pub use self::#namespace::#package::#interface; }
        })
        .collect()
}
