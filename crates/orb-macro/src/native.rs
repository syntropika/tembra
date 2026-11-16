//! The native side of the module: `orb()`, the Orb a native host installs, written from the same
//! analysis as the component. Every item goes inside `::tembra::__host!`, the facade's hidden
//! macro that expands its input only where the facade links the runtime, so a component build
//! gets none of it and the host crate's `tembra` decides.
//!
//! Each service is registered by a hidden function in the module that declares it, next to its
//! typed client, where the author's types and functions resolve as written, whatever their
//! visibility; `orb()`, in the Orb module, collects the registrations. A method's portable
//! arguments and result cross as the boundary's JSON, the form `tembra_contracts` validates,
//! through a codec written from the WIT types, kind by kind: each type of the module that enters
//! the WIT reads and writes its own JSON through a hidden trait of the Orb module, implemented in
//! the type's module, where its members are visible. What the native side does not cover yet is a
//! compile error inside the same wrapper, naming the item; a component build of the module is
//! unaffected.
use crate::{
    Analysis, Function, Payload, Service, Shape, ShapeKind, camel_ident, metadata::Metadata,
    rust_ident, wit,
};
use heck::{ToShoutySnakeCase, ToSnakeCase};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use std::collections::BTreeMap;
use syn::{Ident, LitStr, Type, spanned::Spanned};
use tembra_contracts::javascript_name;

/// The native output, by the module each part goes in: the Orb module's items, and the items of
/// each sub-module declaring a service or a type that enters the WIT, by scope, which the
/// component's tree renders in place.
pub(crate) struct Native {
    pub root: TokenStream,
    pub nested: Vec<(usize, TokenStream)>,
}

