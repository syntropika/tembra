//! The native side of the module: `orb()`, the Orb a native host installs, written from the same
//! analysis as the component. Every item goes inside `::tembra::__host!`, the facade's hidden
//! macro that expands its input only where the facade links the runtime, so a component build
//! gets none of it and the host crate's `tembra` decides. Every path through the facade names it
//! as the crate depends on it, `crate = ".."` on the attribute, `tembra` without it.
//!
//! Each service is registered by a hidden function in the module that declares it, next to its
//! typed client, where the author's types and functions resolve as written, whatever their
//! visibility, as a function of a hidden type of the Orb module, through which `orb()` collects
//! the registrations whatever the visibility of the modules between. A method's portable
//! arguments and result cross as the boundary's JSON, the form `tembra_contracts` validates,
//! through a codec written from the WIT types, kind by kind: each type of the module that enters
//! the WIT reads and writes its own JSON through a hidden trait of the Orb module, implemented in
//! the type's module, where its members are visible, and each data type of an interface the module
//! implements through the same trait, implemented in the Orb module from the interface's WIT for
//! the bindings' type. A `#[wit_resource]` struct is issued as a capability of its resource type:
//! a function whose result holds one issues it into the call's result, as a portable method
//! without a local entrypoint, and each method of the resource reads its `self` back from the
//! call; the resource's functions are registered from its own module, with the handle type a
//! native caller holds the resource as, whose methods are the resource's, and the calls of its
//! constructor and static functions on the service's client. With the `context` option,
//! the Orb module gets a native `context`, whose functions read the call each method's future runs
//! for. A service whose objects own a `#[state]` declares it in its factory, and its methods run in
//! the scope of their object, whose state the state's `read` and `update` reach natively. The Orb's
//! create declares each `#[resource]` of its root, into a table `orb()` builds for the Orb, and
//! every method runs in the scope of its Orb, whose resources their `with` reaches natively. An
//! import of another Orb's interface whose functions are all `async func` calls the provider the
//! host names through the kernel, and a contribution joins the plan of the hook of the Orb the
//! host names, see `providers`. What the native side does not cover yet leaves the module
//! compiling, with an `orb()` that answers an error naming the item, so only a host that builds
//! the Orb fails; a component build of the module is unaffected.
use crate::{
    Analysis, Function, Newtype, Payload, Resource, Root, Service, Shape, ShapeKind, WitResource,
    camel_ident, foreign::Role, metadata::Metadata, rust_ident, wit,
};
use heck::{ToShoutySnakeCase, ToSnakeCase};
use proc_macro2::{Literal, Span, TokenStream};
use quote::{format_ident, quote};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};
use syn::{Ident, Type};
use tembra_contracts::javascript_name;
use wit_parser::{TypeDefKind, TypeId};

mod providers;

/// The native output, by the module each part goes in: the Orb module's items, and the items of
/// each sub-module declaring a service or a type that enters the WIT, by scope, which the
/// component's tree renders in place. `served` names the modules of the imports it writes in the
/// Orb module, in place of the bindings' import modules, which the component's re-exports leave to
/// a component build.
pub(crate) struct Native {
    pub root: TokenStream,
    pub nested: Vec<(usize, TokenStream)>,
    pub served: BTreeSet<String>,
}

