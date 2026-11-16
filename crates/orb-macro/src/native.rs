//! The native side of the module: `orb()`, the Orb a native host installs, written from the same
//! analysis as the component. Every item goes inside `::tembra::__host!`, the facade's hidden
//! macro that expands its input only where the facade links the runtime, so a component build
//! gets none of it and the host crate's `tembra` decides.
//!
//! Each service is registered by a hidden function in the module that declares it, where the
//! author's types and functions resolve as written, whatever their visibility; `orb()`, in the Orb
//! module, collects them. A method's portable arguments and result cross as the boundary's JSON
//! through a codec written from the WIT types, kind by kind. What the native side does not cover
//! yet is a compile error inside the same wrapper, naming the item; a component build of the
//! module is unaffected.
use crate::{Analysis, Function, Service, ShapeKind, metadata::Metadata, wit};
use heck::{ToShoutySnakeCase, ToSnakeCase};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use std::collections::BTreeMap;
use syn::{Ident, LitStr, Type, spanned::Spanned};

/// The native output, by the module each part goes in: the Orb module's items, and the items of
/// each sub-module declaring a service, by scope, which the component's tree renders in place.
pub(crate) struct Native {
    pub root: TokenStream,
    pub nested: Vec<(usize, TokenStream)>,
}

/// Write the native side of the module: `orb()` with the registration of every service, or the
/// compile error for what the native side does not cover yet.
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
        calls.push(quote! { self::#(#path::)* #function(name) });
        by_scope.entry(service.scope).or_default().push(items);
    }
    let root = by_scope.remove(&0).unwrap_or_default();
    let inputs = analysis
        .inputs
        .as_ref()
        .map(|inputs| crate::from_root(&analysis.modules[inputs.scope], &inputs.ident));
    let orb = orb(metadata, inputs.as_ref(), &calls);
    Native {
        root: wrapped(quote! { #orb #(#root)* }),
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

/// `orb()`: the Orb named and versioned by the manifest, with every service registered. With an
/// `#[inputs]` struct, `inputs` the path the Orb module names it by, it is `orb(inputs)`.
///
/// Cargo does not rebuild a crate when only `[package.metadata.tembra]` changes, so the generated
/// code includes the manifest, which makes the compiler track the file: the next build then reads
/// the table again.
fn orb(metadata: &Metadata, inputs: Option<&TokenStream>, calls: &[TokenStream]) -> TokenStream {
    let name = &metadata.name;
    let version = metadata.api_version;
    let mut doc = format!(
        " The Orb for a native host: `{name}` at API version {version}, as the crate manifest names it. Install it with `Host::install`; its services are named `{name}::<interface identity>`."
    );
    let parameter = inputs.map(|path| {
        doc.push_str(" `inputs` are what the host lends every service of the Orb.");
        quote! { inputs: #path }
    });
    quote! {
        const _: &[u8] = ::core::include_bytes!(::core::concat!(
            ::core::env!("CARGO_MANIFEST_DIR"),
            "/Cargo.toml"
        ));

        #[doc = #doc]
        pub fn orb(#parameter) -> ::tembra::Result<::tembra::Orb> {
            let name = #name;
            ::core::result::Result::Ok(::tembra::Orb::new(name, #version) #(.service(#calls))*)
        }
    }
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

/// The codec constant of a method, `__TEMBRA_CODEC_LEDGER_COUNT` for `Ledger::count`.
fn codec_ident(service: &Service, function: &Function) -> Ident {
    format_ident!(
        "__TEMBRA_CODEC_{}_{}",
        service.ident.to_string().to_shouty_snake_case(),
        function.ident.to_string().to_shouty_snake_case()
    )
}

/// The registration of one service, in its own module: the codec of each method and the function
/// `orb()` calls, which builds the `Service` named `{orb}::{identity}` with the signature of its
/// interface, the ownership policy of its attribute and one method per `pub fn`, under its WIT
/// name. The method runs `context.check()` first and then the author's function, awaited when it
/// is `async`, with the arguments as a tuple in WIT order.
fn registration(service: &Service, analysis: &Analysis) -> Result<TokenStream, Refused> {
    let root = crate::supers(analysis.modules[service.scope].len());
    let signature = crate::signature_ident(service);
    let identity = service.identity(&analysis.package);
    let function = service_ident(service);
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
        let codec = codec_ident(service, method);
        codecs.push(self::codec(service, method, &codec, analysis)?);
        methods.push(self::method(service, method, &codec));
    }
    let doc = format!(
        " The service `{}` of the native Orb named `orb`, as `orb()` registers it.",
        service.ident
    );
    Ok(quote! {
        #(#codecs)*

        #[doc = #doc]
        pub(crate) fn #function(orb: &str) -> ::tembra::Service<()> {
            ::tembra::Service::new(::std::format!("{}::{}", orb, #identity), |_| {
                ::core::result::Result::Ok(())
            })
            .signature(::core::option::Option::Some(::std::string::String::from(#root #signature)))
            #lifetime
            #(#methods)*
        }
    })
}

/// The `method_with` call binding one method: the handle takes the arguments as a tuple of the
/// author's parameter types, checks the context and calls the author's function.
fn method(service: &Service, function: &Function, codec: &Ident) -> TokenStream {
    let name = wit::kebab(&function.ident);
    let target = &service.ident;
    let ident = &function.ident;
    let params: Vec<&Ident> = function.params.iter().map(|(name, _, _)| name).collect();
    let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
    let awaited = function.asynchronous.then(|| quote! { .await });
    let call = quote! { #target::#ident(#(#params),*) #awaited };
    let body = match &function.result {
        Some(_) => quote! { ::core::result::Result::Ok(#call) },
        None => quote! { #call; ::core::result::Result::Ok(()) },
    };
    quote! {
        .method_with(
            #name,
            |_, __tembra_context: ::tembra::Context, (#(#params,)*): (#(#types,)*)| async move {
                __tembra_context.check()?;
                #body
            },
            #codec,
        )
    }
}

/// The codec of one method: the arguments as the boundary writes them, a JSON array in parameter
/// order, which a method without parameters also takes as `null`, read into the tuple of the
/// author's parameter types; and the result written back, `null` for a method without one. A type
/// the codec does not cover yet is refused, naming it and where the method takes it.
fn codec(
    service: &Service,
    function: &Function,
    ident: &Ident,
    analysis: &Analysis,
) -> Result<TokenStream, Refused> {
    let place = |role: &str, ty: &Type, uncovered: Uncovered| {
        let holds = if std::ptr::eq(uncovered.ty, ty) {
            String::new()
        } else {
            format!(", which holds `{}`", spelled(uncovered.ty))
        };
        refused(
            ty.span(),
            &format!(
                "`{}::{}` {role} `{}`{holds}, {}; {} are not supported natively yet",
                service.ident,
                function.ident,
                spelled(ty),
                uncovered.what,
                uncovered.plural
            ),
        )
    };
    let mut kinds = Vec::new();
    for (_, ty, _) in &function.params {
        kinds.push(
            kind(ty, service.scope, analysis).map_err(|uncovered| place("takes", ty, uncovered))?,
        );
    }
    let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
    let decode = decode_arguments(&kinds);
    let (result, encode) = match &function.result {
        Some((ty, _)) => {
            let kind = kind(ty, service.scope, analysis)
                .map_err(|uncovered| place("returns", ty, uncovered))?;
            let encode = encode(&kind, quote! { value });
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

/// A WIT type as the codec reads and writes it, over the author's Rust type. Each kind has its
/// JSON form on the boundary; the ones here are the kinds whose form is serde's own.
enum Kind<'a> {
    /// A number, `bool`, `string` or `char`: serde's JSON, 64-bit integers as exact numbers.
    Plain(&'a Type),
    /// `list<t>`: a JSON array, read into the author's list type.
    List(&'a Type, Box<Kind<'a>>),
    /// `option<t>`: `null` or the value.
    Option(Box<Kind<'a>>),
    /// `tuple<..>`: a JSON array of the elements.
    Tuple(Vec<Kind<'a>>),
}

/// A type the codec does not cover yet: the type and what it is, "a record" and "records".
struct Uncovered<'a> {
    ty: &'a Type,
    what: String,
    plural: &'static str,
}

/// The kind of a type a method takes or returns, written in the module `scope`; `wit_type`
/// accepted it, so a path names a type with a WIT spelling of its own, or a type of the module or
/// of a consumed interface.
fn kind<'a>(ty: &'a Type, scope: usize, analysis: &Analysis) -> Result<Kind<'a>, Uncovered<'a>> {
    let uncovered = |what: &str, plural: &'static str| Uncovered {
        ty,
        what: what.to_owned(),
        plural,
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
                return Err(uncovered("an empty path", "such types"));
            };
            // The one type parameter of `Vec<T>` or `Option<T>`, as `wit_type` checked it.
            let parameter = || match &segment.arguments {
                syn::PathArguments::AngleBracketed(arguments) => {
                    arguments.args.iter().find_map(|argument| match argument {
                        syn::GenericArgument::Type(ty) => Some(ty),
                        _ => None,
                    })
                }
                _ => None,
            };
            match segment.ident.to_string().as_str() {
                "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64"
                | "bool" | "char" | "String" => Ok(Kind::Plain(ty)),
                "Vec" => match parameter() {
                    Some(element) => Ok(Kind::List(ty, Box::new(kind(element, scope, analysis)?))),
                    None => Err(uncovered("a list without its type", "such types")),
                },
                "Option" => match parameter() {
                    Some(inner) => Ok(Kind::Option(Box::new(kind(inner, scope, analysis)?))),
                    None => Err(uncovered("an option without its type", "such types")),
                },
                "Result" => Err(uncovered("a result", "results")),
                _ => Err(classified(ty, &segment.ident, scope, analysis)),
            }
        }
        _ => Err(uncovered("a type without a WIT spelling", "such types")),
    }
}

/// What a type of the module or of a consumed interface is, for the refusal: a record, an enum, a
/// variant or a flags of the module, a `#[wit_resource]` struct, or a type a consumed interface's
/// module names.
fn classified<'a>(ty: &'a Type, ident: &Ident, scope: usize, analysis: &Analysis) -> Uncovered<'a> {
    let (what, plural) = if let Some(named) = analysis.consumed_modules.named(ty, scope) {
        (
            format!(
                "a type of the consumed interface {}",
                named.interface.identity
            ),
            "consumed types",
        )
    } else if let Some(shape) = analysis.shapes.iter().find(|shape| shape.ident == *ident) {
        let (what, plural) = match shape.kind {
            ShapeKind::Record(_) => ("a record", "records"),
            ShapeKind::Enum(_) => ("an enum", "enums"),
            ShapeKind::Variant(_) => ("a variant", "variants"),
            ShapeKind::Flags(_) => ("a flags", "flags"),
        };
        (what.to_owned(), plural)
    } else if analysis
        .wit_resources
        .iter()
        .any(|resource| resource.ident == *ident)
    {
        ("a resource".to_owned(), "resources")
    } else {
        ("a type without a native codec".to_owned(), "such types")
    };
    Uncovered { ty, what, plural }
}

/// The names of the elements of a JSON array taken apart: `__tembra_0`, `__tembra_1`, ...
fn elements(count: usize) -> Vec<Ident> {
    (0..count)
        .map(|index| format_ident!("__tembra_{index}"))
        .collect()
}

/// Read the arguments, `value`, into the tuple of the parameters: a JSON array of as many
/// elements as there are parameters, or `null` for none.
fn decode_arguments(kinds: &[Kind<'_>]) -> TokenStream {
    let count = kinds.len();
    let names = elements(count);
    let decoded = kinds
        .iter()
        .zip(&names)
        .map(|(kind, name)| decode(kind, quote! { #name }));
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

/// Read a value of a kind from `value`, a JSON value: an expression of
/// `Result<T, String>` over the author's type `T`.
fn decode(kind: &Kind<'_>, value: TokenStream) -> TokenStream {
    match kind {
        Kind::Plain(ty) => quote! {
            ::tembra::serde_json::from_value::<#ty>(#value).map_err(|error| error.to_string())
        },
        Kind::List(ty, element) => {
            let element = decode(element, quote! { item });
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
            let inner = decode(inner, quote! { item });
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
                .map(|(kind, name)| decode(kind, quote! { #name }));
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
    }
}

/// Write a value of a kind, `value`, of the author's type, as its JSON: an expression of
/// `Result<Value, String>`.
fn encode(kind: &Kind<'_>, value: TokenStream) -> TokenStream {
    match kind {
        Kind::Plain(_) => quote! {
            ::tembra::serde_json::to_value(#value).map_err(|error| error.to_string())
        },
        Kind::List(_, element) => {
            let element = encode(element, quote! { item });
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
            let inner = encode(inner, quote! { item });
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
                .map(|(kind, name)| encode(kind, quote! { #name }));
            quote! {
                {
                    let (#(#names,)*) = #value;
                    ::core::result::Result::Ok(
                        ::tembra::serde_json::Value::Array(::std::vec![#(#encoded?),*])
                    )
                }
            }
        }
    }
}