/// Write the native side of the module: `orb()` with the registration of every service, the
/// client of each and the JSON of every type that enters the WIT, or the compile error for what
/// the native side does not cover yet.
pub(crate) fn emit(analysis: &Analysis, metadata: &Metadata) -> Native {
    let registrations = covered(analysis).and_then(|()| {
        analysis
            .services
            .iter()
            .map(|service| registration(service, analysis))
            .collect::<Result<Vec<_>, _>>()
    });
    let registrations = match registrations {
        Ok(registrations) => registrations,
        Err(Refused(error)) => {
            return Native {
                root: wrapped(error),
                nested: Vec::new(),
            };
        }
    };
    let mut by_scope: BTreeMap<usize, Vec<TokenStream>> = BTreeMap::new();
    let mut calls = Vec::new();
    for (service, items) in analysis.services.iter().zip(registrations) {
        let path = &analysis.modules[service.scope];
        let function = service_ident(service);
        // A service its create builds takes the inputs `orb(..)` shares.
        let inputs = service.create.is_some().then(|| quote! { , &inputs });
        calls.push(quote! { self::#(#path::)* #function(name #inputs) });
        by_scope
            .entry(service.scope)
            .or_default()
            .extend([items, client(service)]);
    }
    // The JSON of each type of the module that enters the WIT, in the type's module, once the
    // methods that take or return it are covered.
    let codecs = analysis
        .shapes
        .iter()
        .map(|shape| Ok((shape.scope, shape_codec(shape, analysis)?)))
        .collect::<Result<Vec<_>, Refused>>();
    match codecs {
        Ok(codecs) => {
            for (scope, items) in codecs {
                by_scope.entry(scope).or_default().push(items);
            }
        }
        Err(Refused(error)) => {
            return Native {
                root: wrapped(error),
                nested: Vec::new(),
            };
        }
    }
    let root = by_scope.remove(&0).unwrap_or_default();
    let inputs = analysis
        .inputs
        .as_ref()
        .map(|inputs| crate::from_root(&analysis.modules[inputs.scope], &inputs.ident));
    // The Orb's `#[root]` is its create, which takes the inputs too.
    let create = analysis
        .root
        .as_ref()
        .map(|root| crate::from_root(&analysis.modules[root.scope], &root.ident));
    let shared = create.is_some()
        || analysis
            .services
            .iter()
            .any(|service| service.create.is_some());
    let orb = orb(metadata, inputs.as_ref(), shared, create.as_ref(), &calls);
    let boundary = (!analysis.shapes.is_empty()).then(boundary_trait);
    Native {
        root: wrapped(quote! { #orb #boundary #(#root)* }),
        nested: by_scope
            .into_iter()
            .map(|(scope, items)| (scope, wrapped(quote! { #(#items)* })))
            .collect(),
    }
}

/// The items, inside the facade's wrapper, through the absolute path: the module may hold its own
/// `tembra` module of bindings.
fn wrapped(items: TokenStream) -> TokenStream {
    quote! { ::tembra::__host! { #items } }
}

/// `orb()`, the Orb named and versioned by the manifest, and `orb_named`, which builds it with
/// every service registered under the name and API version a host gives it; `orb()` forwards to
/// it. With an `#[inputs]` struct, `inputs` the path the Orb module names it by, they are
/// `orb(inputs)` and `orb_named(name, api_version, inputs)`, and when a service has a create or
/// the Orb a `#[root]`, `shared`, the inputs go to each as one `Rc`. The root, `create` the path
/// the Orb module names it by, is the Orb's create: `Orb::create` runs it with the inputs and the
/// Orb's declaration.
///
/// Cargo does not rebuild a crate when only `[package.metadata.tembra]` changes, so the generated
/// code includes the manifest, which makes the compiler track the file: the next build then reads
/// the table again.
fn orb(
    metadata: &Metadata,
    inputs: Option<&TokenStream>,
    shared: bool,
    create: Option<&TokenStream>,
    calls: &[TokenStream],
) -> TokenStream {
    let name = &metadata.name;
    let version = metadata.api_version;
    let mut doc = format!(
        " The Orb for a native host: `{name}` at API version {version}, as the crate manifest names it. Install it with `Host::install`; its services are named `{name}::<interface identity>`. `orb_named` builds it under another name or version."
    );
    let mut named = String::from(
        " The Orb for a native host, named `name` at API version `api_version`, for a host that chooses them at run time or installs the crate more than once: its services are named `<name>::<interface identity>`.",
    );
    if inputs.is_some() {
        let lent = " `inputs` are what the host lends every service of the Orb.";
        doc.push_str(lent);
        named.push_str(lent);
    }
    let parameter = inputs.map(|path| quote! { inputs: #path });
    let named_parameter = inputs.map(|path| quote! { , inputs: #path });
    let argument = inputs.map(|_| quote! { , inputs });
    let shared = shared.then(|| quote! { let inputs = ::std::rc::Rc::new(inputs); });
    let create = create.map(|create| {
        quote! {
            .create({
                let inputs = ::std::rc::Rc::clone(&inputs);
                move |declaration| self::#create(&inputs, &declaration)
            })
        }
    });
    quote! {
        const _: &[u8] = ::core::include_bytes!(::core::concat!(
            ::core::env!("CARGO_MANIFEST_DIR"),
            "/Cargo.toml"
        ));

        #[doc = #doc]
        pub fn orb(#parameter) -> ::tembra::Result<::tembra::Orb> {
            orb_named(#name, #version #argument)
        }

        #[doc = #named]
        pub fn orb_named(
            name: &str,
            api_version: u32
            #named_parameter
        ) -> ::tembra::Result<::tembra::Orb> {
            #shared
            ::core::result::Result::Ok(
                ::tembra::Orb::new(name, api_version) #create #(.service(#calls))*
            )
        }
    }
}

/// The name of the trait through which each type of the module that enters the WIT reads and
/// writes its JSON, declared in the Orb module.
const BOUNDARY: &str = "__TembraBoundary";

/// The trait through which each type of the module that enters the WIT reads and writes its JSON:
/// private to the Orb module, so its sub-modules implement it and every codec names it, and no
/// other crate sees it. A type only taken or only returned leaves one function unused.
fn boundary_trait() -> TokenStream {
    let boundary = format_ident!("{BOUNDARY}");
    quote! {
        /// How a type of the module that enters the WIT crosses the boundary: as the JSON a
        /// portable caller sends and receives, which `tembra_contracts` validates.
        #[allow(dead_code)]
        trait #boundary: ::core::marker::Sized {
            /// Read a value from its JSON, or refuse it with a message.
            fn decode(
                value: ::tembra::serde_json::Value,
            ) -> ::core::result::Result<Self, ::std::string::String>;

            /// Write the value as its JSON, or refuse it with a message.
            fn encode(
                self,
            ) -> ::core::result::Result<::tembra::serde_json::Value, ::std::string::String>;
        }
    }
}

/// The trait of `boundary_trait`, as the module `scope` names it.
fn boundary(scope: usize, analysis: &Analysis) -> TokenStream {
    let root = crate::supers(analysis.modules[scope].len());
    let boundary = format_ident!("{BOUNDARY}");
    quote! { #root #boundary }
}

/// What the native side does not cover yet, as the compile error reported where the runtime is
/// linked, at the item.
struct Refused(TokenStream);

fn refused(span: Span, message: &str) -> Refused {
    let message = LitStr::new(message, span);
    Refused(quote_spanned! { span => ::core::compile_error!(#message); })
}

/// Whether the native side covers the module's features; the first it does not is refused. Each
/// has its item of the plan, which lifts the refusal.
fn covered(analysis: &Analysis) -> Result<(), Refused> {
    if analysis.options.context {
        return Err(refused(
            Span::call_site(),
            "the `context` option is not supported natively yet",
        ));
    }
    if let Some(state) = analysis.states.first() {
        return Err(refused(
            state.ident.span(),
            &format!(
                "`{}` is marked #[state]; states are not supported natively yet",
                state.ident
            ),
        ));
    }
    if let Some(resource) = analysis.resources.first() {
        return Err(refused(
            resource.ident.span(),
            &format!(
                "`{}` is marked #[resource]; root resources are not supported natively yet",
                resource.ident
            ),
        ));
    }
    if let Some((import, identity)) = analysis
        .options
        .imports
        .iter()
        .zip(&analysis.imports)
        .next()
    {
        return Err(refused(
            import.span(),
            &format!("the import of {identity} is not supported natively yet"),
        ));
    }
    if let Some(import) = analysis.options.keyed.first() {
        return Err(refused(
            import.span(),
            &format!(
                "the import of {} by key is not supported natively yet",
                import.value()
            ),
        ));
    }
    if let Some(contribution) = analysis.contributions.first() {
        return Err(refused(
            contribution.ident.span(),
            &format!(
                "the contribution to the hook {} is not supported natively yet",
                contribution.hook
            ),
        ));
    }
    if let Some(declaration) = analysis.declared.first() {
        return Err(refused(
            declaration.ident.span(),
            &format!(
                "the declaration of the hook {} is not supported natively yet",
                declaration.hook
            ),
        ));
    }
    if let Some(resource) = analysis.wit_resources.first() {
        return Err(refused(
            resource.ident.span(),
            &format!(
                "`{}` is marked #[wit_resource]; WIT resources are not supported natively yet",
                resource.ident
            ),
        ));
    }
    Ok(())
}

/// The hidden function registering a service, `__tembra_service_ledger` for `Ledger`.
fn service_ident(service: &Service) -> Ident {
    format_ident!(
        "__tembra_service_{}",
        service.ident.to_string().to_snake_case()
    )
}

/// The codec constant of a method, `__TEMBRA_CODEC_COUNT` for `count`, inside the registration of
/// its service: there it names one method of one service, whatever other services are called.
fn codec_ident(function: &Function) -> Ident {
    format_ident!(
        "__TEMBRA_CODEC_{}",
        function.ident.to_string().to_shouty_snake_case()
    )
}

/// The registration of one service, in its own module: the function `orb()` calls, which declares
/// the codec of each method and builds the `Service` named `{orb}::{identity}` with the signature
/// of its interface, the ownership policy of its attribute and one method per `pub fn`, under its
/// WIT name. The method runs `context.check()` first and then the author's function, awaited when
/// it is `async`, with the arguments as a tuple in WIT order. A service with a create takes the
/// inputs `orb(..)` shares, and its factory calls the create with them and the object's
/// declaration: the service it returns is the object's environment, which a method borrowing
/// `&self` is called on. The guard of the attribute, if any, runs first in the factory.
fn registration(service: &Service, analysis: &Analysis) -> Result<TokenStream, Refused> {
    let depth = analysis.modules[service.scope].len();
    let root = crate::supers(depth);
    let signature = crate::signature_ident(service);
    let identity = service.identity(&analysis.package);
    let function = service_ident(service);
    let target = &service.ident;
    let (parameter, environment, shared, build) = match (&service.create, &analysis.inputs) {
        (Some(create), Some(inputs)) => {
            let inputs = crate::relative(depth, &analysis.modules[inputs.scope], &inputs.ident);
            (
                quote! { , inputs: &::std::rc::Rc<#inputs> },
                quote! { #target },
                quote! { let inputs = ::std::rc::Rc::clone(inputs); },
                quote! { #target::#create(&inputs, &declaration) },
            )
        }
        _ => (
            quote! {},
            quote! { () },
            quote! {},
            quote! { ::core::result::Result::Ok(()) },
        ),
    };
    let factory = match (&service.create, guard(service, &identity)) {
        (Some(_), None) => quote! { move |declaration| #build },
        (Some(_), Some(guard)) => quote! { move |declaration| { #guard #build } },
        (None, None) => quote! { |_| { #build } },
        (None, Some(guard)) => quote! { |declaration| { #guard #build } },
    };
    let lifetime = service.policy.as_ref().map(|policy| {
        let lifetime = if policy.host {
            quote! { Host }
        } else {
            quote! { Referenced }
        };
        let linger = policy.linger_ms;
        quote! { .lifetime(::tembra::core::Lifetime::#lifetime, #linger) }
    });
    let mut codecs = Vec::new();
    let mut methods = Vec::new();
    for method in &service.functions {
        let codec = codec_ident(method);
        codecs.push(self::codec(service, method, &codec, analysis)?);
        methods.push(self::method(service, method, &codec));
    }
    let doc = format!(
        " The service `{}` of the native Orb named `orb`, as `orb()` registers it.",
        service.ident
    );
    Ok(quote! {
        #[doc = #doc]
        pub(crate) fn #function(orb: &str #parameter) -> ::tembra::Service<#environment> {
            #(#codecs)*

            #shared
            ::tembra::Service::new(::std::format!("{}::{}", orb, #identity), #factory)
            .signature(::core::option::Option::Some(::std::string::String::from(#root #signature)))
            #lifetime
            #(#methods)*
        }
    })
}

/// The guard of a service's objects, first in its factory, if its attribute has one: an object at
/// another key than `main`, or of another principal, is refused as forbidden, with the reason
/// `key` or `principal` as a hand-written factory gives it, before the create runs. `identity`
/// names the service's interface in the errors.
fn guard(service: &Service, identity: &str) -> Option<TokenStream> {
    let refusal = |reason: &str, message: String| {
        quote! {
            return ::core::result::Result::Err(::tembra::Error::new(
                ::tembra::Code::Forbidden,
                #reason,
                #message
            ));
        }
    };
    let key = service.guard.main.then(|| {
        let refused = refusal("key", format!("the objects of {identity} use the key main"));
        quote! { if address.key != ["main"] { #refused } }
    });
    let principal = service.guard.principal.as_ref().map(|principal| {
        let refused = refusal(
            "principal",
            format!("the objects of {identity} belong to the principal {principal}"),
        );
        quote! { if address.principal != #principal { #refused } }
    });
    (key.is_some() || principal.is_some()).then(|| {
        quote! {
            let address = declaration.address()?;
            #key
            #principal
        }
    })
}

/// The `method_with` call binding one method: the handle takes the arguments as a tuple of the
/// author's parameter types, checks the context and calls the author's function, on the object's
/// environment when it borrows `&self`, answering with its value, or with the `tembra::Result` it
/// returns, whose failure is the call's error.
fn method(service: &Service, function: &Function, codec: &Ident) -> TokenStream {
    let name = wit::kebab(&function.ident);
    let target = &service.ident;
    let ident = &function.ident;
    let params: Vec<&Ident> = function.params.iter().map(|(name, _, _)| name).collect();
    let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
    let awaited = function.asynchronous.then(|| quote! { .await });
    let (environment, call) = if function.receiver {
        (
            quote! { __tembra_self: ::std::rc::Rc<#target> },
            quote! { __tembra_self.#ident(#(#params),*) #awaited },
        )
    } else {
        (
            quote! { _ },
            quote! { #target::#ident(#(#params),*) #awaited },
        )
    };
    // A method returning `tembra::Result<T>` answers with it: its failure is the call's error.
    let body = match (&function.result, function.fallible) {
        (_, true) => call,
        (Some(_), false) => quote! { ::core::result::Result::Ok(#call) },
        (None, false) => quote! { #call; ::core::result::Result::Ok(()) },
    };
    quote! {
        .method_with(
            #name,
            |#environment, __tembra_context: ::tembra::Context, (#(#params,)*): (#(#types,)*)| async move {
                __tembra_context.check()?;
                #body
            },
            #codec,
        )
    }
}

/// The typed client of a service, `MathClient` for `Math`, in the service's module and as visible
/// as its struct: `new` wraps a reference to one of its objects, and each function of the
/// interface is an async method that takes the call's `&Context` first and then the function's
/// parameters, and calls the method's local entrypoint with them as a tuple in WIT order, `()`
/// without parameters, so the compiler checks the types `Reference::call` checks at run time. A
/// function named `new` is `new_` on the client, since `new` builds the client; its WIT name stays
/// `new`. The context's name is hygienic: a parameter of the same name stays the author's.
fn client(service: &Service) -> TokenStream {
    let vis = &service.vis;
    let ident = format_ident!("{}Client", service.ident);
    let context = Ident::new("context", Span::mixed_site());
    let methods = service.functions.iter().map(|function| {
        let name = wit::kebab(&function.ident);
        let method = if function.ident == "new" {
            format_ident!("new_", span = function.ident.span())
        } else {
            function.ident.clone()
        };
        let docs = &function.docs;
        let params: Vec<&Ident> = function.params.iter().map(|(name, _, _)| name).collect();
        let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
        let result = match &function.result {
            Some((ty, _)) => quote! { #ty },
            None => quote! { () },
        };
        quote! {
            #(#[doc = #docs])*
            pub async fn #method(
                &self,
                #context: &::tembra::Context
                #(, #params: #types)*
            ) -> ::tembra::Result<#result> {
                self.reference
                    .call::<(#(#types,)*), #result>(#context, #name, (#(#params,)*))
                    .await
            }
        }
    });
    let doc = format!(
        " A typed client of the service `{}`: one async method per function of its interface, which takes the call's context first and then the function's parameters.",
        service.ident
    );
    quote! {
        #[doc = #doc]
        #vis struct #ident {
            reference: ::std::rc::Rc<::tembra::Reference>,
        }

        impl #ident {
            /// A client of the object `reference` holds.
            pub fn new(reference: ::std::rc::Rc<::tembra::Reference>) -> Self {
                Self { reference }
            }

            #(#methods)*
        }
    }
}

/// The codec of one method: the arguments as the boundary writes them, a JSON array in parameter
/// order, which a method without parameters also takes as `null`, read into the tuple of the
/// author's parameter types; and the result written back, `null` for a method without one. A type
/// the codec does not cover is refused, naming it and where the method takes it.
fn codec(
    service: &Service,
    function: &Function,
    ident: &Ident,
    analysis: &Analysis,
) -> Result<TokenStream, Refused> {
    let place = |role: &str, ty: &Type, uncovered: Uncovered| {
        refused(
            ty.span(),
            &format!(
                "`{}::{}` {role} `{}`{}",
                service.ident,
                function.ident,
                spelled(ty),
                uncovered.described(ty)
            ),
        )
    };
    let boundary = boundary(service.scope, analysis);
    let mut kinds = Vec::new();
    for (_, ty, _) in &function.params {
        kinds.push(
            kind(ty, service.scope, analysis).map_err(|uncovered| place("takes", ty, uncovered))?,
        );
    }
    let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
    let decode = decode_arguments(&kinds, &boundary);
    let (result, encode) = match &function.result {
        Some((ty, _)) => {
            let kind = kind(ty, service.scope, analysis)
                .map_err(|uncovered| place("returns", ty, uncovered))?;
            let encode = encode(&kind, quote! { value }, &boundary);
            (quote! { #ty }, quote! { |value| { #encode } })
        }
        None => (
            quote! { () },
            quote! { |()| ::core::result::Result::Ok(::tembra::serde_json::Value::Null) },
        ),
    };
    Ok(quote! {
        const #ident: ::tembra::Codec<(#(#types,)*), #result> = ::tembra::Codec {
            decode: |value| { #decode },
            encode: #encode,
        };
    })
}

/// A type as the author wrote it, for the errors: `Vec<Point>`.
fn spelled(ty: &Type) -> String {
    quote!(#ty).to_string().replace(' ', "").replace(',', ", ")
}

/// A WIT type as the codec reads and writes it, over the author's Rust type, in the JSON form of
/// the boundary.
enum Kind<'a> {
    /// An unsigned integer, `bool`, `string` or `char`: serde's JSON, 64-bit integers as exact
    /// numbers.
    Plain(&'a Type),
    /// A signed integer: serde's JSON, but `-0`, which the boundary refuses as an integer.
    Signed(&'a Type),
    /// `f32` when `single`, else `f64`: a finite number other than `-0`, which an `f32` reads only
    /// when it narrows to such a number too, and writes as the `f64` it widens to, as a component
    /// writes it.
    Float { ty: &'a Type, single: bool },
    /// `list<t>`: a JSON array, read into the author's list type.
    List(&'a Type, Box<Kind<'a>>),
    /// `option<t>`: `null` or the value; `t` is no option.
    Option(Box<Kind<'a>>),
    /// `tuple<..>`: a JSON array of the elements.
    Tuple(Vec<Kind<'a>>),
    /// `result<ok, err>`: `{"tag": "ok" | "err", "val": ..}`, without `val` for a side `()`.
    Result(Option<Box<Kind<'a>>>, Option<Box<Kind<'a>>>),
    /// A record, an enum, a variant or a flags of the module: its own JSON, through the boundary
    /// trait it implements.
    Named(&'a Type),
}

/// A type the codec does not cover: the type, and what it is and why it is refused, after the
/// type's name and a comma, "a record; records are not supported natively yet".
struct Uncovered<'a> {
    ty: &'a Type,
    reason: String,
}

impl Uncovered<'_> {
    /// The end of the refusal of `outer`, which holds the uncovered type or is it: ", which holds
    /// `Entry`, a record; ...".
    fn described(&self, outer: &Type) -> String {
        let holds = if std::ptr::eq(self.ty, outer) {
            String::new()
        } else {
            format!(", which holds `{}`", spelled(self.ty))
        };
        format!("{holds}, {}", self.reason)
    }
}

/// The kind of a type a method takes or returns, or a member of a type of the module holds,
/// written in the module `scope`; `wit_type` accepted it, so a path names a type with a WIT
/// spelling of its own, or a type of the module or of a consumed interface.
fn kind<'a>(ty: &'a Type, scope: usize, analysis: &Analysis) -> Result<Kind<'a>, Uncovered<'a>> {
    let uncovered = |reason: &str| Uncovered {
        ty,
        reason: reason.to_owned(),
    };
    match ty {
        Type::Tuple(tuple) => Ok(Kind::Tuple(
            tuple
                .elems
                .iter()
                .map(|element| kind(element, scope, analysis))
                .collect::<Result<_, _>>()?,
        )),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return Err(uncovered(
                    "an empty path; such types are not supported natively yet",
                ));
            };
            // The type parameters of `Vec<T>`, `Option<T>` or `Result<T, E>`, as `wit_type`
            // checked them.
            let parameters: Vec<&Type> = match &segment.arguments {
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
            // A side of a `Result` that is `()` carries nothing.
            let side = |ty: &'a Type| match ty {
                Type::Tuple(tuple) if tuple.elems.is_empty() => Ok(None),
                ty => kind(ty, scope, analysis).map(|kind| Some(Box::new(kind))),
            };
            match (segment.ident.to_string().as_str(), parameters.as_slice()) {
                ("u8" | "u16" | "u32" | "u64" | "bool" | "char" | "String", _) => {
                    Ok(Kind::Plain(ty))
                }
                ("i8" | "i16" | "i32" | "i64", _) => Ok(Kind::Signed(ty)),
                ("f32", _) => Ok(Kind::Float { ty, single: true }),
                ("f64", _) => Ok(Kind::Float { ty, single: false }),
                ("Vec", [element]) => Ok(Kind::List(ty, Box::new(kind(element, scope, analysis)?))),
                ("Option", [inner]) => match kind(inner, scope, analysis)? {
                    Kind::Option(_) => Err(uncovered(
                        "a nested option, which the boundary's JSON cannot carry: it writes `None` and `Some(None)` alike, as `null`, so the portable profile refuses it, as `cargo tembra build` does",
                    )),
                    inner => Ok(Kind::Option(Box::new(inner))),
                },
                ("Result", [ok, err]) => Ok(Kind::Result(side(ok)?, side(err)?)),
                ("Vec" | "Option" | "Result", _) => Err(uncovered(
                    "a container without its types; such types are not supported natively yet",
                )),
                _ => named(ty, &segment.ident, scope, analysis),
            }
        }
        _ => Err(uncovered(
            "a type without a WIT spelling; such types are not supported natively yet",
        )),
    }
}

/// The kind of a type of the module or of a consumed interface: a type that enters the WIT from
/// the module reads and writes its own JSON; the others are refused, saying what they are.
fn named<'a>(
    ty: &'a Type,
    ident: &Ident,
    scope: usize,
    analysis: &Analysis,
) -> Result<Kind<'a>, Uncovered<'a>> {
    let reason = if let Some(named) = analysis.consumed_modules.named(ty, scope) {
        format!(
            "a type of the consumed interface {}; consumed types are not supported natively yet",
            named.interface.identity
        )
    } else if analysis.shapes.iter().any(|shape| shape.ident == *ident) {
        return Ok(Kind::Named(ty));
    } else if analysis
        .wit_resources
        .iter()
        .any(|resource| resource.ident == *ident)
    {
        "a resource; resources are not supported natively yet".to_owned()
    } else {
        "a type without a native codec; such types are not supported natively yet".to_owned()
    };
    Err(Uncovered { ty, reason })
}

/// The names of the elements of a JSON array taken apart: `__tembra_0`, `__tembra_1`, ...
fn elements(count: usize) -> Vec<Ident> {
    (0..count)
        .map(|index| format_ident!("__tembra_{index}"))
        .collect()
}

/// Read the arguments, `value`, into the tuple of the parameters: a JSON array of as many
/// elements as there are parameters, or `null` for none.
fn decode_arguments(kinds: &[Kind<'_>], boundary: &TokenStream) -> TokenStream {
    let count = kinds.len();
    let names = elements(count);
    let decoded = kinds
        .iter()
        .zip(&names)
        .map(|(kind, name)| decode(kind, quote! { #name }, boundary));
    let expected = match count {
        0 => "expected no arguments, got {}".to_owned(),
        1 => "expected 1 argument, got {}".to_owned(),
        count => format!("expected {count} arguments, got {{}}"),
    };
    quote! {
        let items = match value {
            ::tembra::serde_json::Value::Array(items) => items,
            ::tembra::serde_json::Value::Null => ::std::vec::Vec::new(),
            _ => {
                return ::core::result::Result::Err(
                    ::std::string::String::from("expected the arguments as a list")
                );
            }
        };
        let [#(#names),*] = <[::tembra::serde_json::Value; #count]>::try_from(items)
            .map_err(|items| ::std::format!(#expected, items.len()))?;
        ::core::result::Result::Ok((#(#decoded?,)*))
    }
}

/// Whether a number, an `f64` or an `f32` expression, is one the boundary carries: finite, and no
/// `-0`, which JSON loses between the two heaps.
fn carried(number: TokenStream) -> TokenStream {
    quote! { (#number.is_finite() && !(#number == 0.0 && #number.is_sign_negative())) }
}

/// Read a value of a kind from `value`, a JSON value: an expression of `Result<T, String>` over
/// the author's type `T`, which the boundary trait, as `boundary` names it there, reads for a type
/// of the module.
fn decode(kind: &Kind<'_>, value: TokenStream, boundary: &TokenStream) -> TokenStream {
    match kind {
        Kind::Plain(ty) => quote! {
            ::tembra::serde_json::from_value::<#ty>(#value).map_err(|error| error.to_string())
        },
        Kind::Signed(ty) => quote! {
            match #value {
                ::tembra::serde_json::Value::Number(number)
                    if number.as_f64().is_some_and(|wide| wide == 0.0 && wide.is_sign_negative()) =>
                {
                    ::core::result::Result::Err(::std::string::String::from(
                        "expected an integer, got -0",
                    ))
                }
                item => ::tembra::serde_json::from_value::<#ty>(item).map_err(|error| error.to_string()),
            }
        },
        Kind::Float { ty, single } => {
            let expected = if *single {
                "expected a finite f32 other than -0, got {}"
            } else {
                "expected a finite f64 other than -0, got {}"
            };
            let wide_carried = carried(quote! { wide });
            let read = if *single {
                let narrow_carried = carried(quote! { narrow });
                quote! {
                    {
                        let narrow = wide as f32;
                        if #narrow_carried {
                            ::core::result::Result::Ok(narrow)
                        } else {
                            ::core::result::Result::Err(::std::format!(#expected, number))
                        }
                    }
                }
            } else {
                quote! { ::core::result::Result::Ok(wide) }
            };
            quote! {
                match #value {
                    ::tembra::serde_json::Value::Number(number) => match number.as_f64() {
                        ::core::option::Option::Some(wide) if #wide_carried => #read,
                        _ => ::core::result::Result::Err(::std::format!(#expected, number)),
                    },
                    item => ::tembra::serde_json::from_value::<#ty>(item).map_err(|error| error.to_string()),
                }
            }
        }
        Kind::List(ty, element) => {
            let element = decode(element, quote! { item }, boundary);
            quote! {
                match #value {
                    ::tembra::serde_json::Value::Array(items) => items
                        .into_iter()
                        .map(|item| #element)
                        .collect::<::core::result::Result<#ty, ::std::string::String>>(),
                    _ => ::core::result::Result::Err(::std::string::String::from("expected a list")),
                }
            }
        }
        Kind::Option(inner) => {
            let inner = decode(inner, quote! { item }, boundary);
            quote! {
                match #value {
                    ::tembra::serde_json::Value::Null => {
                        ::core::result::Result::Ok(::core::option::Option::None)
                    }
                    item => #inner.map(::core::option::Option::Some),
                }
            }
        }
        Kind::Tuple(kinds) => {
            let count = kinds.len();
            let names = elements(count);
            let decoded = kinds
                .iter()
                .zip(&names)
                .map(|(kind, name)| decode(kind, quote! { #name }, boundary));
            let expected = format!("expected a tuple of {count} elements, got {{}}");
            quote! {
                match #value {
                    ::tembra::serde_json::Value::Array(items) => {
                        match <[::tembra::serde_json::Value; #count]>::try_from(items) {
                            ::core::result::Result::Ok([#(#names),*]) => {
                                ::core::result::Result::Ok((#(#decoded?,)*))
                            }
                            ::core::result::Result::Err(items) => {
                                ::core::result::Result::Err(::std::format!(#expected, items.len()))
                            }
                        }
                    }
                    _ => ::core::result::Result::Err(
                        ::std::string::String::from("expected a tuple as a list")
                    ),
                }
            }
        }
        Kind::Result(ok, err) => {
            let side = |side: &Option<Box<Kind<'_>>>, wrap: TokenStream| match side {
                Some(kind) => {
                    let read = decode(kind, quote! { item }, boundary);
                    Read::Carried(quote! { #read.map(#wrap) })
                }
                None => Read::Bare(quote! { ::core::result::Result::Ok(#wrap(())) }),
            };
            tagged_decode(
                value,
                "a result",
                vec![
                    (
                        "ok".to_owned(),
                        side(ok, quote! { ::core::result::Result::Ok }),
                    ),
                    (
                        "err".to_owned(),
                        side(err, quote! { ::core::result::Result::Err }),
                    ),
                ],
            )
        }
        Kind::Named(ty) => quote! { <#ty as #boundary>::decode(#value) },
    }
}

/// Write a value of a kind, `value`, of the author's type, as its JSON: an expression of
/// `Result<Value, String>`, which the boundary trait, as `boundary` names it there, writes for a
/// type of the module.
fn encode(kind: &Kind<'_>, value: TokenStream, boundary: &TokenStream) -> TokenStream {
    match kind {
        Kind::Plain(_) | Kind::Signed(_) => quote! {
            ::tembra::serde_json::to_value(#value).map_err(|error| error.to_string())
        },
        Kind::Float { .. } => {
            let wide_carried = carried(quote! { wide });
            quote! {
                {
                    let wide = ::core::convert::Into::<f64>::into(#value);
                    match ::tembra::serde_json::Number::from_f64(wide) {
                        ::core::option::Option::Some(number) if #wide_carried => {
                            ::core::result::Result::Ok(::tembra::serde_json::Value::Number(number))
                        }
                        _ => ::core::result::Result::Err(::std::format!(
                            "cannot write {}: the boundary carries finite numbers other than -0",
                            wide
                        )),
                    }
                }
            }
        }
        Kind::List(_, element) => {
            let element = encode(element, quote! { item }, boundary);
            quote! {
                #value
                    .into_iter()
                    .map(|item| #element)
                    .collect::<::core::result::Result<
                        ::std::vec::Vec<::tembra::serde_json::Value>,
                        ::std::string::String
                    >>()
                    .map(::tembra::serde_json::Value::Array)
            }
        }
        Kind::Option(inner) => {
            let inner = encode(inner, quote! { item }, boundary);
            quote! {
                match #value {
                    ::core::option::Option::None => {
                        ::core::result::Result::Ok(::tembra::serde_json::Value::Null)
                    }
                    ::core::option::Option::Some(item) => #inner,
                }
            }
        }
        Kind::Tuple(kinds) => {
            let names = elements(kinds.len());
            let encoded = kinds
                .iter()
                .zip(&names)
                .map(|(kind, name)| encode(kind, quote! { #name }, boundary));
            let written = written();
            quote! {
                {
                    let (#(#names,)*) = #value;
                    #written(::tembra::serde_json::Value::Array(::std::vec![#(#encoded?),*]))
                }
            }
        }
        Kind::Result(ok, err) => {
            let side = |side: &Option<Box<Kind<'_>>>, tag: &str, wrap: TokenStream| match side {
                Some(kind) => {
                    let written = encode(kind, quote! { item }, boundary);
                    let tagged = tagged_encode(tag, Some(quote! { #written? }));
                    quote! { #wrap(item) => #tagged }
                }
                None => {
                    let tagged = tagged_encode(tag, None);
                    quote! { #wrap(()) => #tagged }
                }
            };
            let ok = side(ok, "ok", quote! { ::core::result::Result::Ok });
            let err = side(err, "err", quote! { ::core::result::Result::Err });
            quote! { match #value { #ok, #err } }
        }
        Kind::Named(ty) => quote! { <#ty as #boundary>::encode(#value) },
    }
}

/// `Ok` of what writing a value evaluates to, `Result<Value, String>`, spelled whole: `?` on an
/// expression that never fails needs its error type.
fn written() -> TokenStream {
    quote! { ::core::result::Result::<::tembra::serde_json::Value, ::std::string::String>::Ok }
}

/// How the value of a tagged case is read: from its payload, `item`, or from nothing.
enum Read {
    /// An expression of `Result<T, String>` over `item`, the case's payload.
    Carried(TokenStream),
    /// An expression of `Result<T, String>` for a case that carries nothing.
    Bare(TokenStream),
}

/// Read a value tagged as the boundary writes a variant or a result from `value`: an object of
/// its case's `tag` and, when the case carries one, its payload, `val`, and nothing else. `what`
/// names the type for the errors, "a result". An expression of `Result<T, String>`.
fn tagged_decode(value: TokenStream, what: &str, cases: Vec<(String, Read)>) -> TokenStream {
    let expected = format!("expected {what} as an object of a tag and its value");
    let arms = cases.iter().map(|(tag, read)| match read {
        Read::Carried(read) => {
            let bare = format!("the case `{tag}` carries a value, `val`");
            quote! {
                (#tag, ::core::option::Option::Some(item)) => #read,
                (#tag, ::core::option::Option::None) => {
                    ::core::result::Result::Err(::std::string::String::from(#bare))
                }
            }
        }
        Read::Bare(read) => {
            let carried = format!("the case `{tag}` carries no value");
            quote! {
                (#tag, ::core::option::Option::None) => #read,
                (#tag, ::core::option::Option::Some(_)) => {
                    ::core::result::Result::Err(::std::string::String::from(#carried))
                }
            }
        }
    });
    quote! {
        match #value {
            ::tembra::serde_json::Value::Object(mut object) => {
                let tag = object.remove("tag");
                let payload = object.remove("val");
                match (tag, payload, object.keys().next()) {
                    (_, _, ::core::option::Option::Some(key)) => ::core::result::Result::Err(
                        ::std::format!("unexpected field `{}`", key)
                    ),
                    (
                        ::core::option::Option::Some(::tembra::serde_json::Value::String(tag)),
                        payload,
                        ::core::option::Option::None,
                    ) => match (tag.as_str(), payload) {
                        #(#arms)*
                        (other, _) => ::core::result::Result::Err(
                            ::std::format!("unknown case `{}`", other)
                        ),
                    },
                    _ => ::core::result::Result::Err(::std::string::String::from(#expected)),
                }
            }
            _ => ::core::result::Result::Err(::std::string::String::from(#expected)),
        }
    }
}

/// The JSON of a tagged case, `{"tag": .., "val": ..}`, the payload left out for a case that
/// carries nothing: an expression of `Result<Value, String>`, where `payload` is an expression of
/// `Value`.
fn tagged_encode(tag: &str, payload: Option<TokenStream>) -> TokenStream {
    let payload = payload.map(|payload| {
        quote! { (::std::string::String::from("val"), #payload), }
    });
    let written = written();
    quote! {
        #written(::tembra::serde_json::Value::Object(
            ::core::iter::FromIterator::from_iter([
                (
                    ::std::string::String::from("tag"),
                    ::tembra::serde_json::Value::String(::std::string::String::from(#tag)),
                ),
                #payload
            ])
        ))
    }
}

/// A member of a record, or of the named fields of a variant's case: the Rust field holding it,
/// its key in the JSON object, the lowerCamelCase form of its WIT name, and its kind.
struct Member<'a> {
    field: Ident,
    key: String,
    kind: Kind<'a>,
}

/// Read a record from `value`, a JSON object of exactly its members' keys, into `built`, the path
/// of the struct or of the case with named fields: an expression of `Result<T, String>`, inside a
/// function returning that type, which a member the object misses returns from.
fn record_decode(
    value: TokenStream,
    built: TokenStream,
    members: &[Member<'_>],
    boundary: &TokenStream,
) -> TokenStream {
    let fields = members.iter().map(|Member { field, key, kind }| {
        let missing = format!("missing the field `{key}`");
        let read = decode(kind, quote! { item }, boundary);
        quote! {
            #field: match object.remove(#key) {
                ::core::option::Option::Some(item) => #read?,
                ::core::option::Option::None => {
                    return ::core::result::Result::Err(::std::string::String::from(#missing));
                }
            },
        }
    });
    quote! {
        match #value {
            ::tembra::serde_json::Value::Object(mut object) => {
                let record = #built { #(#fields)* };
                match object.keys().next() {
                    ::core::option::Option::Some(key) => ::core::result::Result::Err(
                        ::std::format!("unexpected field `{}`", key)
                    ),
                    ::core::option::Option::None => ::core::result::Result::Ok(record),
                }
            }
            _ => ::core::result::Result::Err(::std::string::String::from(
                "expected a record as an object",
            )),
        }
    }
}

/// Write a record whose members' values are bound to `bound`, in order, as a JSON object of their
/// keys: an expression of `Result<Value, String>`.
fn record_encode(bound: &[Ident], members: &[Member<'_>], boundary: &TokenStream) -> TokenStream {
    let entries = bound.iter().zip(members).map(|(name, member)| {
        let key = &member.key;
        let written = encode(&member.kind, quote! { #name }, boundary);
        quote! { (::std::string::String::from(#key), #written?), }
    });
    let written = written();
    quote! {
        #written(::tembra::serde_json::Value::Object(
            ::core::iter::FromIterator::from_iter([#(#entries)*])
        ))
    }
}

/// The kind of a member of the type of the module `shape`, written in its module, or its refusal,
/// after `place`, which names the member: "the field `count` of `Entry` is".
fn member<'a>(
    ty: &'a Type,
    place: &str,
    shape: &Shape,
    analysis: &Analysis,
) -> Result<Kind<'a>, Refused> {
    kind(ty, shape.scope, analysis).map_err(|uncovered| {
        refused(
            ty.span(),
            &format!("{place} `{}`{}", spelled(ty), uncovered.described(ty)),
        )
    })
}

/// The members of a record of `shape`, or of the record of one of its cases, `owner` for the
/// errors: each in the Rust field the author names, when the type stays the author's, or the one
/// the bindings name after its WIT name.
fn members<'a>(
    fields: &'a [(Ident, Type)],
    owner: &str,
    authored: bool,
    shape: &Shape,
    analysis: &Analysis,
) -> Result<Vec<Member<'a>>, Refused> {
    fields
        .iter()
        .map(|(field, ty)| {
            let name = wit::kebab(field);
            Ok(Member {
                field: if authored {
                    field.clone()
                } else {
                    rust_ident(&name)
                },
                key: javascript_name(&name),
                kind: member(
                    ty,
                    &format!("the field `{field}` of `{owner}` is"),
                    shape,
                    analysis,
                )?,
            })
        })
        .collect()
}

/// The JSON of a type of the module that enters the WIT, as its impl of the boundary trait, in its
/// own module, where its members are visible: a record as an object of its fields, keyed by the
/// lowerCamelCase form of their WIT names; an enum as the WIT name of its case; a variant as an
/// object of its case's `tag` and payload, `val`, the record of a case with named fields as an
/// object too; a flags as an object of the flags set, each `true`, which reads a flag left out as
/// unset. The members and cases are named as the bindings generate them, but those of a type the
/// macro converts and of a flags, which stay the author's. A member the codec does not cover is
/// refused, naming it.
fn shape_codec(shape: &Shape, analysis: &Analysis) -> Result<TokenStream, Refused> {
    let ident = &shape.ident;
    let boundary = boundary(shape.scope, analysis);
    let converted = analysis.converted.contains_key(&ident.to_string());
    let member = |ty, place: String| member(ty, &place, shape, analysis);
    let members =
        |fields, owner: String, authored| members(fields, &owner, authored, shape, analysis);
    let (decode, encode) = match &shape.kind {
        ShapeKind::Record(fields) => {
            let members = members(fields, ident.to_string(), converted)?;
            let bound = elements(members.len());
            let fields = members.iter().map(|member| &member.field);
            let decode = record_decode(quote! { value }, quote! { Self }, &members, &boundary);
            let encode = record_encode(&bound, &members, &boundary);
            (
                decode,
                quote! { let Self { #(#fields: #bound),* } = self; #encode },
            )
        }
        ShapeKind::Enum(cases) => {
            let tags: Vec<String> = cases.iter().map(wit::kebab).collect();
            let cases: Vec<Ident> = tags.iter().map(|tag| camel_ident(tag)).collect();
            let decode = quote! {
                match value {
                    ::tembra::serde_json::Value::String(case) => match case.as_str() {
                        #(#tags => ::core::result::Result::Ok(Self::#cases),)*
                        other => ::core::result::Result::Err(
                            ::std::format!("unknown case `{}`", other)
                        ),
                    },
                    _ => ::core::result::Result::Err(::std::string::String::from(
                        "expected an enum case as a string",
                    )),
                }
            };
            let encode = quote! {
                ::core::result::Result::Ok(::tembra::serde_json::Value::String(
                    ::std::string::String::from(match self { #(Self::#cases => #tags,)* })
                ))
            };
            (decode, encode)
        }
        ShapeKind::Variant(cases) => {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            for (case, payload) in cases {
                let tag = wit::kebab(case);
                let named = if converted {
                    case.clone()
                } else {
                    camel_ident(&tag)
                };
                match payload {
                    Payload::None => {
                        let read = quote! { ::core::result::Result::Ok(Self::#named) };
                        reads.push((tag.clone(), Read::Bare(read)));
                        let written = tagged_encode(&tag, None);
                        writes.push(quote! { Self::#named => #written });
                    }
                    Payload::Unnamed(ty) => {
                        let kind = member(ty, format!("the case `{ident}::{case}` carries"))?;
                        let read = decode(&kind, quote! { item }, &boundary);
                        reads.push((
                            tag.clone(),
                            Read::Carried(quote! { #read.map(Self::#named) }),
                        ));
                        let written = encode(&kind, quote! { item }, &boundary);
                        let written = tagged_encode(&tag, Some(quote! { #written? }));
                        writes.push(quote! { Self::#named(item) => #written });
                    }
                    // Only in an enum the macro converts: the author's case, with its fields.
                    Payload::Named(fields) => {
                        let members = members(fields, format!("{ident}::{case}"), true)?;
                        let read = record_decode(
                            quote! { item },
                            quote! { Self::#named },
                            &members,
                            &boundary,
                        );
                        reads.push((tag.clone(), Read::Carried(read)));
                        let bound = elements(members.len());
                        let fields = members.iter().map(|member| &member.field);
                        let record = record_encode(&bound, &members, &boundary);
                        let written = tagged_encode(&tag, Some(quote! { #record? }));
                        writes.push(quote! { Self::#named { #(#fields: #bound),* } => #written });
                    }
                }
            }
            let decode = tagged_decode(quote! { value }, "a variant", reads);
            (decode, quote! { match self { #(#writes,)* } })
        }
        ShapeKind::Flags(item) => {
            let fields: Vec<&Ident> = item
                .fields
                .iter()
                .filter_map(|field| field.ident.as_ref())
                .collect();
            let keys: Vec<String> = fields
                .iter()
                .map(|field| javascript_name(&wit::kebab(field)))
                .collect();
            let decode = quote! {
                match value {
                    ::tembra::serde_json::Value::Object(object) => {
                        let mut flags = Self { #(#fields: false),* };
                        for (key, set) in object {
                            let flag = match key.as_str() {
                                #(#keys => &mut flags.#fields,)*
                                _ => {
                                    return ::core::result::Result::Err(
                                        ::std::format!("unknown flag `{}`", key)
                                    );
                                }
                            };
                            match set {
                                ::tembra::serde_json::Value::Bool(set) => *flag = set,
                                _ => {
                                    return ::core::result::Result::Err(::std::format!(
                                        "expected the flag `{}` as a boolean",
                                        key
                                    ));
                                }
                            }
                        }
                        ::core::result::Result::Ok(flags)
                    }
                    _ => ::core::result::Result::Err(::std::string::String::from(
                        "expected flags as an object of booleans",
                    )),
                }
            };
            let encode = quote! {
                let mut flags = ::tembra::serde_json::Map::new();
                #(
                    if self.#fields {
                        flags.insert(
                            ::std::string::String::from(#keys),
                            ::tembra::serde_json::Value::Bool(true),
                        );
                    }
                )*
                ::core::result::Result::Ok(::tembra::serde_json::Value::Object(flags))
            };
            (decode, encode)
        }
    };
    Ok(quote! {
        impl #boundary for #ident {
            fn decode(
                value: ::tembra::serde_json::Value,
            ) -> ::core::result::Result<Self, ::std::string::String> {
                #decode
            }

            fn encode(
                self,
            ) -> ::core::result::Result<::tembra::serde_json::Value, ::std::string::String> {
                #encode
            }
        }
    })
}