/// Write the native side of the module: `orb()` with the registration of every service, the
/// client of each and the JSON of every type that enters the WIT; for what the native side does not
/// cover yet, an `orb()` that answers the error naming it; or the compile error for a manifest whose
/// `metadata` could not be read. With the `context` option, the native `context` too, which the
/// author's code calls either way.
pub(crate) fn emit(
    analysis: &Analysis,
    metadata: Result<&Metadata, &syn::Error>,
    contract: Result<&tembra_contracts::Contract, &syn::Error>,
) -> Native {
    let facade = &analysis.options.facade();
    let context = analysis.options.context.then(|| context_module(facade));
    // The items the native side writes in place of the Orb, beside `context`.
    let alone = |items: TokenStream| Native {
        root: wrapped(facade, quote! { #items #context }),
        nested: Vec::new(),
        served: BTreeSet::new(),
    };
    // Only `orb()` needs the Orb's name and API version, so a manifest the macro cannot read fails
    // a native build alone.
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) => return alone(error.to_compile_error()),
    };
    let inputs = analysis
        .inputs
        .as_ref()
        .map(|inputs| crate::from_root(&analysis.modules[inputs.scope], &inputs.ident));
    let refusal =
        |Refused(message)| alone(unsupported(metadata, inputs.as_ref(), &message, facade));
    // The interfaces the module imports from other Orbs and the hooks it contributes to, which
    // `orb(..)` takes the providers of.
    let consumed = providers::imports(analysis, contract).and_then(|imports| {
        covered(analysis)?;
        Ok((imports, providers::contributions(analysis, contract)?))
    });
    let (imports, contributions) = match consumed {
        Ok(consumed) => consumed,
        Err(refused) => return refusal(refused),
    };
    let imported = !imports.is_empty();
    let registrations = analysis
        .services
        .iter()
        .map(|service| {
            Ok([
                registration(service, analysis, imported)?,
                client(service, analysis)?,
            ])
        })
        .collect::<Result<Vec<_>, _>>();
    let registrations = match registrations {
        Ok(registrations) => registrations,
        Err(refused) => return refusal(refused),
    };
    let mut by_scope: BTreeMap<usize, Vec<TokenStream>> = BTreeMap::new();
    let mut calls = Vec::new();
    // The resources of the Orb's root, which `orb(..)` keeps for the Orb, for the methods of every
    // service, and so are its imports.
    let resources = !analysis.resources.is_empty();
    // Each registration is a function of the registry of the module it is written in.
    let registry = format_ident!("{REGISTRY}");
    for (service, items) in analysis.services.iter().zip(registrations) {
        let registered = Literal::usize_unsuffixed(service.scope);
        let function = service_ident(service);
        // A service its create builds takes the inputs `orb(..)` shares.
        let inputs = service.create.is_some().then(|| quote! { , &inputs });
        let table = resources.then(|| quote! { , &resources });
        let imports = imported.then(|| quote! { , &imports });
        calls.push(quote! {
            self::#registry::<#registered>::#function(name #inputs #table #imports)
        });
        by_scope.entry(service.scope).or_default().extend(items);
    }
    // Each contribution, which `orb(..)` adds to the Orb after its services, with the Orb that
    // declares the hook, from its module.
    let mut contributed = Vec::new();
    for hook in &contributions {
        let scope = hook.contribution.scope;
        let registered = Literal::usize_unsuffixed(scope);
        let function = providers::contribution_ident(hook.contribution);
        let field = &hook.field;
        let table = resources.then(|| quote! { , &resources });
        let imports = imported.then(|| quote! { , &imports });
        contributed.push(quote! {
            self::#registry::<#registered>::#function(orb, &providers.#field.name #table #imports)?
        });
        by_scope
            .entry(scope)
            .or_default()
            .push(providers::contribution(hook, analysis, imported));
    }
    // The Orb's `#[root]` is its create, which takes the inputs too, called by a function of the
    // registry of its module.
    let create = analysis.root.as_ref().map(|root| {
        let registered = Literal::usize_unsuffixed(root.scope);
        by_scope
            .entry(root.scope)
            .or_default()
            .push(root_call(root, analysis));
        quote! { #registry::<#registered>::__tembra_root }
    });
    // The JSON of each type of the module that enters the WIT, and of each newtype the WIT spells
    // as its field, in the type's module, once the methods that take or return it are covered, and
    // the functions of each WIT resource, in the resource's module.
    let codecs = analysis
        .shapes
        .iter()
        .map(|shape| Ok((shape.scope, shape_codec(shape, analysis)?)))
        .chain(
            analysis
                .newtypes
                .iter()
                .map(|(newtype, _)| Ok((newtype.scope, newtype_codec(newtype, analysis)?))),
        )
        .chain(
            analysis
                .wit_resources
                .iter()
                .map(|resource| Ok((resource.scope, resource_glue(resource, analysis, imported)?))),
        )
        .collect::<Result<Vec<_>, Refused>>();
    match codecs {
        Ok(codecs) => {
            for (scope, items) in codecs {
                by_scope.entry(scope).or_default().push(items);
            }
        }
        Err(refused) => return refusal(refused),
    }
    let root = by_scope.remove(&0).unwrap_or_default();
    // The JSON of each data type of an implemented interface, in the Orb module, from its WIT.
    let consumed: Vec<TokenStream> = analysis
        .consumed_types
        .data_types()
        .into_iter()
        .filter_map(|id| consumed_codec(id, &analysis.consumed_types, facade))
        .collect();
    let shared = create.is_some()
        || analysis
            .services
            .iter()
            .any(|service| service.create.is_some());
    let orb = orb(
        metadata,
        inputs.as_ref(),
        shared,
        create.as_ref(),
        resources,
        &imports,
        &calls,
        &contributed,
        facade,
    );
    // The registries, for an Orb that registers a service, adds a contribution or has a root.
    let declared_registry = (!calls.is_empty() || !contributed.is_empty() || create.is_some())
        .then(|| {
            quote! {
                /// The registry of each module of the Orb, by its scope: its functions register the
                /// services of the module, add its contributions and call the Orb's root, written
                /// there, where the items resolve as written, and `orb()` reaches them through it,
                /// whatever the visibility of the module.
                struct #registry<const SCOPE: usize>;
            }
        });
    let boundary =
        (!analysis.shapes.is_empty() || !analysis.newtypes.is_empty() || !consumed.is_empty())
            .then(|| boundary_trait(facade));
    // The providers `orb(..)` takes and what the Orb requires of them, and the table of the
    // imports and the module of each, in place of the bindings'.
    let provided = (imported || !contributions.is_empty()).then(|| {
        let providers = providers::providers(&imports, &contributions, facade);
        let requirements = providers::requirements(&imports, &contributions, facade);
        quote! { #providers #requirements }
    });
    let imported = imported.then(|| {
        let table = providers::table(&imports, facade);
        let modules = providers::modules(&imports, analysis);
        quote! { #table #modules }
    });
    Native {
        root: wrapped(
            facade,
            quote! { #orb #declared_registry #provided #imported #boundary #(#root)* #(#consumed)* #context },
        ),
        nested: by_scope
            .into_iter()
            .map(|(scope, items)| (scope, wrapped(facade, quote! { #(#items)* })))
            .collect(),
        served: imports
            .iter()
            .map(|import| import.field.to_string())
            .collect(),
    }
}

/// The items, inside the wrapper of the facade named `facade`, through the absolute path: the module
/// may hold its own `tembra` module of bindings.
fn wrapped(facade: &Ident, items: TokenStream) -> TokenStream {
    quote! { ::#facade::__host! { #items } }
}

/// `orb()`, the Orb named and versioned by the manifest, and `orb_named`, which builds it with
/// every service registered under the name and API version a host gives it; `orb()` forwards to
/// it. With an `#[inputs]` struct, `inputs` the path the Orb module names it by, they are
/// `orb(inputs)` and `orb_named(name, api_version, inputs)`, and when a service has a create or
/// the Orb a `#[root]`, `shared`, the inputs go to each as one `Rc`. The root, `create` the path
/// of the function of its module's registry that calls it, is the Orb's create: `Orb::create` runs
/// it with the inputs and the Orb's declaration. With root `resources`, `orb_named` builds the table of the Orb's resources,
/// which the Orb's create fills, declaring each before the root runs, and which every service
/// takes, as one `Rc`. With `imports`, they take `providers` last, the Orb requires each provider,
/// and `orb_named` builds the table of the Orb's imports, which every service takes too, as one
/// `Rc`, and whose plain imports the Orb's create declares first. With `contributions`, the calls
/// adding each to the Orb, which it makes after the services, they take `providers` too, and the
/// Orb requires each hook of the Orb that declares it.
///
/// Cargo does not rebuild a crate when only `[package.metadata.tembra]` changes, so the generated
/// code includes the manifest, which makes the compiler track the file: the next build then reads
/// the table again. The runtime's types are reached through the facade named `facade`.
#[allow(clippy::too_many_arguments)]
fn orb(
    metadata: &Metadata,
    inputs: Option<&TokenStream>,
    shared: bool,
    create: Option<&TokenStream>,
    resources: bool,
    imports: &[providers::Import<'_>],
    calls: &[TokenStream],
    contributions: &[TokenStream],
    facade: &Ident,
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
    let imported = !imports.is_empty();
    let consumes = imported || !contributions.is_empty();
    if consumes {
        let provided = " `providers` name the Orbs it consumes from: the one answering each interface it imports, and the one declaring each hook it contributes to.";
        doc.push_str(provided);
        named.push_str(provided);
    }
    let parameters = inputs
        .map(|path| quote! { inputs: #path })
        .into_iter()
        .chain(consumes.then(|| quote! { providers: Providers }));
    let parameter = quote! { #(#parameters),* };
    let named_parameter = inputs.map(|path| quote! { , inputs: #path });
    let named_providers = consumes.then(|| quote! { , providers: Providers });
    let argument = inputs.map(|_| quote! { , inputs });
    let provided = consumes.then(|| quote! { , providers });
    let shared = shared.then(|| quote! { let inputs = ::std::rc::Rc::new(inputs); });
    let table = resources.then(|| {
        quote! {
            let resources = ::std::rc::Rc::new(
                <self::__tembra_resource::Resources as ::core::default::Default>::default(),
            );
        }
    });
    // The Orb requires each provider, and its services and its create share the table of its
    // imports, whose plain ones the create declares before anything else.
    let imports_table = imported.then(|| {
        quote! {
            let imports = ::std::rc::Rc::new(self::__tembra_import::Imports::new(&providers));
        }
    });
    let built = if consumes {
        quote! {
            {
                let mut orb = ::#facade::Orb::new(name, api_version);
                for requirement in self::__tembra_providers::requirements(&providers)? {
                    orb = orb.requires(requirement);
                }
                orb
            }
        }
    } else {
        quote! { ::#facade::Orb::new(name, api_version) }
    };
    let declared = imports.iter().any(|import| import.view.is_none());
    let create = if declared {
        let shares = create.map(|_| quote! { let inputs = ::std::rc::Rc::clone(&inputs); });
        let holds = resources.then(|| quote! { let resources = ::std::rc::Rc::clone(&resources); });
        let opens = resources.then(|| quote! { resources.declare(&declaration)?; });
        let root = match create {
            Some(create) => quote! { self::#create(&inputs, &declaration) },
            None => quote! { ::core::result::Result::Ok(()) },
        };
        Some(quote! {
            .create({
                let imports = ::std::rc::Rc::clone(&imports);
                #shares
                #holds
                move |declaration| {
                    imports.declare(&declaration)?;
                    #opens
                    #root
                }
            })
        })
    } else {
        match (create, resources) {
            (Some(create), false) => Some(quote! {
                .create({
                    let inputs = ::std::rc::Rc::clone(&inputs);
                    move |declaration| self::#create(&inputs, &declaration)
                })
            }),
            (Some(create), true) => Some(quote! {
                .create({
                    let inputs = ::std::rc::Rc::clone(&inputs);
                    let resources = ::std::rc::Rc::clone(&resources);
                    move |declaration| {
                        resources.declare(&declaration)?;
                        self::#create(&inputs, &declaration)
                    }
                })
            }),
            (None, true) => Some(quote! {
                .create({
                    let resources = ::std::rc::Rc::clone(&resources);
                    move |declaration| resources.declare(&declaration)
                })
            }),
            (None, false) => None,
        }
    };
    // The contributions come after the services, each added to the Orb built so far.
    let built = if contributions.is_empty() {
        quote! { ::core::result::Result::Ok(#built #create #(.service(#calls))*) }
    } else {
        quote! {
            let orb = #built #create #(.service(#calls))*;
            #(let orb = #contributions;)*
            ::core::result::Result::Ok(orb)
        }
    };
    quote! {
        const _: &[u8] = ::core::include_bytes!(::core::concat!(
            ::core::env!("CARGO_MANIFEST_DIR"),
            "/Cargo.toml"
        ));

        #[doc = #doc]
        pub fn orb(#parameter) -> ::#facade::Result<::#facade::Orb> {
            orb_named(#name, #version #argument #provided)
        }

        #[doc = #named]
        pub fn orb_named(
            name: &str,
            api_version: u32
            #named_parameter
            #named_providers
        ) -> ::#facade::Result<::#facade::Orb> {
            #shared
            #table
            #imports_table
            #built
        }
    }
}

/// The name of the trait through which each type of the module that enters the WIT reads and
/// writes its JSON, declared in the Orb module.
const BOUNDARY: &str = "__TembraBoundary";

/// The name of the type of the Orb module whose functions register its services, add its
/// contributions and call its root, one type per module, by its scope, so that each module keeps
/// its own names: each function is written in its item's module, and `orb()` reaches it through
/// the type.
const REGISTRY: &str = "__TembraRegistry";

/// The function of the registry of the root's module that calls the Orb's `#[root]`, as the author
/// wrote it, with its parameters and its result: `orb_named` calls the root through it, whatever
/// the visibility of the module.
fn root_call(root: &Root, analysis: &Analysis) -> TokenStream {
    let supers = crate::supers(analysis.modules[root.scope].len());
    let registry = format_ident!("{REGISTRY}");
    let registered = Literal::usize_unsuffixed(root.scope);
    let Root {
        ident,
        params,
        output,
        ..
    } = root;
    let [inputs, declaration] = params.as_slice() else {
        unreachable!("a root takes the inputs and the declaration");
    };
    let doc = format!(
        " The Orb's root, `{ident}`, which `orb_named` calls through the registry of its module."
    );
    quote! {
        impl #supers #registry<#registered> {
            #[doc = #doc]
            pub(crate) fn __tembra_root(
                __tembra_inputs: #inputs,
                __tembra_declaration: #declaration,
            ) #output {
                self::#ident(__tembra_inputs, __tembra_declaration)
            }
        }
    }
}

/// The trait through which each type of the module that enters the WIT reads and writes its JSON:
/// private to the Orb module, so its sub-modules implement it and every codec names it, and no
/// other crate sees it. A type only taken or only returned leaves one function unused. Its JSON is
/// that of the facade named `facade`.
fn boundary_trait(facade: &Ident) -> TokenStream {
    let boundary = format_ident!("{BOUNDARY}");
    quote! {
        /// How a type of the module that enters the WIT crosses the boundary: as the JSON a
        /// portable caller sends and receives, which `tembra_contracts` validates.
        #[allow(dead_code)]
        trait #boundary: ::core::marker::Sized {
            /// Read a value from its JSON, or refuse it with a message.
            fn decode(
                value: ::#facade::serde_json::Value,
            ) -> ::core::result::Result<Self, ::std::string::String>;

            /// Write the value as its JSON, or refuse it with a message.
            fn encode(
                self,
            ) -> ::core::result::Result<::#facade::serde_json::Value, ::std::string::String>;
        }
    }
}

/// `context` with the `context` option, in place of the component's, with the functions and
/// types of the kernel's interface: each function reads the call of the method being polled, which
/// the method's scope sets around each poll of its future, as the executor answers a component
/// from the call it serves. The runtime's types are reached through the facade named `facade`.
fn context_module(facade: &Ident) -> TokenStream {
    quote! {
        /// The kernel's view of the call a method serves: its principal and cancellation, the
        /// address of the object running it, and the host's log.
        pub mod context {
            pub use super::tembra::guest::context::{Address, Level};

            /// The call of the method being polled on this thread, with the principal it was
            /// admitted with.
            struct Call {
                context: ::#facade::Context,
                principal: ::std::string::String,
            }

            ::std::thread_local! {
                static CALL: ::core::cell::RefCell<::core::option::Option<::std::rc::Rc<Call>>> =
                    const { ::core::cell::RefCell::new(::core::option::Option::None) };
            }

            /// Run a method's future in the scope of its call: while it is polled, the functions
            /// of this module read that call, and an outer poll's call comes back after.
            pub(crate) fn __scoped<F: ::core::future::Future>(
                context: ::#facade::Context,
                future: F,
            ) -> impl ::core::future::Future<Output = F::Output> {
                // Read as the call starts: the call keeps this principal once its root is
                // cancelled, when the root no longer answers it.
                let principal = context.principal().unwrap_or_default();
                let call = ::std::rc::Rc::new(Call { context, principal });
                let mut future = ::std::boxed::Box::pin(future);
                ::core::future::poll_fn(move |cx| {
                    let _entered = Entered::enter(call.clone());
                    ::core::future::Future::poll(future.as_mut(), cx)
                })
            }

            /// The call set for one poll, which puts the previous one back when dropped, also when
            /// the poll panics.
            struct Entered(::core::option::Option<::std::rc::Rc<Call>>);

            impl Entered {
                fn enter(call: ::std::rc::Rc<Call>) -> Self {
                    Self(CALL.with(|current| current.replace(::core::option::Option::Some(call))))
                }
            }

            impl ::core::ops::Drop for Entered {
                fn drop(&mut self) {
                    let previous = self.0.take();
                    CALL.with(|current| current.replace(previous));
                }
            }

            /// The call of the method running, which only a method of this Orb has.
            fn current() -> ::std::rc::Rc<Call> {
                CALL.with(|current| current.borrow().clone())
                    .expect("the kernel's context is read inside a method of the Orb")
            }

            /// The address of the object running the current call, or an empty key and the
            /// principal `host` in a hook contribution, which runs in the Orb's root, as in a
            /// component.
            pub fn object() -> Address {
                match current().context.address() {
                    ::core::result::Result::Ok(address) => Address {
                        key: address.key,
                        principal: address.principal,
                    },
                    ::core::result::Result::Err(_) => Address {
                        key: ::std::vec::Vec::new(),
                        principal: ::std::string::String::from("host"),
                    },
                }
            }

            /// The principal of the current call, the one it was admitted with, also once
            /// cancelled.
            pub fn principal() -> ::std::string::String {
                current().principal.clone()
            }

            /// Whether the current call was cancelled.
            pub fn cancelled() -> bool {
                current().context.signal().is_cancelled()
            }

            /// Cancel the current call: the caller and this Orb both observe it cancelled.
            /// Cancelling a cancelled call changes nothing, as in a component: its caller may
            /// have cancelled it alone, and its root goes on.
            pub fn cancel() {
                let call = current();
                if call.context.signal().is_cancelled() {
                    return;
                }
                call.context
                    .cancel()
                    .expect("the root of a running call is retained");
            }

            /// Write a diagnostic record attributed to this Orb, for the host's log observer.
            pub fn log(level: Level, message: &str) {
                let level = match level {
                    Level::Log => "log",
                    Level::Debug => "debug",
                    Level::Info => "info",
                    Level::Warn => "warn",
                    Level::Error => "error",
                };
                current()
                    .context
                    .log(level, message)
                    .expect("a running call writes for its Orb");
            }
        }
    }
}

/// The kernel's state natively: `read` and `update` of the component's `__tembra_state`, in place
/// of those the bindings' import serves, which the state types' accessors call in both builds.
/// Each reaches the state of the object whose method is being polled, which the method's scope
/// sets around each poll of its future from the object's environment, where the service's factory
/// keeps the state it declared. The state keeps the JSON a component's keeps for the same value:
/// the text `serde_json` writes, which the module's own `storable` checks reads back, read as the
/// executor reads a component's, every negative zero written as zero. As in a component, a value
/// the rule refuses never reaches the kernel, `update` answers a call cancelled before the kernel
/// stored its value with the value kept, and a reload reads the value it inherits as the state's
/// type and runs the rule on it. The runtime's types are reached through the facade named `facade`.
pub(crate) fn state_access(facade: &Ident) -> TokenStream {
    let items = quote! {
        /// The object a method runs on, for its state: the state its factory declared, and the
        /// call, which the kernel checks before it stores a value.
        struct Object {
            context: ::#facade::Context,
            state: ::#facade::State<::#facade::serde_json::Value>,
        }

        ::std::thread_local! {
            static OBJECT: ::core::cell::RefCell<::core::option::Option<::std::rc::Rc<Object>>> =
                const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }

        /// Run a method's future on its object: while it is polled, `read` and `update` reach the
        /// object's state, and an outer poll's object comes back after.
        pub(crate) fn scoped<F: ::core::future::Future>(
            context: ::#facade::Context,
            state: ::#facade::State<::#facade::serde_json::Value>,
            future: F,
        ) -> impl ::core::future::Future<Output = F::Output> {
            let object = ::std::rc::Rc::new(Object { context, state });
            let mut future = ::std::boxed::Box::pin(future);
            ::core::future::poll_fn(move |cx| {
                let _entered = Entered::enter(object.clone());
                ::core::future::Future::poll(future.as_mut(), cx)
            })
        }

        /// The object set for one poll, which puts the previous one back when dropped, also when
        /// the poll panics.
        struct Entered(::core::option::Option<::std::rc::Rc<Object>>);

        impl Entered {
            fn enter(object: ::std::rc::Rc<Object>) -> Self {
                Self(OBJECT.with(|current| current.replace(::core::option::Option::Some(object))))
            }
        }

        impl ::core::ops::Drop for Entered {
            fn drop(&mut self) {
                let previous = self.0.take();
                OBJECT.with(|current| current.replace(previous));
            }
        }

        /// The object of the method running, which only a method of a service that owns a state
        /// has.
        fn current() -> ::std::rc::Rc<Object> {
            OBJECT
                .with(|current| current.borrow().clone())
                .expect("a state is read and updated inside a method of the service that owns it")
        }

        /// The state of the object running the current call.
        pub fn read<T: ::serde::de::DeserializeOwned>() -> T {
            let value = current()
                .state
                .value()
                .expect("the kernel keeps the state of an object running a call");
            parse(&value.to_string())
        }

        /// Store a value and return the stored one, or the refusal of a value whose JSON would
        /// not read back, which never reaches the kernel. The kernel refuses the value of a call
        /// cancelled since it started and keeps the previous one, which is then the answer; any
        /// other refusal panics, as it traps a component.
        pub fn update<T: ::serde::Serialize + ::serde::de::DeserializeOwned>(
            value: &T,
            name: &str,
        ) -> ::core::result::Result<T, String> {
            let json = storable(value, name)?;
            let object = current();
            let stored = match object
                .state
                .update(&object.context, |_| ::core::result::Result::Ok(kept(&json)))
            {
                ::core::result::Result::Ok(stored) => stored,
                ::core::result::Result::Err(_) if object.context.signal().is_cancelled() => object
                    .state
                    .value()
                    .expect("the kernel keeps the state of an object running a call"),
                ::core::result::Result::Err(error) => {
                    ::core::panic!("the kernel refused the `{}` state: {}", name, error)
                }
            };
            ::core::result::Result::Ok(parse(&stored.to_string()))
        }

        /// Declare the state of an object of the service that owns it, for its factory: the JSON
        /// of the state's `Default`, at the state's version, which a reload checks is a `T` the
        /// rule accepts before it adopts it. A `Default` whose JSON would not read back fails the
        /// object's creation.
        pub(crate) fn declare<T>(
            declaration: &::#facade::Declaration,
            version: u32,
            name: &'static str,
            rule: impl ::core::ops::Fn(&T) -> ::core::result::Result<(), String> + 'static,
        ) -> ::#facade::Result<::#facade::State<::#facade::serde_json::Value>>
        where
            T: ::core::default::Default
                + ::serde::Serialize
                + ::serde::de::DeserializeOwned
                + 'static,
        {
            let json = storable(&T::default(), name)
                .map_err(|refusal| ::#facade::Error::invalid("state", refusal))?;
            let state = declaration.state(kept(&json), version, |_| ::core::result::Result::Ok(()))?;
            let retained = state.clone();
            declaration.validate_on_reload(move || {
                let value = retained.value()?;
                let accepted = match ::serde_json::from_str::<T>(&value.to_string()) {
                    ::core::result::Result::Ok(state) => rule(&state),
                    ::core::result::Result::Err(error) => ::core::result::Result::Err(
                        format!("the retained state is not a `{}`: {}", name, error),
                    ),
                };
                accepted.map_err(|refusal| {
                    ::#facade::Error::invalid("state", format!("the state was refused: {}", refusal))
                })
            })?;
            ::core::result::Result::Ok(state)
        }

        /// The value the kernel keeps for a state's JSON, every negative zero written as zero, as
        /// the executor keeps a component's: the platform hosts' value profile has none.
        fn kept(json: &str) -> ::#facade::serde_json::Value {
            let mut value = ::#facade::serde_json::from_str(json)
                .expect("the JSON of a state reads back as JSON");
            unsigned_zeros(&mut value);
            value
        }

        fn unsigned_zeros(value: &mut ::#facade::serde_json::Value) {
            match value {
                ::#facade::serde_json::Value::Number(number) => {
                    if number
                        .as_f64()
                        .is_some_and(|float| float == 0.0 && float.is_sign_negative())
                    {
                        *value = ::#facade::serde_json::Value::from(0);
                    }
                }
                ::#facade::serde_json::Value::Array(items) => {
                    items.iter_mut().for_each(unsigned_zeros);
                }
                ::#facade::serde_json::Value::Object(fields) => {
                    fields.values_mut().for_each(unsigned_zeros);
                }
                ::#facade::serde_json::Value::Null
                | ::#facade::serde_json::Value::Bool(_)
                | ::#facade::serde_json::Value::String(_) => {}
            }
        }
    };
    wrapped(facade, items)
}

/// The resources of the Orb's root natively, in the component's `__tembra_resource`: the table of
/// one Orb's resources, `Resources`, which `orb(..)` builds and the Orb's create fills, declaring
/// each with `Declaration::resource` before the `#[root]` runs; the scope each method's future runs
/// in, set around each of its polls with the table of its Orb; and `with` on each resource type, in
/// place of the component's, which reaches the resource of the Orb whose method is running. The
/// host opens each resource once the create returns, once for the Orb, and closes it when it
/// removes the Orb; the value lives in a `Slot`, as in a component, filled by the `#[open]`
/// function and emptied, through the `#[close]` one, if any, as the host closes it. Before the host
/// opens it and once it has closed it, or outside a method of the Orb, `with` panics, as it traps a
/// component. After a reload to another module whose resource keeps its name and version, the host
/// keeps the old code's value, of another Rust type, which `with` could not reach: the scope
/// answers each call with the runtime's refusal of it, `invalid/rust_type`, before the method runs.
/// The runtime's types are reached through the facade named `facade`.
pub(crate) fn resource_access(
    resources: &[Resource],
    modules: &[Vec<Ident>],
    facade: &Ident,
) -> TokenStream {
    let fields: Vec<Ident> = resources
        .iter()
        .map(|resource| rust_ident(&wit::kebab(&resource.ident)))
        .collect();
    // The types as this module, one level below the Orb module, names them.
    let paths: Vec<TokenStream> = resources
        .iter()
        .map(|resource| crate::relative(1, &modules[resource.scope], &resource.ident))
        .collect();
    let declarations =
        resources
            .iter()
            .zip(&fields)
            .zip(&paths)
            .map(|((resource, field), path)| {
                let name = wit::kebab(&resource.ident);
                let version = resource.version;
                let open = resource
                    .open
                    .as_ref()
                    .expect("a resource has its #[open] function");
                let close = match &resource.close {
                    Some(close) => quote! {
                        if let ::core::option::Option::Some(value) = slot.close() {
                            value.#close();
                        }
                    },
                    None => quote! { slot.close(); },
                };
                quote! {
                    let resource = declaration.resource(
                        #name,
                        #version,
                        || {
                            let slot = Slot::new();
                            slot.open(#path::#open());
                            ::core::result::Result::Ok(slot)
                        },
                        |slot, _| {
                            #close
                            ::std::boxed::Box::pin(async { ::core::result::Result::Ok(()) })
                        },
                    )?;
                    let _ = self.#field.set(resource);
                }
            });
    let accessors = resources
        .iter()
        .zip(&fields)
        .zip(&paths)
        .map(|((resource, field), path)| {
            let name = wit::kebab(&resource.ident);
            let doc = format!(
                " Use the resource `{name}` of this Orb's root, which the host opened with the root, in a method of the Orb: the closure gets the open value, which lives in the host."
            );
            let open = format!("the resource `{name}` of the Orb's root is open");
            quote! {
                impl #path {
                    #[doc = #doc]
                    pub fn with<R>(use_: impl ::core::ops::FnOnce(&mut Self) -> R) -> R {
                        let slot = current()
                            .#field
                            .get()
                            .and_then(|resource| resource.value().ok())
                            .expect(#open);
                        slot.with(use_)
                    }
                }
            }
        });
    let items = quote! {
        /// The resources of the root of one Orb `orb(..)` builds, which its create declares.
        #[derive(Default)]
        pub struct Resources {
            #(#fields: ::core::cell::OnceCell<::#facade::Resource<Slot<#paths>>>,)*
        }

        impl Resources {
            /// Declare each resource of the root, in the Orb's create: the host opens it with the
            /// `#[open]` function once the create returns, and closes it when it removes the Orb.
            pub(crate) fn declare(
                &self,
                declaration: &::#facade::Declaration,
            ) -> ::#facade::Result<()> {
                #(#declarations)*
                ::core::result::Result::Ok(())
            }

            /// The runtime's refusal of a value the host keeps for a resource of the table as
            /// another Rust type, `invalid/rust_type`, as it refuses a hand-written Orb that reads
            /// the value as its type: a reload keeps a resource whose name and version stay the
            /// same, and the struct of another module is another type. A resource the host has not
            /// opened, or is closing, is left to `with`.
            fn typed(&self) -> ::#facade::Result<()> {
                #(
                    if let ::core::option::Option::Some(error) = self
                        .#fields
                        .get()
                        .and_then(|resource| resource.value().err())
                        .filter(|error| error.reason == ::#facade::core::Reason::RustType.as_str())
                    {
                        return ::core::result::Result::Err(error);
                    }
                )*
                ::core::result::Result::Ok(())
            }
        }

        ::std::thread_local! {
            static RESOURCES: ::core::cell::RefCell<
                ::core::option::Option<::std::rc::Rc<Resources>>
            > = const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }

        /// Run a method's future for its Orb: while it is polled, `with` reaches the resources of
        /// that Orb, and an outer poll's come back after. A call to code whose resource the host
        /// keeps as a value of another Rust type answers the runtime's refusal instead, before the
        /// future runs.
        pub(crate) fn scoped<T, F: ::core::future::Future<Output = ::#facade::Result<T>>>(
            resources: ::std::rc::Rc<Resources>,
            future: F,
        ) -> impl ::core::future::Future<Output = ::#facade::Result<T>> {
            let mut future = ::std::boxed::Box::pin(future);
            async move {
                resources.typed()?;
                ::core::future::poll_fn(|cx| {
                    let _entered = Entered::enter(resources.clone());
                    ::core::future::Future::poll(future.as_mut(), cx)
                })
                .await
            }
        }

        /// The resources set for one poll, which puts the previous ones back when dropped, also
        /// when the poll panics.
        struct Entered(::core::option::Option<::std::rc::Rc<Resources>>);

        impl Entered {
            fn enter(resources: ::std::rc::Rc<Resources>) -> Self {
                Self(RESOURCES.with(|current| {
                    current.replace(::core::option::Option::Some(resources))
                }))
            }
        }

        impl ::core::ops::Drop for Entered {
            fn drop(&mut self) {
                let previous = self.0.take();
                RESOURCES.with(|current| current.replace(previous));
            }
        }

        /// The resources of the Orb whose method is running, which only a method of the Orb has.
        fn current() -> ::std::rc::Rc<Resources> {
            RESOURCES
                .with(|current| current.borrow().clone())
                .expect("a resource of the Orb's root is used inside a method of the Orb")
        }

        #(#accessors)*
    };
    wrapped(facade, items)
}

/// The trait of `boundary_trait`, as the module `scope` names it.
fn boundary(scope: usize, analysis: &Analysis) -> TokenStream {
    boundary_at(analysis.modules[scope].len())
}

/// The trait of `boundary_trait`, as a module `depth` levels below the Orb module names it.
fn boundary_at(depth: usize) -> TokenStream {
    let root = crate::supers(depth);
    let boundary = format_ident!("{BOUNDARY}");
    quote! { #root #boundary }
}

/// What the native side does not cover yet: the message, naming the item, of the error the module's
/// `orb()` answers where the runtime is linked.
struct Refused(String);

fn refused(message: &str) -> Refused {
    Refused(message.to_owned())
}

/// `orb()` and `orb_named` of a module the native side does not cover yet, which answer the error
/// with the code `unsupported`, the reason `native` and `message`, naming what. The module compiles
/// where the runtime is linked, as it must: Cargo enables the facade's `host` for a guest whenever
/// another crate of the build does, and only a host that builds the Orb fails. They take the inputs
/// of an `#[inputs]` struct, as they would otherwise, and no providers. The runtime's types are
/// reached through the facade named `facade`.
fn unsupported(
    metadata: &Metadata,
    inputs: Option<&TokenStream>,
    message: &str,
    facade: &Ident,
) -> TokenStream {
    let name = &metadata.name;
    let version = metadata.api_version;
    let doc = format!(
        " The Orb for a native host, which the native side does not build yet: it answers an error with the code `unsupported`, saying that {message}. A component build of the module is unaffected."
    );
    let parameter = inputs.map(|path| quote! { inputs: #path });
    let named_parameter = inputs.map(|path| quote! { , inputs: #path });
    let argument = inputs.map(|_| quote! { , inputs });
    let unused = inputs.map(|_| quote! { let _ = inputs; });
    quote! {
        #[doc = #doc]
        pub fn orb(#parameter) -> ::#facade::Result<::#facade::Orb> {
            orb_named(#name, #version #argument)
        }

        #[doc = #doc]
        pub fn orb_named(
            name: &str,
            api_version: u32
            #named_parameter
        ) -> ::#facade::Result<::#facade::Orb> {
            let _ = (name, api_version);
            #unused
            ::core::result::Result::Err(::#facade::Error::new(
                ::#facade::core::Code::Unsupported,
                "native",
                #message
            ))
        }
    }
}

/// Whether the native side covers the module's hook declarations: a declared hook's `run` is
/// synchronous, and natively a method has no fiber to suspend while the kernel runs the hook, so a
/// declaration works in a component build only. `providers::imports` refuses the imports it does
/// not cover first, and `providers::contributions` the contributions.
fn covered(analysis: &Analysis) -> Result<(), Refused> {
    if let Some(declaration) = analysis.declared.first() {
        return Err(refused(&format!(
            "the declaration of the hook {} works in a component build only: its `run` is synchronous, and natively a method has no fiber to suspend while the kernel runs the hook's contributions, so a native Orb declares no hook",
            declaration.hook
        )));
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
/// of its interface, unless it has no method, the ownership policy of its attribute and one method
/// per `pub fn`, under its WIT name; a `#[local]` one, under the name it would have there, has no
/// portable entrypoint. The method runs `context.check()` first and then the author's function,
/// awaited when it is `async`, with the arguments as a tuple in WIT order. A service with a create
/// takes the inputs `orb(..)` shares, and its factory calls the create with them and the object's
/// declaration: the service it returns is the object's environment, which a method borrowing
/// `&self` is called on. The guard of the attribute, if any, runs first in the factory. A service
/// whose objects own a `#[state]` declares it next, before its create runs, and keeps it beside the
/// environment, for the scope each method's future runs in, where the state's `read` and `update`
/// reach it. With root resources, the service takes the table of its Orb's too, which each
/// method's future runs with, where their `with` reaches them, and so with the Orb's imports,
/// `imported`, whose functions call with the call of the method. The function is the registry's of
/// the service's module, which `orb()` reaches whatever the visibility of the module.
fn registration(
    service: &Service,
    analysis: &Analysis,
    imported: bool,
) -> Result<TokenStream, Refused> {
    let facade = &analysis.options.facade();
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
    let guard = guard(service, &identity, facade);
    let state = analysis
        .states
        .iter()
        .find(|state| state.identity == identity);
    let (environment, factory) = match state {
        None => (
            environment,
            match (&service.create, guard) {
                (Some(_), None) => quote! { move |declaration| #build },
                (Some(_), Some(guard)) => quote! { move |declaration| { #guard #build } },
                (None, None) => quote! { |_| { #build } },
                (None, Some(guard)) => quote! { |declaration| { #guard #build } },
            },
        ),
        Some(state) => {
            let path = crate::relative(depth, &analysis.modules[state.scope], &state.ident);
            let version = state.version;
            let name = state.ident.to_string();
            let rule = match &state.validator {
                Some(validator) => quote! { |state: &#path| state.#validator() },
                None => quote! { |_: &#path| ::core::result::Result::Ok(()) },
            };
            let (capture, built) = match &service.create {
                Some(_) => (quote! { move }, quote! { #build? }),
                None => (quote! {}, quote! { () }),
            };
            (
                quote! { (#environment, ::#facade::State<::#facade::serde_json::Value>) },
                quote! {
                    #capture |declaration| {
                        #guard
                        let state = self::#root __tembra_state::declare::<#path>(
                            &declaration,
                            #version,
                            #name,
                            #rule,
                        )?;
                        ::core::result::Result::Ok((#built, state))
                    }
                },
            )
        }
    };
    let lifetime = service.policy.as_ref().map(|policy| {
        let lifetime = if policy.host {
            quote! { Host }
        } else {
            quote! { Referenced }
        };
        let linger = policy.linger_ms;
        quote! { .lifetime(::#facade::core::Lifetime::#lifetime, #linger) }
    });
    // A service without methods or resources exports an empty interface, of which a contract
    // plans no service: it has no signature.
    let signed = service.planned(&analysis.wit_resources).then(|| {
        quote! {
            .signature(::core::option::Option::Some(::std::string::String::from(#root #signature)))
        }
    });
    // Each method runs in the scope of its call, which the native `context` reads, with the
    // `context` option, and in that of its object, when the service's objects own a state.
    let scopes = Scopes {
        context: analysis
            .options
            .context
            .then(|| quote! { self::#root context::__scoped }),
        state: state.map(|_| ObjectState {
            scoped: quote! { self::#root __tembra_state::scoped },
            object: environment.clone(),
            state: quote! { __tembra_object.1.clone() },
        }),
        resources: (!analysis.resources.is_empty())
            .then(|| quote! { self::#root __tembra_resource::scoped }),
        imports: imported.then(|| quote! { self::#root __tembra_import::scoped }),
    };
    // With root resources, the service takes the table of its Orb's, which each method's scope
    // sets, and so with imports.
    let table = scopes.resources.is_some().then(|| {
        quote! { , resources: &::std::rc::Rc<self::#root __tembra_resource::Resources> }
    });
    let imports = imported.then(|| {
        quote! { , imports: &::std::rc::Rc<self::#root __tembra_import::Imports> }
    });
    let mut codecs = Vec::new();
    let mut methods = Vec::new();
    for method in &service.functions {
        if issues(method, service.scope, analysis) {
            methods.push(issuing(service, method, analysis, &scopes)?);
            continue;
        }
        let codec = codec_ident(method);
        codecs.push(self::codec(service, method, &codec, analysis)?);
        methods.push(self::method(service, method, Some(&codec), &scopes, facade));
    }
    for local in &service.locals {
        methods.push(self::method(service, local, None, &scopes, facade));
    }
    let doc = format!(
        " The service `{}` of the native Orb named `orb`, as `orb()` registers it.",
        service.ident
    );
    let built = quote! {
        ::#facade::Service::new(::std::format!("{}::{}", orb, #identity), #factory)
        #signed
        #lifetime
        #(#methods)*
    };
    // The functions of each WIT resource of the service's interface, registered from the
    // resource's module, which reach the object's state through the function given them, and the
    // Orb's resources through its table.
    let state_of = state.map(|_| quote! { , |object: &#environment| object.1.clone() });
    let resources = scopes.resources.is_some().then(|| quote! { , resources });
    let imported = imported.then(|| quote! { , imports });
    let built = owned_resources(service, analysis).fold(built, |built, resource| {
        let glue = crate::relative(
            depth,
            &analysis.modules[resource.scope],
            &glue_ident(resource),
        );
        quote! { #glue(#built #state_of #resources #imported) }
    });
    let registry = format_ident!("{REGISTRY}");
    let registered = Literal::usize_unsuffixed(service.scope);
    Ok(quote! {
        impl #root #registry<#registered> {
            #[doc = #doc]
            pub(crate) fn #function(
                orb: &str #parameter #table #imports
            ) -> ::#facade::Service<#environment> {
                #(#codecs)*

                #shared
                #built
            }
        }
    })
}

/// The WIT resources of a service's interface.
fn owned_resources<'a>(
    service: &'a Service,
    analysis: &'a Analysis,
) -> impl Iterator<Item = &'a WitResource> {
    analysis
        .wit_resources
        .iter()
        .filter(|resource| resource.service.as_ref() == Some(&service.ident))
}

/// Whether a function's result holds a resource, which the call issues: the function is then
/// bound with `payload_method`, without a local entrypoint.
fn issues(function: &Function, scope: usize, analysis: &Analysis) -> bool {
    function.result.as_ref().is_some_and(|(ty, _)| {
        kind(ty, scope, analysis).is_ok_and(|kind| kind.resource().is_some())
    })
}

/// The `payload_method` call binding a function of a service whose result holds a resource, under
/// its WIT name: the call issues each resource of the value into its result, passing the context
/// itself where the function takes `&Context`; its future runs in the `scopes` of the service, as
/// a method's does. The function has no local entrypoint, so the service's client calls it
/// portably, and holds the resources as their handle types.
fn issuing(
    service: &Service,
    function: &Function,
    analysis: &Analysis,
    scopes: &Scopes,
) -> Result<TokenStream, Refused> {
    let owner = format!("{}::{}", service.ident, function.ident);
    let result = function.result.as_ref().map(|(ty, _)| ty);
    let (kinds, result) = signature(&owner, &function.params, result, service.scope, analysis)?;
    let target = &service.ident;
    let ident = &function.ident;
    let mut arguments: Vec<TokenStream> = function
        .params
        .iter()
        .map(|(name, _, _)| quote! { #name })
        .collect();
    if let Some(index) = function.context {
        arguments.insert(index, quote! { &__tembra_context });
    }
    let awaited = function.asynchronous.then(|| quote! { .await });
    let (environment, receiver) = scopes.environment(target, function.receiver);
    // Through the service's type, its receiver first, as `method` calls a method.
    if let Some(receiver) = receiver {
        arguments.insert(0, receiver);
    }
    let call = quote! { #target::#ident(#(#arguments),*) #awaited };
    // A method returning `tembra::Result<T>` answers with it: its failure is the call's error.
    let call = if function.fallible {
        quote! { #call? }
    } else {
        call
    };
    Ok(payload_method(
        &wit::kebab(ident),
        environment,
        None,
        &function.params,
        &kinds,
        result.as_ref().map(|(ty, kind)| (*ty, kind)),
        call,
        &boundary(service.scope, analysis),
        scopes,
        &analysis.options.facade(),
    ))
}

/// The kinds of a function's parameters, and its result's type with its kind, written in the module
/// `scope`, or the refusal of what the codec does not cover, naming it and where `owner`, the
/// function, `Ledger::add`, takes or returns it.
#[allow(clippy::type_complexity)]
fn signature<'a>(
    owner: &str,
    params: &'a [(Ident, Type, String)],
    result: Option<&'a Type>,
    scope: usize,
    analysis: &Analysis,
) -> Result<(Vec<Kind<'a>>, Option<(&'a Type, Kind<'a>)>), Refused> {
    let place = |role: &str, ty: &Type, uncovered: Uncovered| {
        refused(&format!(
            "`{owner}` {role} `{}`{}",
            spelled(ty),
            uncovered.described(ty)
        ))
    };
    let mut kinds = Vec::new();
    for (_, ty, _) in params {
        kinds.push(
            argument(ty, scope, analysis).map_err(|uncovered| place("takes", ty, uncovered))?,
        );
    }
    let result = result
        .map(|ty| {
            kind(ty, scope, analysis)
                .map(|kind| (ty, kind))
                .map_err(|uncovered| place("returns", ty, uncovered))
        })
        .transpose()?;
    Ok((kinds, result))
}

/// A function bound with `payload_method` under its WIT name, `name`, its handle taking the
/// object's environment as `environment` binds it: the call checks its context, takes its `self`
/// as the `Receiver` of `receiver`, the resource's type, for a method, reads the arguments with the
/// codec into the parameters, and writes the value of `call`, an expression over them of the
/// result's type and kind, as the result's data, issuing each resource it holds into the result.
/// An argument the codec refuses fails the call as its arguments, before the function runs, and a
/// value it cannot write fails it as its result, which closes what was issued. The call's future
/// runs in the `scopes` of the service whose objects answer it. The runtime is the facade named
/// `facade`.
#[allow(clippy::too_many_arguments)]
fn payload_method(
    name: &str,
    environment: TokenStream,
    receiver: Option<&TokenStream>,
    params: &[(Ident, Type, String)],
    kinds: &[Kind<'_>],
    result: Option<(&Type, &Kind<'_>)>,
    call: TokenStream,
    boundary: &TokenStream,
    scopes: &Scopes,
    facade: &Ident,
) -> TokenStream {
    let names: Vec<&Ident> = params.iter().map(|(name, _, _)| name).collect();
    let types: Vec<&Type> = params.iter().map(|(_, ty, _)| ty).collect();
    let decode = decode_arguments(kinds, boundary, facade);
    let (payload, receiver) = match receiver {
        Some(resource) => (
            quote! { mut __tembra_payload },
            quote! { let __tembra_receiver = __tembra_payload.receiver::<#resource>()?; },
        ),
        None => (quote! { __tembra_payload }, quote! {}),
    };
    let write = match result {
        None => quote! {
            #call;
            ::core::result::Result::Ok(::#facade::portable::Payload::data(
                ::#facade::serde_json::Value::Null,
            ))
        },
        // The value is written by a closure returning the codec's `Result<Value, String>`, the
        // error type the `?` of its parts converts to.
        Some((ty, kind)) => {
            let encode = encode(kind, quote! { value }, boundary, facade);
            let written = quote! {
                ::core::result::Result<::#facade::serde_json::Value, ::std::string::String>
            };
            let failed = quote! {
                |message| ::#facade::Error::invalid(::#facade::core::Reason::Result, message)
            };
            if kind.resource().is_some() {
                let result = result_payload();
                quote! {
                    let value = #call;
                    let encode = |value: #ty, #result: &mut ::#facade::portable::Payload| -> #written {
                        #encode
                    };
                    let mut #result = ::#facade::portable::Payload::data(
                        ::#facade::serde_json::Value::Null,
                    );
                    let written = encode(value, &mut #result).map_err(#failed)?;
                    #result.map_value(|_| ::core::result::Result::Ok(written))
                }
            } else {
                quote! {
                    let value = #call;
                    let encode = |value: #ty| -> #written { #encode };
                    let written = encode(value).map_err(#failed)?;
                    ::core::result::Result::Ok(::#facade::portable::Payload::data(written))
                }
            }
        }
    };
    let future = quote! {
        async move {
            __tembra_context.check()?;
            #receiver
            let decode = |value: ::#facade::serde_json::Value| -> ::core::result::Result<
                (#(#types,)*),
                ::std::string::String,
            > { #decode };
            let (#(#names,)*): (#(#types,)*) = decode(__tembra_payload.into_data()?)
                .map_err(|message| {
                    ::#facade::Error::invalid(::#facade::core::Reason::Arguments, message)
                })?;
            #write
        }
    };
    let future = scopes.around(future);
    let handle = scopes.handle(quote! {
        |#environment, __tembra_context: ::#facade::Context, #payload: ::#facade::portable::Payload| #future
    });
    quote! {
        .payload_method(
            #name,
            #handle,
        )
    }
}

/// The function registering a WIT resource's functions on its service, `__tembra_resource_cursor`
/// for `Cursor`.
fn glue_ident(resource: &WitResource) -> Ident {
    format_ident!(
        "__tembra_resource_{}",
        resource.ident.to_string().to_snake_case()
    )
}

/// The handle type of a WIT resource for native callers, `CursorHandle` for `Cursor`.
fn handle_ident(resource: &WitResource) -> Ident {
    format_ident!("{}Handle", resource.ident)
}

/// The handle type of a WIT resource, as the module `scope` names it.
fn handle_path(resource: &WitResource, scope: usize, analysis: &Analysis) -> TokenStream {
    crate::relative(
        analysis.modules[scope].len(),
        &analysis.modules[resource.scope],
        &handle_ident(resource),
    )
}

/// The native side of a WIT resource, in its own module: its struct is `Issuable` as the resource
/// of its service's interface, and a function adds its functions to the service, each bound with
/// `payload_method` under its WIT name: the constructor, `[constructor]<resource>`, which issues
/// the value it builds; each method, `[method]<resource>.<name>`, which reads its `self` from the
/// call as the value issued, through the `Receiver` the call pins; and each static function,
/// `[static]<resource>.<name>`. A synchronous method reads the value while it runs, and an `async`
/// one borrows it from the receiver's guard, which its future holds across its awaits until the
/// call ends. Each runs on an object of the service, in the scopes its methods run in: that of its
/// call, with the `context` option, that of its object, when the service's objects own a state,
/// which the function adding them is given the way to reach, as the object's environment is the
/// service's, and that of its Orb, with root resources, whose table that function is given too, as
/// it is the table of the Orb's imports. For native callers, the resource's handle type and the
/// service's client calls of its constructor and static functions come with it.
fn resource_glue(
    resource: &WitResource,
    analysis: &Analysis,
    imported: bool,
) -> Result<TokenStream, Refused> {
    let facade = &analysis.options.facade();
    let ident = &resource.ident;
    let name = wit::kebab(ident);
    let service = analysis
        .services
        .iter()
        .find(|service| resource.service.as_ref() == Some(&service.ident))
        .expect("a WIT resource's service is resolved");
    let identity = service.identity(&analysis.package);
    let boundary = boundary(resource.scope, analysis);
    let root = crate::supers(analysis.modules[resource.scope].len());
    let stated = analysis
        .states
        .iter()
        .any(|state| state.identity == identity);
    let scopes = Scopes {
        context: analysis
            .options
            .context
            .then(|| quote! { self::#root context::__scoped }),
        state: stated.then(|| ObjectState {
            scoped: quote! { self::#root __tembra_state::scoped },
            object: quote! { E },
            state: quote! { __tembra_state_of(&__tembra_object) },
        }),
        resources: (!analysis.resources.is_empty())
            .then(|| quote! { self::#root __tembra_resource::scoped }),
        imports: imported.then(|| quote! { self::#root __tembra_import::scoped }),
    };
    let (environment, _) = scopes.environment(&service.ident, false);
    let mut functions = Vec::new();
    if let Some(constructor) = &resource.constructor {
        let owner = format!("{ident}::{}", constructor.ident);
        let (kinds, _) = signature(&owner, &constructor.params, None, resource.scope, analysis)?;
        let function = &constructor.ident;
        let params: Vec<&Ident> = constructor.params.iter().map(|(name, _, _)| name).collect();
        let built: Type = syn::parse_quote!(#ident);
        functions.push(payload_method(
            &format!("[constructor]{name}"),
            environment.clone(),
            None,
            &constructor.params,
            &kinds,
            Some((
                &built,
                &Kind::Resource {
                    ty: &built,
                    handle: handle_path(resource, resource.scope, analysis),
                },
            )),
            quote! { #ident::#function(#(#params),*) },
            &boundary,
            &scopes,
            facade,
        ));
    }
    for method in &resource.methods {
        let function = &method.ident;
        let owner = format!("{ident}::{function}");
        let result = method.result.as_ref().map(|(ty, _)| ty);
        let (kinds, result) = signature(&owner, &method.params, result, resource.scope, analysis)?;
        let params: Vec<&Ident> = method.params.iter().map(|(name, _, _)| name).collect();
        // A synchronous method reads the value while it runs; an `async` one borrows it from the
        // guard of its receiver, which its future holds across its awaits until the call ends.
        let call = if method.asynchronous {
            quote! {
                {
                    let __tembra_self = __tembra_receiver.guard()?;
                    #ident::#function(&*__tembra_self, #(#params),*).await
                }
            }
        } else {
            quote! {
                __tembra_receiver.with(|__tembra_value| #ident::#function(__tembra_value, #(#params),*))?
            }
        };
        functions.push(payload_method(
            &format!("[method]{name}.{}", wit::kebab(function)),
            environment.clone(),
            Some(&quote! { #ident }),
            &method.params,
            &kinds,
            result.as_ref().map(|(ty, kind)| (*ty, kind)),
            call,
            &boundary,
            &scopes,
            facade,
        ));
    }
    for function in &resource.statics {
        let owner = format!("{ident}::{}", function.ident);
        let result = function.result.as_ref().map(|(ty, _)| ty);
        let (kinds, result) =
            signature(&owner, &function.params, result, resource.scope, analysis)?;
        let static_ident = &function.ident;
        let params: Vec<&Ident> = function.params.iter().map(|(name, _, _)| name).collect();
        let awaited = function.asynchronous.then(|| quote! { .await });
        functions.push(payload_method(
            &format!("[static]{name}.{}", wit::kebab(static_ident)),
            environment.clone(),
            None,
            &function.params,
            &kinds,
            result.as_ref().map(|(ty, kind)| (*ty, kind)),
            quote! { #ident::#static_ident(#(#params),*) #awaited },
            &boundary,
            &scopes,
            facade,
        ));
    }
    let glue = glue_ident(resource);
    let doc = format!(
        " The functions of the WIT resource `{ident}`, added to the service `{}` of the native Orb.",
        service.ident
    );
    // The object of a service whose objects own a state is its environment and its state, which
    // the service's registration gives the way to reach.
    let state_of = stated.then(|| {
        quote! {
            , __tembra_state_of: fn(&E) -> ::#facade::State<::#facade::serde_json::Value>
        }
    });
    // With root resources, the table of the Orb's, which each function's scope sets.
    let resources = scopes.resources.is_some().then(|| {
        quote! { , resources: &::std::rc::Rc<self::#root __tembra_resource::Resources> }
    });
    let imports = scopes.imports.is_some().then(|| {
        quote! { , imports: &::std::rc::Rc<self::#root __tembra_import::Imports> }
    });
    let handle = resource_handle(resource, analysis)?;
    let client = resource_client(resource, service, analysis)?;
    Ok(quote! {
        impl ::#facade::portable::Issuable for #ident {
            fn nominal() -> ::#facade::portable::Nominal {
                ::#facade::portable::Nominal {
                    interface: ::std::string::String::from(#identity),
                    name: ::std::string::String::from(#name),
                }
            }
        }

        #[doc = #doc]
        pub(crate) fn #glue<E: 'static>(
            service: ::#facade::Service<E>
            #state_of
            #resources
            #imports
        ) -> ::#facade::Service<E> {
            service #(#functions)*
        }

        #handle
        #client
    })
}

/// The handle type of a WIT resource for native callers, `CursorHandle` for `Cursor`, public in
/// the resource's module: it owns an `Owned` handle of the resource, whose drop is the WIT drop,
/// and each method of the resource is an async method of it that takes the call's `&Context` first
/// and then the method's parameters, which the compiler checks, and calls the method through the
/// handle portably, answering a resource as its handle type. `TryFrom<Owned>` wraps an owned
/// handle of the resource's type and gives one of another type back as its error, still open, so
/// a caller can try the handle type of each resource of an interface; `Owned::from` gives the
/// owned handle back, to move the resource into a call's arguments. The context's name is
/// hygienic: a parameter of the same name stays the author's.
fn resource_handle(resource: &WitResource, analysis: &Analysis) -> Result<TokenStream, Refused> {
    let facade = &analysis.options.facade();
    let ident = &resource.ident;
    let handle = handle_ident(resource);
    let name = wit::kebab(ident);
    let boundary = boundary(resource.scope, analysis);
    let context = Ident::new("context", Span::mixed_site());
    let mut methods = Vec::new();
    for method in &resource.methods {
        let function = &method.ident;
        let owner = format!("{ident}::{function}");
        let result = method.result.as_ref().map(|(ty, _)| ty);
        let (kinds, result) = signature(&owner, &method.params, result, resource.scope, analysis)?;
        let (returned, body) = portable_call(
            quote! { self.owned },
            &format!("[method]{name}.{}", wit::kebab(function)),
            &method.params,
            &kinds,
            result.as_ref().map(|(ty, kind)| (*ty, kind)),
            &context,
            &boundary,
            facade,
        );
        let docs = &method.docs;
        let params = method
            .params
            .iter()
            .map(|(name, ty, _)| quote! { #name: #ty });
        methods.push(quote! {
            #(#[doc = #docs])*
            pub async fn #function(
                &self,
                #context: &::#facade::Context
                #(, #params)*
            ) -> ::#facade::Result<#returned> {
                #body
            }
        });
    }
    let doc = format!(
        " A handle of the WIT resource `{ident}` for a native caller: one async method per method of the resource, which takes the call's context first and then the method's parameters. Dropping it drops the resource's handle."
    );
    Ok(quote! {
        #[doc = #doc]
        pub struct #handle {
            owned: ::#facade::portable::Owned,
        }

        impl #handle {
            #(#methods)*
        }

        impl ::core::convert::TryFrom<::#facade::portable::Owned> for #handle {
            type Error = ::#facade::portable::Owned;

            /// Wrap an owned handle of the resource; one of another resource type comes back as
            /// the error, still open, for the handle type of its own.
            fn try_from(
                owned: ::#facade::portable::Owned,
            ) -> ::core::result::Result<Self, ::#facade::portable::Owned> {
                if *owned.nominal() != <#ident as ::#facade::portable::Issuable>::nominal() {
                    return ::core::result::Result::Err(owned);
                }
                ::core::result::Result::Ok(Self { owned })
            }
        }

        impl ::core::convert::From<#handle> for ::#facade::portable::Owned {
            /// The owned handle, to call the resource by name or move it into a call's arguments.
            fn from(handle: #handle) -> Self {
                handle.owned
            }
        }
    })
}

/// The calls of a WIT resource's constructor and static functions on the client of its service,
/// in the resource's module, where their types are written: an `impl` block of the client, with
/// `new_<resource>` for the constructor, `new_cursor` for `Cursor`, and `<resource>_<function>`
/// for each static function, `cursor_open` for `Cursor::open`, each called portably, as the
/// client's functions whose result holds a resource are, answering the resource as its handle type.
/// None without either, so a resource's module that cannot see the client still compiles; with
/// one, and a client that module cannot see, the refusal `orb()` answers.
fn resource_client(
    resource: &WitResource,
    service: &Service,
    analysis: &Analysis,
) -> Result<TokenStream, Refused> {
    // A constructor or a static function goes on the client, which the resource's module may not
    // see: the Orb's `orb()` then answers why, and a component build is unaffected.
    if resource.constructor.is_some() || !resource.statics.is_empty() {
        crate::client_seen(
            service,
            &analysis.modules[service.scope],
            &analysis.seen_from[service.scope],
            &analysis.modules[resource.scope],
            &resource.ident,
        )
        .map_err(Refused)?;
    }
    let facade = &analysis.options.facade();
    let ident = &resource.ident;
    let name = wit::kebab(ident);
    let snake = ident.to_string().to_snake_case();
    let client = crate::relative(
        analysis.modules[resource.scope].len(),
        &analysis.modules[service.scope],
        &client_ident(service),
    );
    let boundary = boundary(resource.scope, analysis);
    let context = Ident::new("context", Span::mixed_site());
    let mut methods = Vec::new();
    if let Some(constructor) = &resource.constructor {
        let owner = format!("{ident}::{}", constructor.ident);
        let (kinds, _) = signature(&owner, &constructor.params, None, resource.scope, analysis)?;
        let built: Type = syn::parse_quote!(#ident);
        let kind = Kind::Resource {
            ty: &built,
            handle: handle_path(resource, resource.scope, analysis),
        };
        let (returned, body) = portable_call(
            quote! { self.reference },
            &format!("[constructor]{name}"),
            &constructor.params,
            &kinds,
            Some((&built, &kind)),
            &context,
            &boundary,
            facade,
        );
        let method = format_ident!("new_{snake}", span = constructor.ident.span());
        let doc = format!(" A new `{ident}`, from its constructor, as its handle.");
        let params = constructor
            .params
            .iter()
            .map(|(name, ty, _)| quote! { #name: #ty });
        methods.push(quote! {
            #[doc = #doc]
            pub async fn #method(
                &self,
                #context: &::#facade::Context
                #(, #params)*
            ) -> ::#facade::Result<#returned> {
                #body
            }
        });
    }
    for function in &resource.statics {
        let owner = format!("{ident}::{}", function.ident);
        let result = function.result.as_ref().map(|(ty, _)| ty);
        let (kinds, result) =
            signature(&owner, &function.params, result, resource.scope, analysis)?;
        let (returned, body) = portable_call(
            quote! { self.reference },
            &format!("[static]{name}.{}", wit::kebab(&function.ident)),
            &function.params,
            &kinds,
            result.as_ref().map(|(ty, kind)| (*ty, kind)),
            &context,
            &boundary,
            facade,
        );
        let method = format_ident!("{snake}_{}", function.ident, span = function.ident.span());
        let docs = &function.docs;
        let params = function
            .params
            .iter()
            .map(|(name, ty, _)| quote! { #name: #ty });
        methods.push(quote! {
            #(#[doc = #docs])*
            pub async fn #method(
                &self,
                #context: &::#facade::Context
                #(, #params)*
            ) -> ::#facade::Result<#returned> {
                #body
            }
        });
    }
    // Without a constructor or a static function, the resource's module names no client, which it
    // may not see; with one, it was refused above.
    if methods.is_empty() {
        return Ok(TokenStream::new());
    }
    Ok(quote! {
        impl #client {
            #(#methods)*
        }
    })
}

/// The guard of a service's objects, first in its factory, if its attribute has one: an object at
/// another key than `main`, or of another principal, is refused as forbidden, with the reason
/// `key` or `principal` as a hand-written factory gives it, before the create runs. `identity`
/// names the service's interface in the errors, and `facade` the facade.
fn guard(service: &Service, identity: &str, facade: &Ident) -> Option<TokenStream> {
    let refusal = |reason: &str, message: String| {
        quote! {
            return ::core::result::Result::Err(::#facade::Error::new(
                ::#facade::Code::Forbidden,
                #reason,
                #message
            ));
        }
    };
    let key = service.guard.main.map(|_| {
        let refused = refusal("key", format!("the objects of {identity} use the key main"));
        quote! { if address.key != ["main"] { #refused } }
    });
    let principal = service.guard.principal.as_ref().map(|(principal, _)| {
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

/// The scopes the future of each function a service's objects answer runs in, set around each of
/// its polls, as the executor gives a component's call its authority: a method's, a function's whose
/// result holds a WIT resource, and the functions of the service's WIT resources.
struct Scopes {
    /// The call's, which the native `context` reads, with the `context` option: the path of its
    /// `__scoped`.
    context: Option<TokenStream>,
    /// The object's, whose state the native `read` and `update` reach, for a service whose objects
    /// own one.
    state: Option<ObjectState>,
    /// The Orb's, whose root resources the native `with` reaches, for an Orb with some: the path of
    /// `__tembra_resource::scoped`, which takes the table of the Orb's resources, `resources`, which
    /// the function registering the handle takes.
    resources: Option<TokenStream>,
    /// The call's, which the functions of the import modules call with, for an Orb that imports:
    /// the path of `__tembra_import::scoped`, which takes the call's context and the table of the
    /// Orb's imports, `imports`, which the function registering the handle takes.
    imports: Option<TokenStream>,
}

/// How a handle reaches the state of its object: the path of `__tembra_state::scoped`, the type of
/// the object, its environment and its state, which the handle takes as `__tembra_object`, an `Rc`
/// of it, and the expression of the object's state over `__tembra_object`.
struct ObjectState {
    scoped: TokenStream,
    object: TokenStream,
    state: TokenStream,
}

impl Scopes {
    /// How a handle takes the object's environment, and the reference to it a function borrowing
    /// `&self` takes, if it does: the service's environment, or the object of a service whose
    /// objects own a state, which holds the environment beside the state.
    fn environment(&self, target: &Ident, receiver: bool) -> (TokenStream, Option<TokenStream>) {
        // The index is a token of its own: `0.` written before an interpolation would lex as a
        // float.
        let environment_field = syn::Index::from(0);
        match (&self.state, receiver) {
            (None, true) => (
                quote! { __tembra_self: ::std::rc::Rc<#target> },
                Some(quote! { &*__tembra_self }),
            ),
            (None, false) => (quote! { _ }, None),
            (Some(state), receiver) => {
                let object = &state.object;
                (
                    quote! { __tembra_object: ::std::rc::Rc<#object> },
                    receiver.then(|| quote! { &__tembra_object.#environment_field }),
                )
            }
        }
    }

    /// `future`, a handle's future over its `__tembra_context`, `__tembra_object`,
    /// `__tembra_resources` and `__tembra_imports`, run in each scope.
    fn around(&self, future: TokenStream) -> TokenStream {
        let future = match &self.imports {
            Some(scoped) => quote! {
                #scoped(
                    __tembra_context.clone(),
                    ::std::rc::Rc::clone(&__tembra_imports),
                    #future,
                )
            },
            None => future,
        };
        let future = match &self.state {
            Some(ObjectState { scoped, state, .. }) => {
                quote! { #scoped(__tembra_context.clone(), #state, #future) }
            }
            None => future,
        };
        let future = match &self.resources {
            Some(scoped) => quote! { #scoped(::std::rc::Rc::clone(&__tembra_resources), #future) },
            None => future,
        };
        match &self.context {
            Some(scoped) => quote! { #scoped(__tembra_context.clone(), #future) },
            None => future,
        }
    }

    /// `handle`, a closure, as one that owns what its scopes read: a handle that reaches its
    /// object's state moves what it reads it with, and one that reaches the Orb's resources or its
    /// imports keeps the table of them.
    fn handle(&self, handle: TokenStream) -> TokenStream {
        let imports = self
            .imports
            .is_some()
            .then(|| quote! { let __tembra_imports = ::std::rc::Rc::clone(imports); });
        match (&self.resources, &self.state, imports) {
            (Some(_), _, imports) => quote! {
                {
                    let __tembra_resources = ::std::rc::Rc::clone(resources);
                    #imports
                    move #handle
                }
            },
            (None, _, Some(imports)) => quote! {
                {
                    #imports
                    move #handle
                }
            },
            (None, Some(_), None) => quote! { move #handle },
            (None, None, None) => handle,
        }
    }
}

/// The call binding one method: `method_with` and its codec, or `local_method` for a `#[local]`
/// one, without a codec, which refuses portable callers. The handle takes the arguments as a tuple
/// of the author's parameter types, checks the context and calls the author's function, on the
/// object's environment when it borrows `&self`, passing the context itself where the function
/// takes `&Context`, and answering with its value, or with the `tembra::Result` it returns, whose
/// failure is the call's error; its future runs in the `scopes` of the service: the scope of its
/// call for the native `context`, that of its object, for the object's state, and that of its Orb,
/// for the root's resources. The function is called through the service's type, so a method named
/// as one of the `Rc` holding the environment, such as `clone`, is the service's, and the runtime
/// through the facade named `facade`.
fn method(
    service: &Service,
    function: &Function,
    codec: Option<&Ident>,
    scopes: &Scopes,
    facade: &Ident,
) -> TokenStream {
    let name = wit::kebab(&function.ident);
    let target = &service.ident;
    let ident = &function.ident;
    let params: Vec<&Ident> = function.params.iter().map(|(name, _, _)| name).collect();
    let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
    let mut arguments: Vec<TokenStream> = params.iter().map(|param| quote! { #param }).collect();
    if let Some(index) = function.context {
        arguments.insert(index, quote! { &__tembra_context });
    }
    let awaited = function.asynchronous.then(|| quote! { .await });
    let (environment, receiver) = scopes.environment(target, function.receiver);
    // Through the service's type, its receiver first: called on the `Rc` holding the environment,
    // a method named as one of `Rc`'s, such as `clone`, would be `Rc`'s.
    if let Some(receiver) = receiver {
        arguments.insert(0, receiver);
    }
    let call = quote! { #target::#ident(#(#arguments),*) #awaited };
    // A method returning `tembra::Result<T>` answers with it: its failure is the call's error.
    let body = match (&function.result, function.fallible) {
        (_, true) => call,
        (Some(_), false) => quote! { ::core::result::Result::Ok(#call) },
        (None, false) => quote! { #call; ::core::result::Result::Ok(()) },
    };
    let future = scopes.around(quote! {
        async move {
            __tembra_context.check()?;
            #body
        }
    });
    let (register, codec) = match codec {
        Some(codec) => (quote! { method_with }, quote! { #codec, }),
        None => (quote! { local_method }, quote! {}),
    };
    let handle = scopes.handle(quote! {
        |#environment, __tembra_context: ::#facade::Context, (#(#params,)*): (#(#types,)*)| #future
    });
    quote! {
        .#register(
            #name,
            #handle,
            #codec
        )
    }
}

/// The typed client of a service, `MathClient` for `Math`.
fn client_ident(service: &Service) -> Ident {
    format_ident!("{}Client", service.ident)
}

/// The names the client of a service gives its methods: each function of the interface and each
/// `#[local]` method its own, `new_` for `new`, and for each resource of the interface,
/// `new_<resource>` its constructor and `<resource>_<function>` each static function. Two of one
/// name, which the compiler would refuse as one method defined twice, are refused at the later
/// one, naming both.
fn client_names(service: &Service, analysis: &Analysis) -> Result<(), Refused> {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut claim = |name: String, what: String| {
        if let Some(previous) = names.get(&name) {
            return Err(refused(&format!(
                "the client of `{}` would name both {previous} and {what} `{name}`; rename one of them",
                service.ident
            )));
        }
        names.insert(name, what);
        Ok(())
    };
    for function in service.functions.iter().chain(&service.locals) {
        let name = if function.ident == "new" {
            "new_".to_owned()
        } else {
            function.ident.to_string()
        };
        let what = format!("the function `{}::{}`", service.ident, function.ident);
        claim(name, what)?;
    }
    for resource in owned_resources(service, analysis) {
        let ident = &resource.ident;
        let snake = ident.to_string().to_snake_case();
        if resource.constructor.is_some() {
            let what = format!("the constructor of `{ident}`");
            claim(format!("new_{snake}"), what)?;
        }
        for function in &resource.statics {
            let what = format!("the static function `{ident}::{}`", function.ident);
            let name = format!("{snake}_{}", function.ident);
            claim(name, what)?;
        }
    }
    Ok(())
}

/// The typed client of a service, `MathClient` for `Math`, in the service's module and as visible
/// as its struct: `new` wraps a reference to one of its objects, and each function of the
/// interface, and each `#[local]` method, is an async method that takes the call's `&Context` first
/// and then the function's parameters, which the compiler checks. A function with a local
/// entrypoint is called there with them as a tuple in WIT order, `()` without parameters, so the
/// types `Reference::call` checks at run time are the function's; one whose result holds a
/// resource, which has only its portable entrypoint, is called portably, and answers each resource
/// as its handle type. A function named `new` is `new_` on the client, since `new` builds the
/// client; its WIT name stays `new`. The constructors and static functions of the service's
/// resources are the client's too, in their resources' modules. The reference is visible to the
/// crate for them. Two of these methods of one name are refused, naming both. The context's name is
/// hygienic: a parameter of the same name stays the author's.
fn client(service: &Service, analysis: &Analysis) -> Result<TokenStream, Refused> {
    client_names(service, analysis)?;
    let facade = &analysis.options.facade();
    let vis = &service.vis;
    let ident = client_ident(service);
    let context = Ident::new("context", Span::mixed_site());
    let boundary = boundary(service.scope, analysis);
    let mut methods = Vec::new();
    // A `#[local]` method has only its local entrypoint, whatever it returns.
    let functions = service.functions.iter().map(|function| (function, false));
    let locals = service.locals.iter().map(|function| (function, true));
    for (function, local) in functions.chain(locals) {
        let name = wit::kebab(&function.ident);
        let method = if function.ident == "new" {
            format_ident!("new_", span = function.ident.span())
        } else {
            function.ident.clone()
        };
        let (returned, body) = if !local && issues(function, service.scope, analysis) {
            let owner = format!("{}::{}", service.ident, function.ident);
            let result = function.result.as_ref().map(|(ty, _)| ty);
            let (kinds, result) =
                signature(&owner, &function.params, result, service.scope, analysis)?;
            portable_call(
                quote! { self.reference },
                &name,
                &function.params,
                &kinds,
                result.as_ref().map(|(ty, kind)| (*ty, kind)),
                &context,
                &boundary,
                facade,
            )
        } else {
            let params: Vec<&Ident> = function.params.iter().map(|(name, _, _)| name).collect();
            let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
            let result = match &function.result {
                Some((ty, _)) => quote! { #ty },
                None => quote! { () },
            };
            let body = quote! {
                self.reference
                    .call::<(#(#types,)*), #result>(#context, #name, (#(#params,)*))
                    .await
            };
            (result, body)
        };
        let docs = &function.docs;
        let params = function
            .params
            .iter()
            .map(|(name, ty, _)| quote! { #name: #ty });
        methods.push(quote! {
            #(#[doc = #docs])*
            pub async fn #method(
                &self,
                #context: &::#facade::Context
                #(, #params)*
            ) -> ::#facade::Result<#returned> {
                #body
            }
        });
    }
    let doc = format!(
        " A typed client of the service `{}`: one async method per function of its interface and per `#[local]` method, and per constructor and static function of its resources, which takes the call's context first and then the function's parameters. A resource the function answers is its handle type, whose methods are the resource's.",
        service.ident
    );
    Ok(quote! {
        #[doc = #doc]
        #vis struct #ident {
            pub(crate) reference: ::std::rc::Rc<::#facade::Reference>,
        }

        impl #ident {
            /// A client of the object `reference` holds.
            pub fn new(reference: ::std::rc::Rc<::#facade::Reference>) -> Self {
                Self { reference }
            }

            #(#methods)*
        }
    })
}

/// A native caller's portable call of a function, under its WIT name, `name`, through `callee`, a
/// `Reference` or an `Owned` handle, under `context`: the type the call answers, and the body of
/// the caller's async method. The parameters cross as the boundary's JSON array, which the codec
/// writes, and the result comes back as the author's type, but for each resource it holds, which is
/// its handle type: a bare one from the `Owned` handle the call delivers, nested ones from the
/// `Delivered` result, each the handle at the index the value names. An argument the codec cannot
/// write fails the call as its arguments, before it is issued, and a result it cannot read fails it
/// as its result, which closes every handle the result held. The runtime is the facade named
/// `facade`.
#[allow(clippy::too_many_arguments)]
fn portable_call(
    callee: TokenStream,
    name: &str,
    params: &[(Ident, Type, String)],
    kinds: &[Kind<'_>],
    result: Option<(&Type, &Kind<'_>)>,
    context: &Ident,
    boundary: &TokenStream,
    facade: &Ident,
) -> (TokenStream, TokenStream) {
    let encoded = kinds
        .iter()
        .zip(params)
        .map(|(kind, (name, _, _))| encode(kind, quote! { #name }, boundary, facade));
    let arguments = quote! {
        let __tembra_encode = || -> ::core::result::Result<
            ::#facade::serde_json::Value,
            ::std::string::String,
        > {
            ::core::result::Result::Ok(::#facade::serde_json::Value::Array(
                ::std::vec![#(#encoded?),*],
            ))
        };
        let __tembra_arguments = __tembra_encode().map_err(|message| {
            ::#facade::Error::invalid(::#facade::core::Reason::Arguments, message)
        })?;
    };
    let call = |answer: TokenStream| {
        quote! {
            #callee
                .call_portable::<_, #answer>(#context, #name, __tembra_arguments)
                .await?
        }
    };
    let failed = quote! {
        |message| ::#facade::Error::invalid(::#facade::core::Reason::Result, message)
    };
    let (returned, read) = match result {
        None => {
            let called = call(quote! { ::#facade::serde_json::Value });
            (
                quote! { () },
                quote! {
                    #called;
                    ::core::result::Result::Ok(())
                },
            )
        }
        Some((_, Kind::Resource { handle, .. })) => {
            let called = call(quote! { ::#facade::portable::Owned });
            (
                quote! { #handle },
                quote! {
                    <#handle as ::core::convert::TryFrom<::#facade::portable::Owned>>::try_from(#called)
                        .map_err(|_| ::#facade::Error::invalid(
                            ::#facade::core::Reason::Result,
                            "the result holds a resource of another type",
                        ))
                },
            )
        }
        Some((ty, kind)) if kind.resource().is_none() => {
            let called = call(quote! { ::#facade::serde_json::Value });
            let decode = decode(kind, quote! { __tembra_value }, boundary, facade);
            (
                quote! { #ty },
                quote! {
                    let __tembra_value = #called;
                    let __tembra_decode = |__tembra_value: ::#facade::serde_json::Value|
                        -> ::core::result::Result<#ty, ::std::string::String> { #decode };
                    __tembra_decode(__tembra_value).map_err(#failed)
                },
            )
        }
        Some((_, kind)) => {
            let called = call(quote! { ::#facade::portable::Delivered });
            let delivered = delivered_type(kind);
            let decode = decode(kind, quote! { __tembra_value }, boundary, facade);
            let owned = quote! {
                ::std::vec::Vec<::core::option::Option<::#facade::portable::Owned>>
            };
            (
                delivered.clone(),
                quote! {
                    let __tembra_delivered = #called;
                    let mut __tembra_owned: #owned = __tembra_delivered
                        .owned
                        .into_iter()
                        .map(::core::option::Option::Some)
                        .collect();
                    let __tembra_decode = |__tembra_value: ::#facade::serde_json::Value,
                                           __tembra_owned: &mut #owned|
                        -> ::core::result::Result<#delivered, ::std::string::String> { #decode };
                    let __tembra_read = __tembra_decode(__tembra_delivered.value, &mut __tembra_owned)
                        .map_err(#failed)?;
                    if __tembra_owned.iter().any(::core::option::Option::is_some) {
                        return ::core::result::Result::Err(::#facade::Error::invalid(
                            ::#facade::core::Reason::Result,
                            "the result holds a resource its value does not name",
                        ));
                    }
                    ::core::result::Result::Ok(__tembra_read)
                },
            )
        }
    };
    (returned, quote! { #arguments #read })
}

/// The type a native caller receives for a result of this kind: the Rust type, with each resource
/// it holds replaced by its handle type.
fn delivered_type(kind: &Kind<'_>) -> TokenStream {
    match kind {
        Kind::Resource { handle, .. } => quote! { #handle },
        Kind::List(_, element) => {
            let element = delivered_type(element);
            quote! { ::std::vec::Vec<#element> }
        }
        Kind::Option(inner) => {
            let inner = delivered_type(inner);
            quote! { ::core::option::Option<#inner> }
        }
        Kind::Tuple(kinds) => {
            let kinds = kinds.iter().map(delivered_type);
            quote! { (#(#kinds,)*) }
        }
        Kind::Result(ok, err) => {
            let side = |side: &Option<Box<Kind<'_>>>| {
                side.as_deref()
                    .map_or_else(|| quote! { () }, delivered_type)
            };
            let (ok, err) = (side(ok), side(err));
            quote! { ::core::result::Result<#ok, #err> }
        }
        Kind::Plain(ty)
        | Kind::Signed(ty)
        | Kind::Float { ty, .. }
        | Kind::Named(ty)
        | Kind::Json(ty) => quote! { #ty },
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
    let facade = &analysis.options.facade();
    let place = |role: &str, ty: &Type, uncovered: Uncovered| {
        refused(&format!(
            "`{}::{}` {role} `{}`{}",
            service.ident,
            function.ident,
            spelled(ty),
            uncovered.described(ty)
        ))
    };
    let boundary = boundary(service.scope, analysis);
    let mut kinds = Vec::new();
    for (_, ty, _) in &function.params {
        kinds.push(
            argument(ty, service.scope, analysis)
                .map_err(|uncovered| place("takes", ty, uncovered))?,
        );
    }
    let types: Vec<&Type> = function.params.iter().map(|(_, ty, _)| ty).collect();
    let decode = decode_arguments(&kinds, &boundary, facade);
    let (result, encode) = match &function.result {
        Some((ty, _)) => {
            let kind = kind(ty, service.scope, analysis)
                .map_err(|uncovered| place("returns", ty, uncovered))?;
            let encode = encode(&kind, quote! { value }, &boundary, facade);
            (quote! { #ty }, quote! { |value| { #encode } })
        }
        None => (
            quote! { () },
            quote! { |()| ::core::result::Result::Ok(::#facade::serde_json::Value::Null) },
        ),
    };
    Ok(quote! {
        const #ident: ::#facade::Codec<(#(#types,)*), #result> = ::#facade::Codec {
            decode: |value| { #decode },
            encode: #encode,
        };
    })
}

/// A type as the author wrote it, for the errors: `Vec<Point>`.
pub(crate) fn spelled(ty: &Type) -> String {
    quote!(#ty).to_string().replace(' ', "").replace(',', ", ")
}

/// A WIT type as the codec reads and writes it, over its Rust type, in the JSON form of the
/// boundary: the author's type, borrowed, or the bindings' type of a consumed interface, spelled
/// from its WIT.
enum Kind<'a> {
    /// An unsigned integer, `bool`, `string` or `char`: serde's JSON, 64-bit integers as exact
    /// numbers.
    Plain(Cow<'a, Type>),
    /// A signed integer: serde's JSON, but `-0`, which the boundary refuses as an integer.
    Signed(Cow<'a, Type>),
    /// `f32` when `single`, else `f64`: a finite number other than `-0`, which an `f32` reads only
    /// when it narrows to such a number too, and writes as the `f64` it widens to, as a component
    /// writes it.
    Float { ty: Cow<'a, Type>, single: bool },
    /// `list<t>`: a JSON array, read into the list type.
    List(Cow<'a, Type>, Box<Kind<'a>>),
    /// `option<t>`: `null` or the value; `t` is no option.
    Option(Box<Kind<'a>>),
    /// `tuple<..>`: a JSON array of the elements.
    Tuple(Vec<Kind<'a>>),
    /// `result<ok, err>`: `{"tag": "ok" | "err", "val": ..}`, without `val` for a side `()`.
    Result(Option<Box<Kind<'a>>>, Option<Box<Kind<'a>>>),
    /// A record, an enum, a variant or a newtype of the module, or a record, a variant,
    /// an enum or a flags of a consumed interface: its own JSON, through the boundary trait it
    /// implements.
    Named(Cow<'a, Type>),
    /// Free JSON, `serde_json::Value`: a WIT `string` that holds its JSON text.
    Json(Cow<'a, Type>),
    /// A `#[wit_resource]` struct of the module, only returned: the value is issued into the
    /// call's result, whose data names it by its index there. A native caller holds it as its
    /// handle type, `handle`, the path the module that writes `ty` names it by.
    Resource { ty: &'a Type, handle: TokenStream },
}

impl<'a> Kind<'a> {
    /// The first resource the kind holds, itself or in a container, if any.
    fn resource(&self) -> Option<&'a Type> {
        match self {
            Kind::Resource { ty, .. } => Some(ty),
            Kind::List(_, inner) | Kind::Option(inner) => inner.resource(),
            Kind::Tuple(kinds) => kinds.iter().find_map(Kind::resource),
            Kind::Result(ok, err) => ok.iter().chain(err).find_map(|side| side.resource()),
            _ => None,
        }
    }
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
    if wit::free_json(ty) {
        return Ok(Kind::Json(Cow::Borrowed(ty)));
    }
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
                    Ok(Kind::Plain(Cow::Borrowed(ty)))
                }
                ("i8" | "i16" | "i32" | "i64", _) => Ok(Kind::Signed(Cow::Borrowed(ty))),
                ("f32", _) => Ok(Kind::Float {
                    ty: Cow::Borrowed(ty),
                    single: true,
                }),
                ("f64", _) => Ok(Kind::Float {
                    ty: Cow::Borrowed(ty),
                    single: false,
                }),
                ("Vec", [element]) => Ok(Kind::List(
                    Cow::Borrowed(ty),
                    Box::new(kind(element, scope, analysis)?),
                )),
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
                _ => named(ty, scope, analysis),
            }
        }
        _ => Err(uncovered(
            "a type without a WIT spelling; such types are not supported natively yet",
        )),
    }
}

/// The kind of a type a function takes, written in the module `scope`: one holding a resource is
/// refused, since natively a resource is only returned, or borrowed as a method's `self`.
fn argument<'a>(
    ty: &'a Type,
    scope: usize,
    analysis: &Analysis,
) -> Result<Kind<'a>, Uncovered<'a>> {
    let kind = kind(ty, scope, analysis)?;
    match kind.resource() {
        Some(resource) => Err(Uncovered {
            ty: resource,
            reason:
                "a resource; natively a resource is only returned, or borrowed as a method's `self`"
                    .to_owned(),
        }),
        None => Ok(kind),
    }
}

/// The kind of a type of the module or of a consumed interface: a type that enters the WIT from
/// the module, a newtype and a type of an interface the module implements read and write their
/// own JSON, and a resource is issued; the others are refused, saying what they are. A name of the
/// module is the type it resolves to (`Modules::module_type`).
fn named<'a>(ty: &'a Type, scope: usize, analysis: &Analysis) -> Result<Kind<'a>, Uncovered<'a>> {
    let declared = analysis.consumed_modules.module_type(ty, scope);
    let reason = if let Some(named) = analysis.consumed_modules.named(ty, scope) {
        let identity = &named.interface.identity;
        // A type of an interface the module implements or imports, by name or by key, or of a hook
        // it contributes to, is the bindings' type, which the codec reads from the interface's WIT.
        let defined = matches!(
            named.interface.role,
            Role::Implemented | Role::Imported | Role::Keyed | Role::Contributed
        )
        .then(|| analysis.consumed_types.named(identity, &named.ident))
        .flatten();
        match defined {
            Some(defined) => {
                let depth = analysis.modules[scope].len();
                let interface = match named.interface.role {
                    Role::Implemented => "the implemented interface",
                    Role::Contributed => "the hook",
                    _ => "the imported interface",
                };
                match wit_kind(defined, &analysis.consumed_types, depth) {
                    Ok(kind) => return Ok(kind),
                    Err(what) => format!(
                        "a type of {interface} {identity} that is or holds {what}, which the native side does not cover yet"
                    ),
                }
            }
            None => format!(
                "a type of the consumed interface {identity}; consumed types are not supported natively yet"
            ),
        }
    } else if declared.as_ref().is_some_and(|(ident, module)| {
        analysis
            .shapes
            .iter()
            .any(|shape| shape.ident == *ident && shape.scope == *module)
    }) || analysis.consumed_modules.newtype(ty, scope).is_some()
    {
        return Ok(Kind::Named(Cow::Borrowed(ty)));
    } else if let Some(resource) = declared.as_ref().and_then(|(ident, module)| {
        analysis
            .wit_resources
            .iter()
            .find(|resource| resource.ident == *ident && resource.scope == *module)
    }) {
        return Ok(Kind::Resource {
            ty,
            handle: handle_path(resource, scope, analysis),
        });
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
/// elements as there are parameters, or `null` for none, the JSON of the facade named `facade`.
fn decode_arguments(kinds: &[Kind<'_>], boundary: &TokenStream, facade: &Ident) -> TokenStream {
    let count = kinds.len();
    let names = elements(count);
    let decoded = kinds
        .iter()
        .zip(&names)
        .map(|(kind, name)| decode(kind, quote! { #name }, boundary, facade));
    let expected = match count {
        0 => "expected no arguments, got {}".to_owned(),
        1 => "expected 1 argument, got {}".to_owned(),
        count => format!("expected {count} arguments, got {{}}"),
    };
    quote! {
        let items = match value {
            ::#facade::serde_json::Value::Array(items) => items,
            ::#facade::serde_json::Value::Null => ::std::vec::Vec::new(),
            _ => {
                return ::core::result::Result::Err(
                    ::std::string::String::from("expected the arguments as a list")
                );
            }
        };
        let [#(#names),*] = <[::#facade::serde_json::Value; #count]>::try_from(items)
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
/// of the module. A resource, which only a native caller reads, is its handle type in `T`, taken
/// from the owned handles of the result, `__tembra_owned`, which must then be in scope. The JSON is
/// that of the facade named `facade`.
fn decode(
    kind: &Kind<'_>,
    value: TokenStream,
    boundary: &TokenStream,
    facade: &Ident,
) -> TokenStream {
    match kind {
        Kind::Plain(ty) => quote! {
            ::#facade::serde_json::from_value::<#ty>(#value).map_err(|error| error.to_string())
        },
        Kind::Signed(ty) => quote! {
            match #value {
                ::#facade::serde_json::Value::Number(number)
                    if number.as_f64().is_some_and(|wide| wide == 0.0 && wide.is_sign_negative()) =>
                {
                    ::core::result::Result::Err(::std::string::String::from(
                        "expected an integer, got -0",
                    ))
                }
                item => ::#facade::serde_json::from_value::<#ty>(item).map_err(|error| error.to_string()),
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
                    ::#facade::serde_json::Value::Number(number) => match number.as_f64() {
                        ::core::option::Option::Some(wide) if #wide_carried => #read,
                        _ => ::core::result::Result::Err(::std::format!(#expected, number)),
                    },
                    item => ::#facade::serde_json::from_value::<#ty>(item).map_err(|error| error.to_string()),
                }
            }
        }
        Kind::List(ty, element) => {
            // A list of resources is read into a list of their handles, which its elements name.
            let list = match element.resource() {
                Some(_) => quote! { ::std::vec::Vec<_> },
                None => quote! { #ty },
            };
            let element = decode(element, quote! { item }, boundary, facade);
            quote! {
                match #value {
                    ::#facade::serde_json::Value::Array(items) => items
                        .into_iter()
                        .map(|item| #element)
                        .collect::<::core::result::Result<#list, ::std::string::String>>(),
                    _ => ::core::result::Result::Err(::std::string::String::from("expected a list")),
                }
            }
        }
        Kind::Option(inner) => {
            let inner = decode(inner, quote! { item }, boundary, facade);
            quote! {
                match #value {
                    ::#facade::serde_json::Value::Null => {
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
                .map(|(kind, name)| decode(kind, quote! { #name }, boundary, facade));
            let expected = format!("expected a tuple of {count} elements, got {{}}");
            quote! {
                match #value {
                    ::#facade::serde_json::Value::Array(items) => {
                        match <[::#facade::serde_json::Value; #count]>::try_from(items) {
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
                    let read = decode(kind, quote! { item }, boundary, facade);
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
                facade,
            )
        }
        Kind::Named(ty) => quote! { <#ty as #boundary>::decode(#value) },
        Kind::Json(ty) => quote! {
            match #value {
                ::#facade::serde_json::Value::String(text) => {
                    ::#facade::serde_json::from_str::<#ty>(&text)
                        .map_err(|error| ::std::format!("expected JSON text: {}", error))
                }
                _ => ::core::result::Result::Err(::std::string::String::from(
                    "expected JSON text as a string",
                )),
            }
        },
        // Only a native caller reads a resource, from the result it received: the value is the
        // index of one of the result's owned handles, `__tembra_owned`, each taken once, which
        // it holds as the resource's handle type.
        Kind::Resource { handle, .. } => quote! {
            match #value
                .as_u64()
                .and_then(|index| ::core::convert::TryFrom::try_from(index).ok())
                .and_then(|index: usize| __tembra_owned.get_mut(index))
                .and_then(::core::option::Option::take)
            {
                ::core::option::Option::Some(owned) => {
                    <#handle as ::core::convert::TryFrom<::#facade::portable::Owned>>::try_from(owned)
                        .map_err(|_| ::std::string::String::from(
                            "the result holds a resource of another type",
                        ))
                }
                ::core::option::Option::None => ::core::result::Result::Err(::std::string::String::from(
                    "expected the index of a resource the result holds, once",
                )),
            }
        },
    }
}

/// Write a value of a kind, `value`, of the author's type, as its JSON: an expression of
/// `Result<Value, String>`, which the boundary trait, as `boundary` names it there, writes for a
/// type of the module. The JSON is that of the facade named `facade`.
fn encode(
    kind: &Kind<'_>,
    value: TokenStream,
    boundary: &TokenStream,
    facade: &Ident,
) -> TokenStream {
    match kind {
        Kind::Plain(_) | Kind::Signed(_) => quote! {
            ::#facade::serde_json::to_value(#value).map_err(|error| error.to_string())
        },
        Kind::Float { .. } => {
            let wide_carried = carried(quote! { wide });
            quote! {
                {
                    let wide = ::core::convert::Into::<f64>::into(#value);
                    match ::#facade::serde_json::Number::from_f64(wide) {
                        ::core::option::Option::Some(number) if #wide_carried => {
                            ::core::result::Result::Ok(::#facade::serde_json::Value::Number(number))
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
            let element = encode(element, quote! { item }, boundary, facade);
            quote! {
                #value
                    .into_iter()
                    .map(|item| #element)
                    .collect::<::core::result::Result<
                        ::std::vec::Vec<::#facade::serde_json::Value>,
                        ::std::string::String
                    >>()
                    .map(::#facade::serde_json::Value::Array)
            }
        }
        Kind::Option(inner) => {
            let inner = encode(inner, quote! { item }, boundary, facade);
            quote! {
                match #value {
                    ::core::option::Option::None => {
                        ::core::result::Result::Ok(::#facade::serde_json::Value::Null)
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
                .map(|(kind, name)| encode(kind, quote! { #name }, boundary, facade));
            let written = written(facade);
            quote! {
                {
                    let (#(#names,)*) = #value;
                    #written(::#facade::serde_json::Value::Array(::std::vec![#(#encoded?),*]))
                }
            }
        }
        Kind::Result(ok, err) => {
            let side = |side: &Option<Box<Kind<'_>>>, tag: &str, wrap: TokenStream| match side {
                Some(kind) => {
                    let written = encode(kind, quote! { item }, boundary, facade);
                    let tagged = tagged_encode(tag, Some(quote! { #written? }), facade);
                    quote! { #wrap(item) => #tagged }
                }
                None => {
                    let tagged = tagged_encode(tag, None, facade);
                    quote! { #wrap(()) => #tagged }
                }
            };
            let ok = side(ok, "ok", quote! { ::core::result::Result::Ok });
            let err = side(err, "err", quote! { ::core::result::Result::Err });
            quote! { match #value { #ok, #err } }
        }
        Kind::Named(ty) => quote! { <#ty as #boundary>::encode(#value) },
        Kind::Json(_) => quote! {
            ::#facade::serde_json::to_string(&#value)
                .map(::#facade::serde_json::Value::String)
                .map_err(|error| error.to_string())
        },
        Kind::Resource { .. } => {
            let issued = issued(value, facade);
            let written = written(facade);
            quote! { #written(#issued) }
        }
    }
}

/// The call's result while it is written: the payload a resource of the value is issued into.
fn result_payload() -> Ident {
    format_ident!("__tembra_result")
}

/// Issue `value`, a resource, into the call's result: the JSON of its index there, which names it
/// in the result's data, through the facade named `facade`.
fn issued(value: TokenStream, facade: &Ident) -> TokenStream {
    let result = result_payload();
    quote! {
        ::#facade::serde_json::Value::from(
            <u32 as ::core::convert::From<::#facade::portable::Slot>>::from(#result.issue(#value))
        )
    }
}

/// `Ok` of what writing a value evaluates to, `Result<Value, String>`, spelled whole: `?` on an
/// expression that never fails needs its error type. The `Value` is that of the facade named
/// `facade`.
fn written(facade: &Ident) -> TokenStream {
    quote! { ::core::result::Result::<::#facade::serde_json::Value, ::std::string::String>::Ok }
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
/// names the type for the errors, "a result". An expression of `Result<T, String>`, over the JSON
/// of the facade named `facade`.
fn tagged_decode(
    value: TokenStream,
    what: &str,
    cases: Vec<(String, Read)>,
    facade: &Ident,
) -> TokenStream {
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
            ::#facade::serde_json::Value::Object(mut object) => {
                let tag = object.remove("tag");
                let payload = object.remove("val");
                match (tag, payload, object.keys().next()) {
                    (_, _, ::core::option::Option::Some(key)) => ::core::result::Result::Err(
                        ::std::format!("unexpected field `{}`", key)
                    ),
                    (
                        ::core::option::Option::Some(::#facade::serde_json::Value::String(tag)),
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
/// `Value`, the JSON of the facade named `facade`.
fn tagged_encode(tag: &str, payload: Option<TokenStream>, facade: &Ident) -> TokenStream {
    let payload = payload.map(|payload| {
        quote! { (::std::string::String::from("val"), #payload), }
    });
    let written = written(facade);
    quote! {
        #written(::#facade::serde_json::Value::Object(
            ::core::iter::FromIterator::from_iter([
                (
                    ::std::string::String::from("tag"),
                    ::#facade::serde_json::Value::String(::std::string::String::from(#tag)),
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
/// function returning that type, which a member the object misses returns from. The JSON is that
/// of the facade named `facade`.
fn record_decode(
    value: TokenStream,
    built: TokenStream,
    members: &[Member<'_>],
    boundary: &TokenStream,
    facade: &Ident,
) -> TokenStream {
    let fields = members.iter().map(|Member { field, key, kind }| {
        let missing = format!("missing the field `{key}`");
        let read = decode(kind, quote! { item }, boundary, facade);
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
            ::#facade::serde_json::Value::Object(mut object) => {
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
/// keys: an expression of `Result<Value, String>`, the JSON of the facade named `facade`.
fn record_encode(
    bound: &[Ident],
    members: &[Member<'_>],
    boundary: &TokenStream,
    facade: &Ident,
) -> TokenStream {
    let entries = bound.iter().zip(members).map(|(name, member)| {
        let key = &member.key;
        let written = encode(&member.kind, quote! { #name }, boundary, facade);
        quote! { (::std::string::String::from(#key), #written?), }
    });
    let written = written(facade);
    quote! {
        #written(::#facade::serde_json::Value::Object(
            ::core::iter::FromIterator::from_iter([#(#entries)*])
        ))
    }
}

/// The kind of a member of the type of the module `shape`, written in its module, or its refusal,
/// after `place`, which names the member: "the field `count` of `Entry` is". A member holding a
/// resource is refused, pointing to a tuple, which carries the resource natively too: a record or
/// a variant holding one is component-only, since only a component's conversions wrap the
/// resource into the bindings' handle and take it back.
fn member<'a>(
    ty: &'a Type,
    place: &str,
    shape: &Shape,
    analysis: &Analysis,
) -> Result<Kind<'a>, Refused> {
    let refusal = |uncovered: Uncovered| {
        refused(&format!(
            "{place} `{}`{}",
            spelled(ty),
            uncovered.described(ty)
        ))
    };
    let kind = kind(ty, shape.scope, analysis).map_err(refusal)?;
    match kind.resource() {
        Some(resource) => Err(refusal(Uncovered {
            ty: resource,
            reason: format!(
                "a resource; a record or a variant holding a resource is component-only, since the native boundary carries a resource beside the data, as a handle, never inside the JSON of a record: to return the resource natively too, return it in a tuple beside the data, such as `-> ({}, String)`",
                spelled(resource)
            ),
        })),
        None => Ok(kind),
    }
}

/// The members of a record of `shape`, or of the record of one of its cases, `owner` for the
/// errors: each in the Rust field the author names, keyed by the lowerCamelCase form of its WIT
/// name.
fn members<'a>(
    fields: &'a [(Ident, Type)],
    owner: &str,
    shape: &Shape,
    analysis: &Analysis,
) -> Result<Vec<Member<'a>>, Refused> {
    fields
        .iter()
        .map(|(field, ty)| {
            let name = wit::kebab(field);
            Ok(Member {
                field: field.clone(),
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

/// The JSON of a newtype the WIT spells as its field, as its impl of the boundary trait, in its own
/// module, where the field and its type are named as the author wrote them: the field's JSON, read
/// into the newtype through `TryFrom` of the field's type, which a `From` implements too, whose
/// refusal fails the call with its message, and written from the field. A field the codec does not
/// cover is refused, naming it.
fn newtype_codec(newtype: &Newtype, analysis: &Analysis) -> Result<TokenStream, Refused> {
    let Newtype {
        ident,
        inner,
        scope,
        ..
    } = newtype;
    let facade = &analysis.options.facade();
    let boundary = boundary(*scope, analysis);
    let kind = kind(inner, *scope, analysis).map_err(|uncovered| {
        refused(&format!(
            "the field of `{ident}` is `{}`{}",
            spelled(inner),
            uncovered.described(inner)
        ))
    })?;
    let read = decode(&kind, quote! { value }, &boundary, facade);
    let written = encode(&kind, quote! { self.0 }, &boundary, facade);
    let decode = quote! {
        <Self as ::core::convert::TryFrom<#inner>>::try_from(#read?)
            .map_err(|error| ::std::string::ToString::to_string(&error))
    };
    Ok(boundary_impl(
        &boundary,
        quote! { #ident },
        decode,
        written,
        facade,
    ))
}

/// The JSON of a type of the module that enters the WIT, as its impl of the boundary trait, in its
/// own module, where its members are visible: a record as an object of its fields, keyed by the
/// lowerCamelCase form of their WIT names; an enum as the WIT name of its case; a variant as an
/// object of its case's `tag` and payload, `val`, the record of a case with named fields as an
/// object too. Every type stays the author's, so the members and cases are named as the author
/// wrote them. A member the codec does not cover is refused, naming it.
fn shape_codec(shape: &Shape, analysis: &Analysis) -> Result<TokenStream, Refused> {
    let facade = &analysis.options.facade();
    let ident = &shape.ident;
    let boundary = boundary(shape.scope, analysis);
    let member = |ty, place: String| member(ty, &place, shape, analysis);
    let members = |fields, owner: String| members(fields, &owner, shape, analysis);
    let (decode, encode) = match &shape.kind {
        ShapeKind::Record(fields) => {
            let members = members(fields, ident.to_string())?;
            let bound = elements(members.len());
            let fields = members.iter().map(|member| &member.field);
            let decode = record_decode(
                quote! { value },
                quote! { Self },
                &members,
                &boundary,
                facade,
            );
            let encode = record_encode(&bound, &members, &boundary, facade);
            (
                decode,
                quote! { let Self { #(#fields: #bound),* } = self; #encode },
            )
        }
        ShapeKind::Enum(cases) => {
            let tags: Vec<String> = cases.iter().map(wit::kebab).collect();
            enum_codec(cases, &tags, facade)
        }
        ShapeKind::Variant(cases) => {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            for (case, payload) in cases {
                let tag = wit::kebab(case);
                let named = case;
                match payload {
                    Payload::None => {
                        let (read, written) = variant_case(&tag, named, None, &boundary, facade);
                        reads.push(read);
                        writes.push(written);
                    }
                    Payload::Unnamed(ty) => {
                        let kind = member(ty, format!("the case `{ident}::{case}` carries"))?;
                        let (read, written) =
                            variant_case(&tag, named, Some(&kind), &boundary, facade);
                        reads.push(read);
                        writes.push(written);
                    }
                    Payload::Named(fields) => {
                        let members = members(fields, format!("{ident}::{case}"))?;
                        let read = record_decode(
                            quote! { item },
                            quote! { Self::#named },
                            &members,
                            &boundary,
                            facade,
                        );
                        reads.push((tag.clone(), Read::Carried(read)));
                        let bound = elements(members.len());
                        let fields = members.iter().map(|member| &member.field);
                        let record = record_encode(&bound, &members, &boundary, facade);
                        let written = tagged_encode(&tag, Some(quote! { #record? }), facade);
                        writes.push(quote! { Self::#named { #(#fields: #bound),* } => #written });
                    }
                }
            }
            let decode = tagged_decode(quote! { value }, "a variant", reads, facade);
            (decode, quote! { match self { #(#writes,)* } })
        }
    };
    Ok(boundary_impl(
        &boundary,
        quote! { #ident },
        decode,
        encode,
        facade,
    ))
}

/// The impl of the boundary trait, as `boundary` names it, for `target`, with `decode`, which
/// reads `value`, and `encode`, which writes `self`, the JSON of the facade named `facade`.
fn boundary_impl(
    boundary: &TokenStream,
    target: TokenStream,
    decode: TokenStream,
    encode: TokenStream,
    facade: &Ident,
) -> TokenStream {
    quote! {
        impl #boundary for #target {
            fn decode(
                value: ::#facade::serde_json::Value,
            ) -> ::core::result::Result<Self, ::std::string::String> {
                #decode
            }

            fn encode(
                self,
            ) -> ::core::result::Result<::#facade::serde_json::Value, ::std::string::String> {
                #encode
            }
        }
    }
}

/// The JSON of an enum whose Rust cases are `cases`, the author's for a type of the module and the
/// bindings' UpperCamelCase for a consumed type, with their WIT names `tags`: the WIT name of its
/// case, read from `value` and written from `self`, the JSON of the facade named `facade`.
fn enum_codec(cases: &[Ident], tags: &[String], facade: &Ident) -> (TokenStream, TokenStream) {
    let decode = quote! {
        match value {
            ::#facade::serde_json::Value::String(case) => match case.as_str() {
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
        ::core::result::Result::Ok(::#facade::serde_json::Value::String(
            ::std::string::String::from(match self { #(Self::#cases => #tags,)* })
        ))
    };
    (decode, encode)
}

/// How the case `named` of a variant, `tag` in the WIT, crosses: the case read for
/// `tagged_decode`, and the arm writing it, with its payload of the kind given, if it carries one,
/// in the JSON of the facade named `facade`.
fn variant_case(
    tag: &str,
    named: &Ident,
    payload: Option<&Kind<'_>>,
    boundary: &TokenStream,
    facade: &Ident,
) -> ((String, Read), TokenStream) {
    match payload {
        None => {
            let read = quote! { ::core::result::Result::Ok(Self::#named) };
            let written = tagged_encode(tag, None, facade);
            (
                (tag.to_owned(), Read::Bare(read)),
                quote! { Self::#named => #written },
            )
        }
        Some(kind) => {
            let read = decode(kind, quote! { item }, boundary, facade);
            let written = encode(kind, quote! { item }, boundary, facade);
            let written = tagged_encode(tag, Some(quote! { #written? }), facade);
            (
                (
                    tag.to_owned(),
                    Read::Carried(quote! { #read.map(Self::#named) }),
                ),
                quote! { Self::#named(item) => #written },
            )
        }
    }
}

/// The JSON of a record, a variant, an enum or a flags of a consumed interface, by its id in
/// `types`, as its bindings' type's impl of the boundary trait, in the Orb module, written from
/// its WIT as the codec writes the module's own types: a record as an object of its fields, keyed
/// by the lowerCamelCase form of their WIT names; an enum as the WIT name of its case; a variant as
/// an object of its case's `tag` and payload, `val`; a flags, which the bindings generate with
/// `bitflags`, as an object of the flags set, each `true`, which reads a flag left out as unset,
/// through `bits()` and `from_bits_retain()` alone, which both the `bitflags` crate and
/// `wit-bindgen`'s own `bitflags!` define, as the facade's `wit-bindgen` lacks its feature
/// `bitflags`. The members are named as the bindings generate them. None for a type holding what
/// the codec does not cover, which a method that takes or returns it refuses. The JSON is that of
/// the facade named `facade`.
fn consumed_codec(id: TypeId, types: &wit::ConsumedTypes, facade: &Ident) -> Option<TokenStream> {
    let boundary = format_ident!("{BOUNDARY}");
    let boundary = quote! { #boundary };
    let kind = |ty: wit_parser::Type| wit_kind(ty, types, 0).ok();
    let Ok((_, definition)) = types.definition(wit_parser::Type::Id(id)) else {
        return None;
    };
    let (decode, encode) = match definition {
        TypeDefKind::Record(record) => {
            let members = record
                .fields
                .iter()
                .map(|field| {
                    Some(Member {
                        field: rust_ident(&field.name),
                        key: javascript_name(&field.name),
                        kind: kind(field.ty)?,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            let bound = elements(members.len());
            let fields = members.iter().map(|member| &member.field);
            let decode = record_decode(
                quote! { value },
                quote! { Self },
                &members,
                &boundary,
                facade,
            );
            let encode = record_encode(&bound, &members, &boundary, facade);
            (
                decode,
                quote! { let Self { #(#fields: #bound),* } = self; #encode },
            )
        }
        TypeDefKind::Enum(enumeration) => {
            let tags: Vec<String> = enumeration
                .cases
                .iter()
                .map(|case| case.name.clone())
                .collect();
            let cases: Vec<Ident> = tags.iter().map(|tag| camel_ident(tag)).collect();
            enum_codec(&cases, &tags, facade)
        }
        TypeDefKind::Variant(variant) => {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            for case in &variant.cases {
                let payload = match case.ty {
                    Some(ty) => Some(kind(ty)?),
                    None => None,
                };
                let named = camel_ident(&case.name);
                let (read, written) =
                    variant_case(&case.name, &named, payload.as_ref(), &boundary, facade);
                reads.push(read);
                writes.push(written);
            }
            let decode = tagged_decode(quote! { value }, "a variant", reads, facade);
            (decode, quote! { match self { #(#writes,)* } })
        }
        TypeDefKind::Flags(flags) => {
            let names: Vec<Ident> = flags
                .flags
                .iter()
                .map(|flag| format_ident!("{}", flag.name.to_shouty_snake_case()))
                .collect();
            let keys: Vec<String> = flags
                .flags
                .iter()
                .map(|flag| javascript_name(&flag.name))
                .collect();
            let decode = quote! {
                match value {
                    ::#facade::serde_json::Value::Object(object) => {
                        let mut flags = Self::from_bits_retain(0);
                        for (key, set) in object {
                            let flag = match key.as_str() {
                                #(#keys => Self::#names,)*
                                _ => {
                                    return ::core::result::Result::Err(
                                        ::std::format!("unknown flag `{}`", key)
                                    );
                                }
                            };
                            match set {
                                ::#facade::serde_json::Value::Bool(true) => {
                                    flags = Self::from_bits_retain(flags.bits() | flag.bits());
                                }
                                ::#facade::serde_json::Value::Bool(false) => {
                                    flags = Self::from_bits_retain(flags.bits() & !flag.bits());
                                }
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
                let mut flags = ::#facade::serde_json::Map::new();
                #(
                    if self.bits() & Self::#names.bits() == Self::#names.bits() {
                        flags.insert(
                            ::std::string::String::from(#keys),
                            ::#facade::serde_json::Value::Bool(true),
                        );
                    }
                )*
                ::core::result::Result::Ok(::#facade::serde_json::Value::Object(flags))
            };
            (decode, encode)
        }
        _ => return None,
    };
    Some(boundary_impl(
        &boundary,
        types.path(id, 0),
        decode,
        encode,
        facade,
    ))
}

/// The kind of a WIT type of a consumed interface, as its bindings generate it, read in a module
/// `depth` levels below the Orb module: a record, a variant, an enum or a flags through the
/// boundary trait its bindings' type implements, by its path from there, which only one the codec
/// covers whole implements; anything else by its structure, a list read into a `Vec`. Or what the
/// type is or holds that the codec does not cover.
fn wit_kind(
    ty: wit_parser::Type,
    types: &wit::ConsumedTypes,
    depth: usize,
) -> Result<Kind<'static>, &'static str> {
    use wit_parser::Type as Wit;
    let owned = |ty: TokenStream| Cow::Owned(syn::parse2::<Type>(ty).expect("a Rust type"));
    let (id, definition) = match types.definition(ty) {
        Ok(defined) => defined,
        Err(primitive) => {
            return Ok(match primitive {
                Wit::Bool => Kind::Plain(owned(quote! { bool })),
                Wit::U8 => Kind::Plain(owned(quote! { u8 })),
                Wit::U16 => Kind::Plain(owned(quote! { u16 })),
                Wit::U32 => Kind::Plain(owned(quote! { u32 })),
                Wit::U64 => Kind::Plain(owned(quote! { u64 })),
                Wit::Char => Kind::Plain(owned(quote! { char })),
                Wit::String => Kind::Plain(owned(quote! { ::std::string::String })),
                Wit::S8 => Kind::Signed(owned(quote! { i8 })),
                Wit::S16 => Kind::Signed(owned(quote! { i16 })),
                Wit::S32 => Kind::Signed(owned(quote! { i32 })),
                Wit::S64 => Kind::Signed(owned(quote! { i64 })),
                Wit::F32 => Kind::Float {
                    ty: owned(quote! { f32 }),
                    single: true,
                },
                Wit::F64 => Kind::Float {
                    ty: owned(quote! { f64 }),
                    single: false,
                },
                Wit::ErrorContext => return Err("an error context"),
                Wit::Id(_) => unreachable!("a type through its aliases is a definition"),
            });
        }
    };
    let kind = |ty: Wit| wit_kind(ty, types, depth);
    let side = |ty: Option<Wit>| ty.map(|ty| kind(ty).map(Box::new)).transpose();
    match definition {
        TypeDefKind::Record(record) => {
            for field in &record.fields {
                kind(field.ty)?;
            }
            Ok(Kind::Named(owned(types.path(id, depth))))
        }
        TypeDefKind::Variant(variant) => {
            for payload in variant.cases.iter().filter_map(|case| case.ty) {
                kind(payload)?;
            }
            Ok(Kind::Named(owned(types.path(id, depth))))
        }
        TypeDefKind::Enum(_) | TypeDefKind::Flags(_) => {
            Ok(Kind::Named(owned(types.path(id, depth))))
        }
        TypeDefKind::Option(inner) => match kind(*inner)? {
            Kind::Option(_) => Err("a nested option, which the boundary's JSON cannot carry"),
            inner => Ok(Kind::Option(Box::new(inner))),
        },
        TypeDefKind::List(inner) => Ok(Kind::List(
            owned(quote! { ::std::vec::Vec<_> }),
            Box::new(kind(*inner)?),
        )),
        TypeDefKind::Tuple(tuple) => Ok(Kind::Tuple(
            tuple
                .types
                .iter()
                .map(|ty| kind(*ty))
                .collect::<Result<_, _>>()?,
        )),
        TypeDefKind::Result(result) => Ok(Kind::Result(side(result.ok)?, side(result.err)?)),
        TypeDefKind::Resource | TypeDefKind::Handle(_) => Err("a resource"),
        TypeDefKind::Future(_) => Err("a future"),
        TypeDefKind::Stream(_) => Err("a stream"),
        TypeDefKind::Map(..) => Err("a map"),
        TypeDefKind::FixedLengthList(..) => Err("a list of a fixed length"),
        TypeDefKind::Type(_) | TypeDefKind::Unknown => Err("a type the macro does not know"),
    }
}
